//! 存储后端抽象层：统一的 `RemoteStore` trait + 远端分布式锁 + 仓库布局约定。
//!
//! ## 仓库布局（远端）
//!
//! 引擎的元数据放在同步目录**之外**，同步目录内只出现用户自己的文件：
//!
//! ```text
//! <父目录>/
//! ├── .synccloud/<同步目录名>/   # 元数据区（同步目录的兄弟目录；Git 后端为独立
//! │   ├── manifest.json          # synccloud-meta/<分支> ref；S3 根前缀为 .synccloud/<前缀>）
//! │   └── locks/<device_id>      # 分布式锁（TTL 心跳），防止多机同时写 manifest
//! └── <同步目录>/                # 纯用户文件（明文模式 = 镜像布局）
//! ```
//! 同步根本身位于存储顶层（如云盘根目录）时退化为根下 `.synccloud/`（此时该目录
//! 会被列举过滤，永不进入同步树）。

pub mod s3;
pub mod webdav;
pub mod git_backend;
pub mod baidu;
pub mod aliyun;

use async_trait::async_trait;
use std::path::Path;
use std::time::Duration;

use crate::sync::types::Manifest;

/// 远端对象条目（对账/垃圾回收用）。
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub key: String,
    pub size: u64,
}

/// 计算同步根的**兄弟元数据区**（不含首尾斜杠的相对形式）。
/// `<父>/<.synccloud>/<名>`：`/apps/synccloud` → `apps/.synccloud/synccloud`；
/// 根级目录 `/Sync` → `.synccloud/Sync`；根本身（空）→ `.synccloud`（退化为根下）。
pub fn sibling_meta(root: &str) -> String {
    let r = root.trim_matches('/');
    if r.is_empty() {
        return ".synccloud".into();
    }
    match r.rfind('/') {
        Some(i) => format!("{}/.synccloud/{}", &r[..i], &r[i + 1..]),
        None => format!(".synccloud/{r}"),
    }
}

/// 存储后端统一接口。所有差异（认证、布局、版本语义）封装在实现内部。
#[async_trait]
pub trait RemoteStore: Send + Sync {
    fn kind(&self) -> &'static str;

    /// 锁机制所需的最小读写原语视图。
    fn raw_io(&self) -> &dyn RawIo;

    /// 连接与权限测试（读 + 写探针）。
    async fn test(&self) -> anyhow::Result<()>;

    async fn read_manifest(&self) -> anyhow::Result<Option<Manifest>>;

    /// manifest 的**廉价变更戳**（etag / 版本号 / 更新时间等）。
    /// 引擎的增量快路径在「本地无变化」时用它探测远端是否变化，
    /// 戳未变则整轮同步零成本跳过（不取锁、不下载 manifest）。
    /// 返回 None 表示 manifest 不存在。默认实现读取完整 manifest 派生
    /// （正确但较重）；支持廉价元数据查询的后端（S3 HEAD / WebDAV PROPFIND /
    /// 云盘目录项）应覆盖。任何实现都必须保证：manifest 内容变化 ⇒ 戳变化。
    async fn manifest_stamp(&self) -> anyhow::Result<Option<String>> {
        Ok(self.read_manifest().await?.map(|m| manifest_stamp_of(&m)))
    }

    /// 写入新 manifest（调用方必须已持有远端锁）。
    async fn write_manifest(&self, manifest: &Manifest) -> anyhow::Result<()>;

    /// 上传用户文件：`path` 为统一相对路径。
    async fn put_file(&self, path: &str, src: &Path) -> anyhow::Result<()>;

    /// 下载用户文件到本地路径（引擎保证 dest 在本地临时目录）。
    async fn get_file(&self, path: &str, dest: &Path) -> anyhow::Result<()>;

    async fn delete_file(&self, path: &str) -> anyhow::Result<()>;

    /// 远端重命名/移动文件（支持服务端移动的后端如 S3 copy+delete, WebDAV MOVE 等覆盖实现）。
    /// 默认实现返回不支持错误，引擎会自动回退到上传新路径 + 删除旧路径。
    async fn move_file(&self, _from: &str, _to: &str) -> anyhow::Result<()> {
        anyhow::bail!("服务端移动未实现")
    }

    /// 列出远端实际对象（不含引擎元数据），用于对账与垃圾回收。
    async fn list_objects(&self) -> anyhow::Result<Vec<RemoteEntry>>;

    /// 读取旧版布局（同步目录内 `.synccloud/manifest.json`）的 manifest。
    /// 仅用于一次性迁移：新版元数据缺失而旧版存在时，引擎把它搬到新位置。
    async fn read_legacy_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        Ok(None)
    }

    /// 删除旧版布局残留在同步目录内的 `.synccloud/` 元数据。
    /// 实现必须自守：元数据区与旧位置重合（根级退化场景）时应直接返回。
    async fn cleanup_legacy_meta(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// 远端分布式锁：基于元数据区 `locks/<device>` + TTL。
/// 崩溃的持锁者其锁会在 TTL 后被视为过期，可被抢占。
pub struct RemoteLock {
    store_lock_key: String,
}

/// 锁文件内容：`<device_id>|<expiry_unix_ms>`
fn lock_content(device_id: &str, ttl: Duration) -> String {
    let expiry = chrono::Utc::now().timestamp_millis() + ttl.as_millis() as i64;
    format!("{device_id}|{expiry}")
}

fn is_expired(content: &str) -> bool {
    match content.split('|').nth(1).and_then(|s| s.parse::<i64>().ok()) {
        Some(expiry) => chrono::Utc::now().timestamp_millis() >= expiry,
        None => true, // 格式异常视为过期，可抢占
    }
}

/// 尝试获取远端锁（基于 RemoteStore::raw_io 原语）。
/// - 列举所有锁，若存在未过期且非本设备的锁 → 等待重试直至超时。
pub async fn acquire_remote_lock(
    store: &dyn RemoteStore,
    device_id: &str,
    wait_timeout: Duration,
) -> anyhow::Result<RemoteLock> {
    let raw_io = store.raw_io();
    // 锁文件位于同步目录外的元数据区，键相对元数据根
    let lock_dir = "locks";
    let lock_key = format!("{lock_dir}/{device_id}");
    let deadline = tokio::time::Instant::now() + wait_timeout;

    loop {
        // 清理过期锁（尽力而为）
        let existing = raw_io.list_prefix(lock_dir).await.unwrap_or_default();
        let mut held_by_other = false;
        for key in existing {
            if let Some(content) = raw_io.read_text(&key).await.unwrap_or(None) {
                let owner = content.split('|').next().unwrap_or("").to_string();
                if is_expired(&content) {
                    let _ = raw_io.delete(&key).await;
                } else if owner != device_id {
                    held_by_other = true;
                }
            }
        }
        if !held_by_other {
            // 写自己的锁（带 TTL），回读验证（处理"同时写入"竞态的最后防线）
            let content = lock_content(device_id, Duration::from_secs(90));
            raw_io.put_text(&lock_key, &content).await?;
            if let Some(readback) = raw_io.read_text(&lock_key).await? {
                if read_back_mine(&readback, device_id) {
                    return Ok(RemoteLock { store_lock_key: lock_key });
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("获取远端锁超时：另一台设备正在同步该仓库");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn read_back_mine(content: &str, device_id: &str) -> bool {
    content.split('|').next() == Some(device_id)
}

impl RemoteLock {
    /// 释放锁（删除锁文件）。失败仅记录：TTL 会兜底。
    pub async fn release(&self, raw_io: &dyn RawIo) {
        if let Err(e) = raw_io.delete(&self.store_lock_key).await {
            tracing::warn!(error = %e, "释放远端锁失败（将依赖 TTL 过期）");
        }
    }
}

/// 后端的最小文本读写原语，锁机制在其上构建。
/// **键相对元数据区根**（同步目录之外），各后端自行映射到自己的地址空间。
#[async_trait]
pub trait RawIo: Send + Sync {
    async fn put_text(&self, key: &str, content: &str) -> anyhow::Result<()>;
    async fn read_text(&self, key: &str) -> anyhow::Result<Option<String>>;
    async fn delete(&self, key: &str) -> anyhow::Result<()>;
    async fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<String>>;
}

#[cfg(test)]
mod tests {
    use super::sibling_meta;

    #[test]
    fn sibling_meta_paths() {
        assert_eq!(sibling_meta("/Sync"), ".synccloud/Sync");
        assert_eq!(sibling_meta("/apps/synccloud/"), "apps/.synccloud/synccloud");
        assert_eq!(sibling_meta("/a/b/Sync"), "a/b/.synccloud/Sync");
        // 根级目录
        assert_eq!(sibling_meta("docs"), ".synccloud/docs");
        // 同步根就是存储根：退化为根下 .synccloud
        assert_eq!(sibling_meta("/"), ".synccloud");
        assert_eq!(sibling_meta(""), ".synccloud");
    }
}

/// 从 manifest 内容派生变更戳（版本 + 序号 + 文件数 + 更新时间）。
/// 任何一次 manifest 写入都会至少改变 seq 与 updated_at。
pub fn manifest_stamp_of(m: &Manifest) -> String {
    format!("v{}|seq{}|n{}|{}", m.version, m.seq, m.files.len(), m.updated_at)
}

/// 根据配置构造后端实例。敏感字段已由上层从系统凭据管理器取出填入。
pub async fn build_store(backend: &crate::config::BackendConfig) -> anyhow::Result<Box<dyn RemoteStore>> {
    use crate::config::BackendConfig as B;
    match backend {
        B::S3 { endpoint, region, bucket, access_key_id, secret_access_key, path_style, root_prefix } => {
            Ok(Box::new(
                s3::S3Store::new(
                    endpoint.clone(),
                    region.clone(),
                    bucket.clone(),
                    access_key_id.clone(),
                    secret_access_key.clone(),
                    *path_style,
                    root_prefix.clone(),
                )
                .await,
            ))
        }
        B::WebDav { url, username, password, root_path } => Ok(Box::new(webdav::WebDavStore::new(
            url.clone(),
            username.clone(),
            password.clone(),
            root_path.clone(),
        ))),
        B::Git { remote_url, branch, username, password } => Ok(Box::new(git_backend::GitStore::new(
            remote_url.clone(),
            branch.clone(),
            username.clone(),
            password.clone(),
        ))),
        B::BaiduPan { auth_id, app_key, secret_key, remote_dir, .. } => Ok(Box::new(baidu::BaiduStore::new(
            auth_id.clone(),
            app_key.clone(),
            secret_key.clone(),
            remote_dir.clone(),
        ))),
        B::AliyunDrive { auth_id, remote_dir, .. } => Ok(Box::new(aliyun::AliyunStore::new(
            auth_id.clone(),
            remote_dir.clone(),
        ))),
    }
}
