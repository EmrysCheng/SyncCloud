//! 同步引擎编排：扫描 → 增量快路径 → 远端锁 → manifest 拉取（含崩溃恢复重放）
//! → 三方对比 → 并行执行动作 → 提交基线与 manifest。
//! 一个 SyncEngine 实例对应一个同步文件夹，由 manager 保证串行调用 run_cycle。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use serde::Serialize;
use uuid::Uuid;

use crate::config::{FolderConfig, GlobalSettings};
use crate::storage::{self, manifest_stamp_of, RemoteStore};
use crate::sync::conflict;
use crate::sync::plan::{self, Action, SyncPlan};
use crate::sync::scanner::{abs_path, Scanner};
use crate::sync::state::{JournalOp, StateDb};
use crate::sync::types::{CycleReport, FileMeta, FileTree, Manifest};

/// 远端 manifest 缺失时的变更戳哨兵值。
const STAMP_ABSENT: &str = "absent";

/// 引擎对外事件（broadcast 给 UI 与日志）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum SyncEvent {
    CycleStarted { folder_id: Uuid, folder: String },
    ScanDone { folder_id: Uuid, files: usize, hashed: usize, cache_hits: usize, ignored: usize },
    /// 增量快路径命中：本地与远端均无变化，本轮零传输
    CycleSkipped { folder_id: Uuid, folder: String },
    Planned { folder_id: Uuid, stats: plan::PlanStats },
    FileDone { folder_id: Uuid, path: String, action: String },
    FileFailed { folder_id: Uuid, path: String, error: String, retry_in_secs: u64 },
    ConflictCreated { folder_id: Uuid, path: String, copy: String },
    CycleDone { folder_id: Uuid, folder: String, report: CycleReport },
    Error { folder_id: Uuid, folder: String, message: String },
}

#[derive(Clone)]
pub struct EventSink(tokio::sync::mpsc::UnboundedSender<SyncEvent>);

impl EventSink {
    pub fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<SyncEvent>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self(tx), rx)
    }
    pub fn emit(&self, ev: SyncEvent) {
        let _ = self.0.send(ev);
    }
}

pub struct SyncEngine {
    pub folder: FolderConfig,
    pub device_id: String,
    pub device_name: String,
    pub settings: GlobalSettings,
    pub db: StateDb,
    pub events: EventSink,
}

/// 单个动作对远端 manifest 的增量变更（并行执行后统一合并）。
enum DeltaOp {
    Put(String, FileMeta),
    Remove(String),
}

impl SyncEngine {
    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.folder.local_path)
    }

    /// 执行一轮完整同步。本地与远端均无实质变化时走增量快路径，
    /// 不获取远端锁、不下载 manifest、不做三方对比（坚果云式增量体验）。
    pub async fn run_cycle(&mut self, store: &dyn RemoteStore) -> CycleReport {
        let started = Instant::now();
        let mut report = CycleReport::default();
        let fid = self.folder.id;

        self.events.emit(SyncEvent::CycleStarted { folder_id: fid, folder: self.folder.name.clone() });

        // 0. 本地同步目录安全保证：绝不在同步文件夹内创建或保留 .synccloud 目录
        let local_synccloud = self.root().join(".synccloud");
        if local_synccloud.exists() {
            let _ = tokio::fs::remove_dir_all(&local_synccloud).await;
        }

        // 1. 本地扫描（哈希缓存命中时只做 stat）+ 基线
        let scanned = (|| -> anyhow::Result<(crate::sync::types::FileTree, crate::sync::scanner::ScanStats, crate::sync::types::FileTree)> {
            let scanner = Scanner::new(&self.folder.ignore_rules, Some(&self.db))?;
            let (local, stats) = scanner.scan(&self.root())?;
            let baseline = self.db.load_baseline()?;
            Ok((local, stats, baseline))
        })();
        let (local, scan_stats, baseline) = match scanned {
            Ok(v) => v,
            Err(e) => {
                self.events.emit(SyncEvent::Error {
                    folder_id: fid,
                    folder: self.folder.name.clone(),
                    message: format!("本地扫描失败：{e:#}"),
                });
                report.failed += 1;
                return report;
            }
        };
        self.events.emit(SyncEvent::ScanDone {
            folder_id: fid,
            files: scan_stats.files,
            hashed: scan_stats.hashed,
            cache_hits: scan_stats.hash_cache_hits,
            ignored: scan_stats.ignored,
        });

        // 2. 增量快路径：本地无实质变化、无待重放日志、远端变更戳未变 → 整轮跳过
        let has_pending_journal = !self.db.journal_load().unwrap_or_default().is_empty();
        if !tree_changed(&local, &baseline) && !has_pending_journal {
            let stamp_now = match store.manifest_stamp().await {
                // manifest 缺失也用哨兵值参与比较（首 sync 后远端一直为空时可快路径）
                Ok(s) => Some(s.unwrap_or_else(|| STAMP_ABSENT.to_string())),
                Err(_) => None, // 探测失败不致命，退回完整路径
            };
            if stamp_now.as_deref() == self.db.kv_get("manifest_stamp").as_deref() {
                report.skipped = 1;
                self.db.kv_set("last_sync_at", &chrono::Local::now().to_rfc3339());
                self.events.emit(SyncEvent::CycleSkipped {
                    folder_id: fid,
                    folder: self.folder.name.clone(),
                });
                report.took_ms = started.elapsed().as_millis() as u64;
                self.events.emit(SyncEvent::CycleDone {
                    folder_id: fid,
                    folder: self.folder.name.clone(),
                    report: report.clone(),
                });
                return report;
            }
        }

        // 3. 远端锁：防止多机同时写 manifest 交叉覆盖
        let lock = match storage::acquire_remote_lock(store, &self.device_id, Duration::from_secs(60)).await {
            Ok(l) => Some(l),
            Err(e) => {
                self.events.emit(SyncEvent::Error {
                    folder_id: fid,
                    folder: self.folder.name.clone(),
                    message: format!("获取远端锁失败，本轮跳过：{e:#}"),
                });
                report.failed += 1;
                report.took_ms = started.elapsed().as_millis() as u64;
                return report;
            }
        };

        let result = self.run_cycle_locked(store, &mut report, local, baseline).await;

        // 无论成败都释放锁
        if let Some(l) = lock {
            l.release(store.raw_io()).await;
        }

        report.took_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(()) => {
                self.db.kv_set("last_sync_at", &chrono::Local::now().to_rfc3339());
                self.events.emit(SyncEvent::CycleDone {
                    folder_id: fid,
                    folder: self.folder.name.clone(),
                    report: report.clone(),
                });
            }
            Err(e) => {
                report.failed += 1;
                self.events.emit(SyncEvent::Error {
                    folder_id: fid,
                    folder: self.folder.name.clone(),
                    message: format!("{e:#}"),
                });
            }
        }
        report
    }

    /// 持锁后的同步主体。
    async fn run_cycle_locked(
        &mut self,
        store: &dyn RemoteStore,
        report: &mut CycleReport,
        local: FileTree,
        baseline: FileTree,
    ) -> anyhow::Result<()> {
        let fid = self.folder.id;

        // 4. 远端 manifest（元数据区在同步目录之外）。
        //    兼容旧版布局：新位置缺失而同步目录内存在旧 `.synccloud/` 时，
        //    原地迁移到新位置并清理旧目录。绝不能把"manifest 缺失"直接当
        //    "远端已全部删除"使用——那会导致本地文件被整批误删。
        let mut remote = store.read_manifest().await?;
        if remote.is_none() {
            if let Some(legacy) = store.read_legacy_manifest().await? {
                store.write_manifest(&legacy).await?;
                if let Err(e) = store.cleanup_legacy_meta().await {
                    tracing::warn!(error = %e, "清理同步目录内旧版 .synccloud 元数据失败（不影响同步）");
                }
                tracing::info!(folder = %self.folder.name, "远端元数据已迁移到同步目录之外");
                remote = Some(legacy);
            }
        }

        // 5. 崩溃恢复：重放"已写入远端存储但 manifest 尚未提交"的变更日志。
        //    没有这一步，上传成功而 manifest 写入失败的文件在下一轮会被三方对比
        //    误判为「远端已删除」→ 删除本地文件，或触发全量重传。
        let journal = self.db.journal_load().unwrap_or_default();
        let mut remote_dirty = false;
        if !journal.is_empty() {
            let m = remote.get_or_insert_with(|| Manifest::new(&self.device_name));
            for op in &journal {
                match op {
                    JournalOp::Put(p, meta) => {
                        m.files.insert(p.clone(), meta.clone());
                    }
                    JournalOp::Remove(p) => {
                        m.files.remove(p);
                    }
                }
            }
            remote_dirty = true;
            tracing::info!(folder = %self.folder.name, n = journal.len(), "重放未提交的远端变更日志");
        }

        // 6. 三方对比
        let mut plan: SyncPlan = plan::compare(
            &local,
            remote.as_ref(),
            &baseline,
            self.folder.direction,
            self.folder.conflict_policy,
        );
        self.events.emit(SyncEvent::Planned { folder_id: fid, stats: plan.stats.clone() });

        if plan.actions.is_empty() && !remote_dirty {
            // 完全无变化：记录变更戳供下轮快路径使用
            let stamp = remote
                .as_ref()
                .map(manifest_stamp_of)
                .unwrap_or_else(|| STAMP_ABSENT.to_string());
            self.db.kv_set("manifest_stamp", &stamp);
            return Ok(());
        }

        // 7. 并行执行动作（concurrency 上限；每项带指数退避重试应对瞬时文件占用）。
        //    动作之间路径互斥，并行不会互相干扰；对 manifest 的影响以增量 DeltaOp
        //    返回，合并阶段统一应用（不再每个动作克隆整棵 manifest 树）。
        let max_retries = self.settings.max_retries;
        let concurrency = self.settings.concurrency.max(1) as usize;
        let engine: &SyncEngine = self;
        let events = self.events.clone();

        let results: Vec<(Action, anyhow::Result<Vec<DeltaOp>>)> =
            stream::iter(std::mem::take(&mut plan.actions))
                .map(|action| {
                    let events = events.clone();
                    async move {
                        let mut attempt: u32 = 0;
                        loop {
                            match engine.execute_action(store, &action).await {
                                Ok(ops) => break (action, Ok(ops)),
                                Err(e) => {
                                    if attempt >= max_retries {
                                        break (action, Err(e));
                                    }
                                    let delay =
                                        Duration::from_secs(1u64.saturating_pow(attempt + 1).min(60));
                                    events.emit(SyncEvent::FileFailed {
                                        folder_id: fid,
                                        path: action_path(&action),
                                        error: format!("{e:#}"),
                                        retry_in_secs: delay.as_secs(),
                                    });
                                    tokio::time::sleep(delay).await;
                                    attempt += 1;
                                }
                            }
                        }
                    }
                })
                .buffer_unordered(concurrency)
                .collect()
                .await;

        // 8. 合并结果：manifest 增量 + 报告统计 + 崩溃恢复日志
        let mut final_manifest_files = remote.as_ref().map(|m| m.files.clone()).unwrap_or_default();
        let mut final_local = local;
        let mut need_rescan = false;
        for (action, outcome) in results {
            match outcome {
                Ok(ops) => {
                    for op in &ops {
                        match op {
                            DeltaOp::Put(p, m) => {
                                final_manifest_files.insert(p.clone(), m.clone());
                                remote_dirty = true;
                                self.db.journal_put(p, m);
                            }
                            DeltaOp::Remove(p) => {
                                final_manifest_files.remove(p);
                                remote_dirty = true;
                                self.db.journal_remove(p);
                            }
                        }
                    }
                    apply_report(report, &action);
                    // 下载类动作会改动本地文件（mtime/冲突副本），基线需要重扫得到真实状态；
                    // 纯上传/删除轮次直接复用扫描结果，省一次全目录遍历。
                    need_rescan |= matches!(
                        action,
                        Action::Download { .. } | Action::RemoteWinsOverDelete { .. } | Action::Conflict { .. }
                    );
                    if let Action::DeleteLocal { path } = &action {
                        final_local.remove(path);
                    }
                    self.events.emit(SyncEvent::FileDone {
                        folder_id: fid,
                        path: action_path(&action),
                        action: action_kind(&action),
                    });
                }
                Err(e) => {
                    report.failed += 1;
                    tracing::error!(folder = %self.folder.name, path = %action_path(&action), error = %e, "动作执行失败");
                }
            }
        }

        // 9. 写远端 manifest（仅远端有变化时），成功后清空崩溃恢复日志
        let mut stamp = remote
            .as_ref()
            .map(manifest_stamp_of)
            .unwrap_or_else(|| STAMP_ABSENT.to_string());
        if remote_dirty {
            let seq = remote.as_ref().map(|m| m.seq).unwrap_or(0) + 1;
            let mut manifest = Manifest::new(&self.device_name);
            manifest.seq = seq;
            manifest.files = final_manifest_files;
            let mut attempt: u32 = 0;
            loop {
                match store.write_manifest(&manifest).await {
                    Ok(()) => break,
                    Err(e) => {
                        if attempt >= max_retries {
                            return Err(e);
                        }
                        let delay =
                            Duration::from_secs(1u64.saturating_pow(attempt + 1).min(60));
                        tracing::warn!(error = %e, retry_in_secs = delay.as_secs(), "manifest 写入失败，重试");
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                    }
                }
            }
            self.db.journal_clear();
            stamp = manifest_stamp_of(&manifest);
        }
        self.db.kv_set("manifest_stamp", &stamp);

        // 10. 基线更新：下载类轮次重扫真实本地状态（mtime、冲突副本如实反映），
        //     其余轮次用扫描结果扣除本轮删除的本地文件即可。
        if need_rescan {
            let (final_local, _) = {
                let scanner = Scanner::new(&self.folder.ignore_rules, Some(&self.db))?;
                scanner.scan(&self.root())?
            };
            self.db.replace_baseline(&final_local)?;
        } else {
            self.db.replace_baseline(&final_local)?;
        }

        Ok(())
    }

    /// 执行单个动作，返回对远端 manifest 的增量变更（空 = 远端无变化）。
    async fn execute_action(
        &self,
        store: &dyn RemoteStore,
        action: &Action,
    ) -> anyhow::Result<Vec<DeltaOp>> {
        let path_str = action_path(action);
        if path_str.starts_with(".synccloud") || path_str.contains("/.synccloud") || path_str.contains("\\.synccloud") {
            return Ok(Vec::new());
        }
        let root = self.root();
        match action {
            Action::Upload { path, meta } | Action::LocalWinsOverDelete { path, local: meta } => {
                let src = abs_path(&root, path);
                store.put_file(path, &src).await?;
                Ok(vec![DeltaOp::Put(path.clone(), meta.clone())])
            }
            Action::Download { path, meta } | Action::RemoteWinsOverDelete { path, remote: meta } => {
                self.download_to(store, path, &root, meta).await?;
                match action {
                    // 本地曾删除而远端被他人修改：恢复 manifest 条目
                    Action::RemoteWinsOverDelete { path, remote } => {
                        Ok(vec![DeltaOp::Put(path.clone(), remote.clone())])
                    }
                    _ => Ok(Vec::new()),
                }
            }
            Action::DeleteRemote { path } => {
                store.delete_file(path).await?;
                Ok(vec![DeltaOp::Remove(path.clone())])
            }
            Action::DeleteLocal { path } => {
                let target = abs_path(&root, path);
                match tokio::fs::remove_file(&target).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                // 清理空父目录（仅限同步根内的空目录，尽力而为）
                self.prune_empty_parents(&root, Path::new(path));
                Ok(Vec::new()) // 远端本来就没有，manifest 无需变化
            }
            Action::Identical { path: _, meta: _ } => {
                // 双方内容一致：远端无需变化，仅本地基线更新（重扫描后自然完成）
                Ok(Vec::new())
            }
            Action::Conflict { path, local, remote: remote_meta } => {
                // Dropbox 式处理：
                // a) 本地文件改名 → 冲突副本
                let abs = abs_path(&root, path);
                let parent_dir = abs.parent().unwrap_or(&root).to_path_buf();
                let orig_name = path.rsplit('/').next().unwrap_or(path);
                let copy_name = conflict::unique_conflict_copy(
                    &parent_dir,
                    orig_name,
                    &self.device_name,
                    chrono::Local::now(),
                );
                let copy_abs = parent_dir.join(&copy_name);
                match tokio::fs::rename(&abs, &copy_abs).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                // b) 远端版本下载为正式文件
                self.download_to(store, path, &root, remote_meta).await?;
                // c) 冲突副本作为新文件上传远端
                let copy_rel = format!(
                    "{}/{}",
                    path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or(""),
                    copy_name
                )
                .trim_start_matches('/')
                .to_string();
                store.put_file(&copy_rel, &copy_abs).await?;
                // 记录冲突供 UI 冲突中心展示
                let _ = self.db.record_conflict(
                    path,
                    &local.hash,
                    &remote_meta.hash,
                    Some(&copy_rel),
                );
                self.events.emit(SyncEvent::ConflictCreated {
                    folder_id: self.folder.id,
                    path: path.clone(),
                    copy: copy_rel.clone(),
                });
                // d) manifest：正式文件 = 远端版本；新增冲突副本条目
                let copy_meta = FileMeta {
                    size: local.size,
                    mtime_ms: chrono::Utc::now().timestamp_millis(),
                    hash: local.hash.clone(),
                };
                Ok(vec![
                    DeltaOp::Put(path.clone(), remote_meta.clone()),
                    DeltaOp::Put(copy_rel, copy_meta),
                ])
            }
        }
    }

    /// 下载远端文件并落盘（临时文件置于应用数据目录，绝不污染同步文件夹）。
    #[allow(clippy::items_after_statements)]
    async fn download_to(
        &self,
        store: &dyn RemoteStore,
        path: &str,
        root: &Path,
        expected: &crate::sync::types::FileMeta,
    ) -> anyhow::Result<()> {
        if path.starts_with(".synccloud") || path.contains("/.synccloud") || path.contains("\\.synccloud") {
            return Ok(());
        }
        let target = abs_path(root, path);
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // 临时文件置于专门的应用数据临时目录，绝不出现在同步文件夹中
        let temp_dir = crate::config::app_data_dir().join("temp");
        tokio::fs::create_dir_all(&temp_dir).await?;
        let tmp = temp_dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        store.get_file(path, &tmp).await?;
        // 内容校验：远端 manifest 的 size/hash 为权威。某些云存储（如百度 dlink）
        // 刚上传后可能返回空/过期响应，校验失败交给外层动作重试，防止静默损坏本地文件。
        let got_size = tokio::fs::metadata(&tmp).await?.len();
        if got_size != expected.size {
            let _ = tokio::fs::remove_file(&tmp).await;
            anyhow::bail!(
                "下载校验失败（大小不一致：期望 {} 字节，实得 {got_size}），远端数据可能尚未就绪",
                expected.size
            );
        }
        if !expected.hash.is_empty() {
            let got_hash = crate::sync::scanner::hash_file(&tmp)?;
            if got_hash != expected.hash {
                let _ = tokio::fs::remove_file(&tmp).await;
                anyhow::bail!("下载校验失败（哈希不一致：期望 {}，实得 {got_hash}）", expected.hash);
            }
        }
        // Windows：目标存在时 rename 会失败 → 先删后改名（崩溃窗口由下轮同步自愈）
        match tokio::fs::remove_file(&target).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(anyhow::anyhow!("目标文件被占用，无法替换 {}: {e}", target.display()));
            }
        }
        if let Err(_) = tokio::fs::rename(&tmp, &target).await {
            tokio::fs::copy(&tmp, &target).await?;
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        Ok(())
    }

    fn prune_empty_parents(&self, root: &Path, rel: &Path) {
        let mut dir = rel.parent().map(|p| p.to_path_buf());
        while let Some(d) = dir {
            if d.as_os_str().is_empty() {
                break;
            }
            let abs = root.join(&d);
            let empty = match std::fs::read_dir(&abs) {
                Ok(mut it) => it.next().is_none(),
                Err(_) => false,
            };
            if empty {
                let _ = std::fs::remove_dir(&abs);
            } else {
                break;
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
}

/// 本地树相对基线是否有实质变化（内容哈希 + 大小；路径增删由长度差覆盖）。
fn tree_changed(cur: &FileTree, base: &FileTree) -> bool {
    if cur.len() != base.len() {
        return true;
    }
    cur.iter().any(|(p, m)| match base.get(p) {
        Some(b) => b.hash != m.hash || b.size != m.size,
        None => true,
    })
}

/// 按动作类型累计报告统计（并行执行后在合并阶段调用）。
fn apply_report(report: &mut CycleReport, action: &Action) {
    match action {
        Action::Upload { meta, .. } | Action::LocalWinsOverDelete { local: meta, .. } => {
            report.uploaded += 1;
            report.bytes_uploaded += meta.size;
        }
        Action::Download { meta, .. } => {
            report.downloaded += 1;
            report.bytes_downloaded += meta.size;
        }
        Action::RemoteWinsOverDelete { remote, .. } => {
            report.downloaded += 1;
            report.bytes_downloaded += remote.size;
        }
        Action::DeleteRemote { .. } => {
            report.deleted_remote += 1;
        }
        Action::DeleteLocal { .. } => {
            report.deleted_local += 1;
        }
        Action::Identical { .. } => {
            report.conflicts_resolved_content_identical += 1;
        }
        Action::Conflict { remote, .. } => {
            report.conflicts += 1;
            report.downloaded += 1;
            report.bytes_downloaded += remote.size;
        }
    }
}

fn action_path(a: &Action) -> String {
    match a {
        Action::Upload { path, .. }
        | Action::Download { path, .. }
        | Action::DeleteRemote { path }
        | Action::DeleteLocal { path }
        | Action::Identical { path, .. }
        | Action::Conflict { path, .. }
        | Action::RemoteWinsOverDelete { path, .. }
        | Action::LocalWinsOverDelete { path, .. } => path.clone(),
    }
}

fn action_kind(a: &Action) -> String {
    match a {
        Action::Upload { .. } => "upload".into(),
        Action::Download { .. } => "download".into(),
        Action::DeleteRemote { .. } => "delete_remote".into(),
        Action::DeleteLocal { .. } => "delete_local".into(),
        Action::Identical { .. } => "identical".into(),
        Action::Conflict { .. } => "conflict".into(),
        Action::RemoteWinsOverDelete { .. } => "remote_wins".into(),
        Action::LocalWinsOverDelete { .. } => "local_wins".into(),
    }
}
