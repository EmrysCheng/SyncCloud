//! 引擎端到端测试：本地目录模拟远端存储，双设备（双引擎实例）验证
//! 上传 → 下载 → 双向传播 → Dropbox 式冲突副本 → 删除传播 全链路。

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use synccloud_lib::config::{BackendConfig, FolderConfig};
use synccloud_lib::storage::{RemoteEntry, RemoteStore, RawIo};
use synccloud_lib::sync::engine::{EventSink, SyncEngine};
use synccloud_lib::sync::state::StateDb;
use synccloud_lib::sync::types::{ConflictPolicy, Direction, Manifest, SyncMode};

/// 本地文件系统模拟的远端存储（镜像布局 + 同步目录外的元数据区）。
struct LocalFsStore {
    root: PathBuf,
    /// 元数据区：同步目录的兄弟目录（模拟新版布局）
    meta: PathBuf,
}

impl LocalFsStore {
    fn new(root: &Path) -> Self {
        let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("remote");
        Self {
            meta: root.with_file_name(format!("{name}.synccloud-meta")),
            root: root.to_path_buf(),
        }
    }
    fn path_of(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for seg in rel.split('/') {
            p.push(seg);
        }
        p
    }
    fn meta_path_of(&self, key: &str) -> PathBuf {
        let mut p = self.meta.clone();
        for seg in key.split('/') {
            p.push(seg);
        }
        p
    }
}

#[async_trait]
impl RemoteStore for LocalFsStore {
    fn kind(&self) -> &'static str {
        "localfs"
    }
    fn raw_io(&self) -> &dyn RawIo {
        self
    }
    async fn test(&self) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(&self.meta).await?;
        Ok(())
    }
    async fn read_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        let p = self.meta_path_of("manifest.json");
        match tokio::fs::read_to_string(&p).await {
            Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            Err(_) => Ok(None),
        }
    }
    async fn write_manifest(&self, manifest: &Manifest) -> anyhow::Result<()> {
        let p = self.meta_path_of("manifest.json");
        tokio::fs::create_dir_all(p.parent().unwrap()).await?;
        tokio::fs::write(&p, serde_json::to_vec_pretty(manifest)?).await?;
        Ok(())
    }
    async fn read_legacy_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        let p = self.path_of(".synccloud/manifest.json");
        match tokio::fs::read_to_string(&p).await {
            Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            Err(_) => Ok(None),
        }
    }
    async fn cleanup_legacy_meta(&self) -> anyhow::Result<()> {
        let _ = tokio::fs::remove_dir_all(self.path_of(".synccloud")).await;
        Ok(())
    }
    async fn put_file(&self, path: &str, src: &Path) -> anyhow::Result<()> {
        let dest = self.path_of(path);
        tokio::fs::create_dir_all(dest.parent().unwrap()).await?;
        tokio::fs::copy(src, &dest).await?;
        Ok(())
    }
    async fn get_file(&self, path: &str, dest: &Path) -> anyhow::Result<()> {
        let src = self.path_of(path);
        tokio::fs::create_dir_all(dest.parent().unwrap()).await?;
        tokio::fs::copy(&src, dest).await?;
        Ok(())
    }
    async fn delete_file(&self, path: &str) -> anyhow::Result<()> {
        match tokio::fs::remove_file(self.path_of(path)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    async fn list_objects(&self) -> anyhow::Result<Vec<RemoteEntry>> {
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let mut it = tokio::fs::read_dir(&dir).await?;
            while let Some(entry) = it.next_entry().await? {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(rel) = p.strip_prefix(&self.root) {
                    let key = rel.to_string_lossy().replace('\\', "/");
                    if !key.starts_with(".synccloud/") {
                        out.push(RemoteEntry { key, size: entry.metadata().await?.len() });
                    }
                }
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl RawIo for LocalFsStore {
    async fn put_text(&self, key: &str, content: &str) -> anyhow::Result<()> {
        let p = self.meta_path_of(key);
        tokio::fs::create_dir_all(p.parent().unwrap()).await?;
        tokio::fs::write(&p, content).await?;
        Ok(())
    }
    async fn read_text(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(tokio::fs::read_to_string(self.meta_path_of(key)).await.ok())
    }
    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let _ = tokio::fs::remove_file(self.meta_path_of(key)).await;
        Ok(())
    }
    async fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let dir = self.meta_path_of(prefix.trim_end_matches('/'));
        let mut out = Vec::new();
        if let Ok(mut it) = tokio::fs::read_dir(&dir).await {
            while let Some(e) = it.next_entry().await? {
                let name = e.file_name().to_string_lossy().to_string();
                if !e.path().is_dir() {
                    out.push(format!("{}/{}", prefix.trim_end_matches('/'), name));
                }
            }
        }
        Ok(out)
    }
}

struct Device {
    engine: SyncEngine,
}

fn make_device(local: &Path, state_db: &Path, name: &str) -> Device {
    let folder = FolderConfig {
        id: Uuid::new_v4(),
        name: format!("test-{name}"),
        local_path: local.to_string_lossy().to_string(),
        backend: BackendConfig::WebDav {
            url: "http://test.local".into(),
            username: String::new(),
            password: String::new(),
            root_path: String::new(),
        },
        direction: Direction::Bidirectional,
        conflict_policy: ConflictPolicy::BothCopies,
        ignore_rules: vec![],
        sync_mode: SyncMode::Manual,
        interval_secs: 3600,
        encryption: Default::default(),
        transfer_priority: Default::default(),
        max_file_size_mb: None,
        ignore_hidden: false,
        enabled: true,
    };
    std::fs::create_dir_all(local).unwrap();
    let db = StateDb::open(state_db).unwrap();
    let (sink, _rx) = EventSink::new();
    Device {
        engine: SyncEngine {
            folder,
            device_id: format!("device-{name}"),
            device_name: format!("Machine{name}"),
            settings: Default::default(),
            db,
            events: sink,
            paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    }
}

/// 等待文件 mtime 精度窗口过去，避免哈希缓存 (size,mtime) 误命中。
async fn settle() {
    tokio::time::sleep(Duration::from_millis(25)).await;
}

/// 断言目录内没有任何引擎产物（.synccloud* 目录/文件、*.synccloud-part 临时文件）。
async fn assert_dir_clean(dir: &Path) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut it = match tokio::fs::read_dir(&d).await {
            Ok(it) => it,
            Err(_) => continue,
        };
        while let Some(e) = it.next_entry().await.unwrap() {
            let name = e.file_name().to_string_lossy().to_string();
            let p = e.path();
            let display = p.display().to_string();
            if p.is_dir() {
                stack.push(p);
            }
            assert!(
                !name.starts_with(".synccloud") && !name.ends_with(".synccloud-part"),
                "同步目录内出现了引擎文件: {}",
                display
            );
        }
    }
}

#[tokio::test]
async fn full_bidirectional_sync_with_conflict() {
    let tmp = std::env::temp_dir().join(format!("synccloud-e2e-{}", Uuid::new_v4()));
    let _ = std::fs::remove_dir_all(&tmp);
    let remote_dir = tmp.join("remote");
    let dir_a = tmp.join("machineA");
    let dir_b = tmp.join("machineB");
    std::fs::create_dir_all(&remote_dir).unwrap();
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    let store = LocalFsStore::new(&remote_dir);
    let mut dev_a = make_device(&dir_a, &tmp.join("a.db"), "A");
    let mut dev_b = make_device(&dir_b, &tmp.join("b.db"), "B");

    // ---- 1. 设备 A 新增文件 → 上传远端 ----
    tokio::fs::write(dir_a.join("hello.txt"), b"hello v1").await.unwrap();
    let r = dev_a.engine.run_cycle(&store).await;
    assert_eq!(r.uploaded, 1);
    assert!(remote_dir.join("hello.txt").exists(), "远端应已有 hello.txt");
    let manifest = store.read_manifest().await.unwrap().unwrap();
    assert!(manifest.files.contains_key("hello.txt"));

    // ---- 2. 设备 B 首次同步 → 下载 ----
    let r = dev_b.engine.run_cycle(&store).await;
    assert_eq!(r.downloaded, 1);
    assert_eq!(tokio::fs::read(dir_b.join("hello.txt")).await.unwrap(), b"hello v1");
    // 同步目录内不得出现任何引擎文件（本地下载临时件、远端元数据目录）
    assert_dir_clean(&remote_dir).await;
    assert_dir_clean(&dir_a).await;
    assert_dir_clean(&dir_b).await;

    // ---- 3. 设备 A 修改 → B 跟随更新 ----
    settle().await;
    tokio::fs::write(dir_a.join("hello.txt"), b"hello v2 - modified by A").await.unwrap();
    dev_a.engine.run_cycle(&store).await;
    dev_b.engine.run_cycle(&store).await;
    assert_eq!(
        tokio::fs::read(dir_b.join("hello.txt")).await.unwrap(),
        b"hello v2 - modified by A",
        "B 应收到 A 的修改"
    );

    // ---- 4. 冲突：双方同时修改同一文件 ----
    settle().await;
    tokio::fs::write(dir_a.join("hello.txt"), "A 的最终修改".as_bytes()).await.unwrap();
    settle().await;
    tokio::fs::write(dir_b.join("hello.txt"), "B 的最终修改".as_bytes()).await.unwrap();
    dev_a.engine.run_cycle(&store).await; // A 推送自己的修改
    let r = dev_b.engine.run_cycle(&store).await; // B 检测到冲突
    assert_eq!(r.conflicts, 1, "B 应产生一个冲突");
    // Dropbox 语义：正式文件 = 远端（A 的）版本；B 的修改保留为冲突副本
    assert_eq!(tokio::fs::read(dir_b.join("hello.txt")).await.unwrap(), "A 的最终修改".as_bytes());
    let mut entries = tokio::fs::read_dir(&dir_b).await.unwrap();
    let mut conflict_copy: Option<String> = None;
    while let Some(e) = entries.next_entry().await.unwrap() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.contains("冲突副本") && name.contains("MachineB") && name.ends_with(".txt") {
            conflict_copy = Some(name);
        }
    }
    let copy_name = conflict_copy.expect("应存在 B 的冲突副本");
    assert_eq!(
        tokio::fs::read(dir_b.join(&copy_name)).await.unwrap(),
        "B 的最终修改".as_bytes(),
        "冲突副本应保留 B 的内容"
    );

    // 冲突记录入库
    let conflicts = dev_b.engine.db.list_conflicts(false).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].path, "hello.txt");
    assert!(conflicts[0].conflict_copy.as_deref().unwrap().contains("冲突副本"));

    // ---- 5. 冲突副本同步给 A（新文件传播）----
    let r = dev_a.engine.run_cycle(&store).await;
    assert_eq!(r.downloaded, 1, "A 应收到冲突副本");
    assert!(tokio::fs::read(dir_a.join(&copy_name)).await.is_ok());

    // ---- 6. 删除传播：A 删除 → 远端删 → B 跟随删 ----
    tokio::fs::remove_file(dir_a.join("hello.txt")).await.unwrap();
    dev_a.engine.run_cycle(&store).await;
    let r = dev_b.engine.run_cycle(&store).await;
    assert_eq!(r.deleted_local, 1);
    assert!(!dir_b.join("hello.txt").exists(), "B 侧应已删除");
    // 冲突副本不受影响
    assert!(dir_b.join(&copy_name).exists());

    // ---- 7. 远端删除 vs 本地修改 → 本地胜出（重新上传）----
    settle().await;
    tokio::fs::write(dir_b.join(&copy_name), "B 继续编辑冲突副本".as_bytes()).await.unwrap();
    // 模拟另一设备删除远端该文件
    store.delete_file(
        &copy_name,
    ).await.unwrap();
    // 远端 manifest 也要移除（模拟另一设备的完整删除流程）
    let mut m = store.read_manifest().await.unwrap().unwrap();
    m.files.remove(&copy_name);
    store.write_manifest(&m).await.unwrap();
    let r = dev_b.engine.run_cycle(&store).await;
    assert_eq!(r.uploaded, 1, "远端被删 + 本地已改 → 本地胜出重新上传");
    assert!(remote_dir.join(&copy_name).exists());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn upload_only_and_download_only() {
    let tmp = std::env::temp_dir().join(format!("synccloud-e2e2-{}", Uuid::new_v4()));
    let _ = std::fs::remove_dir_all(&tmp);
    let remote_dir = tmp.join("remote");
    let dir_a = tmp.join("up");
    let dir_b = tmp.join("down");
    std::fs::create_dir_all(&remote_dir).unwrap();
    let store = LocalFsStore::new(&remote_dir);

    // A 仅上传
    let mut folder = make_device(&dir_a, &tmp.join("a.db"), "U").engine;
    folder.folder.direction = Direction::UploadOnly;
    tokio::fs::write(dir_a.join("up.txt"), b"from upstream").await.unwrap();
    let r = folder.run_cycle(&store).await;
    assert_eq!(r.uploaded, 1);

    // B 仅下载：收到文件；本地修改不会上传
    let mut folder_b = make_device(&dir_b, &tmp.join("b.db"), "D").engine;
    folder_b.folder.direction = Direction::DownloadOnly;
    let r = folder_b.run_cycle(&store).await;
    assert_eq!(r.downloaded, 1);
    assert!(dir_b.join("up.txt").exists());
    settle().await;
    tokio::fs::write(dir_b.join("up.txt"), b"local edit won't upload").await.unwrap();
    let r = folder_b.run_cycle(&store).await;
    assert_eq!(r.uploaded, 0, "仅下载模式不得上传");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// 旧版布局（同步目录内 `.synccloud/`）→ 新版外置元数据的自动迁移：
/// 迁移后同步目录干净，且绝不把"manifest 缺失"误判为"远端已删"而清空本地。
#[tokio::test]
async fn legacy_in_dir_meta_migrates_out_safely() {
    let tmp = std::env::temp_dir().join(format!("synccloud-e2e3-{}", Uuid::new_v4()));
    let _ = std::fs::remove_dir_all(&tmp);
    let remote_dir = tmp.join("remote");
    let dir_a = tmp.join("machineA");
    std::fs::create_dir_all(&remote_dir).unwrap();
    std::fs::create_dir_all(&dir_a).unwrap();
    let store = LocalFsStore::new(&remote_dir);
    let mut dev_a = make_device(&dir_a, &tmp.join("a.db"), "A");

    // 1. 正常同步一次 → 新版元数据落位（同步目录外）
    tokio::fs::write(dir_a.join("hello.txt"), b"v1").await.unwrap();
    let r = dev_a.engine.run_cycle(&store).await;
    assert_eq!(r.uploaded, 1);
    assert!(store.meta_path_of("manifest.json").exists());
    assert_dir_clean(&remote_dir).await;

    // 2. 伪造旧版状态：manifest 移回同步目录内，外置元数据区清空
    let m = store.read_manifest().await.unwrap().unwrap();
    let legacy = remote_dir.join(".synccloud");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("manifest.json"), serde_json::to_vec_pretty(&m).unwrap()).unwrap();
    std::fs::remove_dir_all(&store.meta).unwrap();

    // 3. 再同步：应自动迁移元数据，且不得误删本地文件
    let r = dev_a.engine.run_cycle(&store).await;
    assert_eq!(r.deleted_local, 0, "迁移轮不得把本地文件当作远端已删");
    assert_eq!(r.failed, 0);
    assert!(dir_a.join("hello.txt").exists());
    assert!(remote_dir.join("hello.txt").exists());
    assert!(!legacy.exists(), "同步目录内的旧 .synccloud 应被清理");
    assert!(store.meta_path_of("manifest.json").exists(), "manifest 应迁移到目录外元数据区");
    assert_dir_clean(&remote_dir).await;

    // 4. 新设备仍可正常接入（读取外置 manifest）
    let dir_b = tmp.join("machineB");
    std::fs::create_dir_all(&dir_b).unwrap();
    let mut dev_b = make_device(&dir_b, &tmp.join("b.db"), "B");
    let r = dev_b.engine.run_cycle(&store).await;
    assert_eq!(r.downloaded, 1);
    assert!(dir_b.join("hello.txt").exists());
    assert_dir_clean(&dir_b).await;

    let _ = std::fs::remove_dir_all(&tmp);
}

// ---- 增量快路径 + 崩溃恢复日志 ----

/// manifest 写入可注入失败的存储（其余委托 LocalFsStore）。
struct FlakyManifestStore {
    inner: LocalFsStore,
    fail_manifest_write: std::sync::atomic::AtomicBool,
}

impl FlakyManifestStore {
    fn new(root: &Path) -> Self {
        Self {
            inner: LocalFsStore::new(root),
            fail_manifest_write: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl RemoteStore for FlakyManifestStore {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    fn raw_io(&self) -> &dyn RawIo {
        self.inner.raw_io()
    }
    async fn test(&self) -> anyhow::Result<()> {
        self.inner.test().await
    }
    async fn read_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        self.inner.read_manifest().await
    }
    async fn write_manifest(&self, manifest: &Manifest) -> anyhow::Result<()> {
        if self.fail_manifest_write.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("注入的 manifest 写入失败（模拟崩溃/断网）");
        }
        self.inner.write_manifest(manifest).await
    }
    async fn put_file(&self, path: &str, src: &Path) -> anyhow::Result<()> {
        self.inner.put_file(path, src).await
    }
    async fn get_file(&self, path: &str, dest: &Path) -> anyhow::Result<()> {
        self.inner.get_file(path, dest).await
    }
    async fn delete_file(&self, path: &str) -> anyhow::Result<()> {
        self.inner.delete_file(path).await
    }
    async fn list_objects(&self) -> anyhow::Result<Vec<RemoteEntry>> {
        self.inner.list_objects().await
    }
}

/// 1) 无变化轮次走增量快路径（skipped，不取锁不下载 manifest）；
/// 2) 上传成功但 manifest 写入失败（模拟崩溃）后，下一轮通过日志重放收敛，
///    不重传文件（这正是「每次重启全部同步一遍」的根因之一）。
#[tokio::test]
async fn fast_path_skip_and_crash_recovery_journal() {
    let tmp = std::env::temp_dir().join(format!("synccloud-e2e4-{}", Uuid::new_v4()));
    let _ = std::fs::remove_dir_all(&tmp);
    let remote_dir = tmp.join("remote");
    let dir_a = tmp.join("machineA");
    std::fs::create_dir_all(&remote_dir).unwrap();
    std::fs::create_dir_all(&dir_a).unwrap();
    let store = FlakyManifestStore::new(&remote_dir);
    let mut dev = make_device(&dir_a, &tmp.join("a.db"), "A");
    // 重试为零：注入失败立即返回，避免测试等待退避
    dev.engine.settings.max_retries = 0;

    // 1. 正常上传
    tokio::fs::write(dir_a.join("a.txt"), b"v1").await.unwrap();
    let r = dev.engine.run_cycle(&store).await;
    assert_eq!(r.uploaded, 1);

    // 2. 无任何变化 → 增量快路径：零传输、标记 skipped
    let r = dev.engine.run_cycle(&store).await;
    assert_eq!(r.skipped, 1, "无变化轮次应走快路径");
    assert_eq!(r.uploaded, 0);

    // 3. 新文件上传成功，但 manifest 写入失败（模拟崩溃）
    settle().await;
    tokio::fs::write(dir_a.join("b.txt"), b"b-content").await.unwrap();
    store.fail_manifest_write.store(true, std::sync::atomic::Ordering::Relaxed);
    let r = dev.engine.run_cycle(&store).await;
    assert_eq!(r.uploaded, 1, "文件本体应已写入远端存储");
    assert_eq!(r.failed, 1, "manifest 写入失败应计为失败");
    assert!(remote_dir.join("b.txt").exists(), "远端存储已有 b.txt");
    assert!(
        !store.inner.read_manifest().await.unwrap().unwrap().files.contains_key("b.txt"),
        "manifest 尚未包含 b.txt（写入失败）"
    );

    // 4. 恢复后下一轮：重放日志 → 不重传、manifest 收敛、文件保留
    store.fail_manifest_write.store(false, std::sync::atomic::Ordering::Relaxed);
    let r = dev.engine.run_cycle(&store).await;
    assert_eq!(r.uploaded, 0, "崩溃恢复轮不得重传已上传文件");
    assert_eq!(r.failed, 0);
    assert!(dir_a.join("b.txt").exists(), "本地文件不得被误删");
    let m = store.inner.read_manifest().await.unwrap().unwrap();
    assert!(m.files.contains_key("b.txt"), "日志重放后 manifest 应包含 b.txt");

    // 5. 再次无变化 → 快路径
    let r = dev.engine.run_cycle(&store).await;
    assert_eq!(r.skipped, 1);

    let _ = std::fs::remove_dir_all(&tmp);
}
