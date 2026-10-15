//! 同步引擎编排：扫描 → 增量快路径 → 远端锁 → manifest 拉取（含崩溃恢复重放）
//! → 三方对比 → 并行执行动作 → 提交基线与 manifest。
//! 一个 SyncEngine 实例对应一个同步文件夹，由 manager 保证串行调用 run_cycle。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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

/// 文件被独占锁定导致的动作挂起（非失败）：不计入 failed，不重试，
/// 基线保持不变，下一轮自动重新规划，占用释放后自然收敛。
#[derive(Debug, Clone)]
pub(crate) struct PendingLockError {
    pub path: String,
}

impl std::fmt::Display for PendingLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "文件被其他程序占用，已挂起待下一轮处理: {}", self.path)
    }
}

impl std::error::Error for PendingLockError {}

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
    /// 文件被独占锁定：新版本已停放到待替换队列，占用释放后自动应用
    FilePending { folder_id: Uuid, path: String, reason: String },
    /// 大规模误删安全熔断：单次拟删除过多文件，已被自动挂起保护（对标 GoodSync / Syncthing）
    SafetyBlocked { folder_id: Uuid, folder: String, deleted_count: usize, total_count: usize, reason: String },
    ConflictCreated { folder_id: Uuid, path: String, copy: String },
    CycleDone { folder_id: Uuid, folder: String, report: CycleReport },
    Error { folder_id: Uuid, folder: String, message: String },
}

impl SyncEvent {
    pub fn folder_id(&self) -> Uuid {
        match self {
            SyncEvent::CycleStarted { folder_id, .. } => *folder_id,
            SyncEvent::ScanDone { folder_id, .. } => *folder_id,
            SyncEvent::CycleSkipped { folder_id, .. } => *folder_id,
            SyncEvent::Planned { folder_id, .. } => *folder_id,
            SyncEvent::FileDone { folder_id, .. } => *folder_id,
            SyncEvent::FileFailed { folder_id, .. } => *folder_id,
            SyncEvent::FilePending { folder_id, .. } => *folder_id,
            SyncEvent::SafetyBlocked { folder_id, .. } => *folder_id,
            SyncEvent::ConflictCreated { folder_id, .. } => *folder_id,
            SyncEvent::CycleDone { folder_id, .. } => *folder_id,
            SyncEvent::Error { folder_id, .. } => *folder_id,
        }
    }
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
    pub paused: Arc<AtomicBool>,
}

/// 单个动作对远端 manifest 的增量变更（并行执行后统一合并）。
enum DeltaOp {
    Put(String, FileMeta),
    Remove(String),
}

/// 同步触发来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncTrigger {
    /// 用户手动点击（立即同步 / 全部立即同步）
    Manual,
    /// 用户显式请求「强制推送（本地覆盖远端）」
    ForcePush,
    /// 实时文件监听器检测到本地改动
    Watcher,
    /// 周期性定时扫描兜底（后台静默运行）
    Periodic,
}

impl SyncEngine {
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.folder.local_path)
    }

    /// 仅执行扫描与三方对比计算，不执行任何写入、上传、下载或删除（对标 GoodSync / EchoSync 差异分析）。
    pub async fn analyze(&mut self, store: &dyn RemoteStore) -> anyhow::Result<SyncPlan> {
        let scanner = Scanner::with_options(
            &self.folder.ignore_rules,
            Some(&self.db),
            self.folder.max_file_size_mb,
            self.folder.ignore_hidden,
        )?;
        let (local, _) = scanner.scan(&self.root())?;
        let baseline = self.db.load_baseline()?;

        let mut remote = store.read_manifest().await?;
        if remote.is_none() {
            if let Some(legacy) = store.read_legacy_manifest().await? {
                remote = Some(legacy);
            }
        }

        let journal = self.db.journal_load().unwrap_or_default();
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
        }

        let plan = plan::compare_with_ignore(
            &local,
            remote.as_ref(),
            &baseline,
            self.folder.direction,
            self.folder.conflict_policy,
            &|p| scanner.is_path_ignored(p),
        );
        Ok(plan)
    }

    /// 执行一轮常规同步（监听变动等场景触发）。
    pub async fn run_cycle(&mut self, store: &dyn RemoteStore) -> CycleReport {
        self.run_cycle_with_trigger(store, SyncTrigger::Watcher).await
    }

    /// 强制执行一轮同步（用户手动点击「立即同步」时触发），跳过增量快路径探测，
    /// 强制拉取 manifest 执行三方对比并刷新时间与明细。
    pub async fn run_cycle_forced(&mut self, store: &dyn RemoteStore) -> CycleReport {
        self.run_cycle_with_trigger(store, SyncTrigger::Manual).await
    }

    /// 强制推送同步（用户显式请求「本地覆盖远端」）：
    /// 临时采用 Mirror（镜像）模式，本地已删除的文件坚决从远端删除（绕过防误删熔断），
    /// 本地文件完全覆盖远端，绝不反向拉取下载任何文件。
    pub async fn run_cycle_force_push(&mut self, store: &dyn RemoteStore) -> CycleReport {
        self.run_cycle_with_trigger(store, SyncTrigger::ForcePush).await
    }

    /// 全功能同步编排主入口（支持不同触发来源的差异化静默与优化）。
    pub async fn run_cycle_with_trigger(
        &mut self,
        store: &dyn RemoteStore,
        trigger: SyncTrigger,
    ) -> CycleReport {
        let is_force_push = trigger == SyncTrigger::ForcePush;
        let force = trigger == SyncTrigger::Manual || is_force_push;
        let is_periodic = trigger == SyncTrigger::Periodic;

        if self.is_paused() && !is_force_push {
            let mut report = CycleReport::default();
            report.skipped = 1;
            return report;
        }

        let started = Instant::now();
        let mut report = CycleReport::default();
        let fid = self.folder.id;

        // 0. 本地同步目录安全保证：非用户自主创建的 .synccloud 目录一律显式设置 Windows 隐藏属性
        let local_synccloud = self.root().join(".synccloud");
        if local_synccloud.exists() {
            crate::sync::fslock::set_hidden_windows(&local_synccloud);
        }

        // 0.5 应用待替换队列：上一轮被独占锁定而未落盘的新版本，现在再试一次。
        self.apply_pending_replaces().await;

        // 1. 本地扫描（哈希缓存命中时只做 stat）+ 基线
        let scanned = (|| -> anyhow::Result<(crate::sync::types::FileTree, crate::sync::scanner::ScanStats, crate::sync::types::FileTree)> {
            let scanner = Scanner::with_options(
                &self.folder.ignore_rules,
                Some(&self.db),
                self.folder.max_file_size_mb,
                self.folder.ignore_hidden,
            )?;
            let (local, stats) = scanner.scan(&self.root())?;
            let baseline = self.db.load_baseline()?;
            Ok((local, stats, baseline))
        })();
        let (local, scan_stats, baseline) = match scanned {
            Ok(v) => v,
            Err(e) => {
                if !is_periodic {
                    self.events.emit(SyncEvent::Error {
                        folder_id: fid,
                        folder: self.folder.name.clone(),
                        message: format!("本地扫描失败：{e:#}"),
                    });
                }
                report.failed += 1;
                return report;
            }
        };

        // 2. 增量快路径判定：无实质变化且远端未变 → 整轮跳过
        let has_pending_journal = !self.db.journal_load().unwrap_or_default().is_empty();
        if !force && !tree_changed(&local, &baseline) && !has_pending_journal {
            let stamp_now = match store.manifest_stamp().await {
                Ok(s) => Some(s.unwrap_or_else(|| STAMP_ABSENT.to_string())),
                Err(_) => None,
            };
            if stamp_now.as_deref() == self.db.kv_get("manifest_stamp").as_deref() {
                report.skipped = 1;
                self.db.kv_set("last_sync_at", &chrono::Local::now().to_rfc3339());
                report.took_ms = started.elapsed().as_millis() as u64;

                // 核心优化（需求4）：周期扫描兜底且无变化时，静默完成，不发射任何无意义的跳过事件与日志！
                if !is_periodic {
                    self.events.emit(SyncEvent::CycleStarted { folder_id: fid, folder: self.folder.name.clone() });
                    self.events.emit(SyncEvent::ScanDone {
                        folder_id: fid,
                        files: scan_stats.files,
                        hashed: scan_stats.hashed,
                        cache_hits: scan_stats.hash_cache_hits,
                        ignored: scan_stats.ignored,
                    });
                    self.events.emit(SyncEvent::CycleSkipped {
                        folder_id: fid,
                        folder: self.folder.name.clone(),
                    });
                    self.events.emit(SyncEvent::CycleDone {
                        folder_id: fid,
                        folder: self.folder.name.clone(),
                        report: report.clone(),
                    });
                }
                return report;
            }
        }

        // 确实有实质变化，或手动强制触发：正式发射周期开始与扫描事件
        self.events.emit(SyncEvent::CycleStarted { folder_id: fid, folder: self.folder.name.clone() });
        self.events.emit(SyncEvent::ScanDone {
            folder_id: fid,
            files: scan_stats.files,
            hashed: scan_stats.hashed,
            cache_hits: scan_stats.hash_cache_hits,
            ignored: scan_stats.ignored,
        });

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

        let override_dir = if is_force_push {
            Some(crate::sync::types::Direction::Mirror)
        } else {
            None
        };
        let result = self
            .run_cycle_locked(store, &mut report, local, baseline, override_dir, is_force_push)
            .await;

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

    /// 应用待替换队列（文件占用兜底）。超过 7 天仍未成功的记录连同临时文件清理。
    async fn apply_pending_replaces(&mut self) {
        let pending = match self.db.pending_list() {
            Ok(p) => p,
            Err(_) => return,
        };
        if pending.is_empty() {
            return;
        }
        let root = self.root();
        let fid = self.folder.id;
        let mut applied = 0usize;
        for p in &pending {
            // 停放文件已丢失（用户清理数据目录等）：直接丢弃记录
            if !Path::new(&p.tmp_path).exists() {
                self.db.pending_remove(p.id);
                continue;
            }
            // 7 天仍无法替换：放弃（远端仍有权威版本，下轮会重新规划）
            if let Ok(created) = chrono::DateTime::parse_from_rfc3339(&p.created_at) {
                if chrono::Local::now().signed_duration_since(created) > chrono::Duration::days(7) {
                    let _ = tokio::fs::remove_file(&p.tmp_path).await;
                    self.db.pending_remove(p.id);
                    tracing::warn!(path = %p.path, "待替换文件超过 7 天未应用，已清理");
                    continue;
                }
            }
            let target = abs_path(&root, &p.path);
            match crate::sync::fslock::replace_file(Path::new(&p.tmp_path), &target).await {
                Ok(()) => {
                    self.db.pending_remove(p.id);
                    applied += 1;
                    tracing::info!(folder = %self.folder.name, path = %p.path, "占用已释放，待替换文件已应用");
                    self.events.emit(SyncEvent::FileDone {
                        folder_id: fid,
                        path: p.path.clone(),
                        action: "pending_replace".into(),
                    });
                }
                Err(e) => {
                    tracing::debug!(path = %p.path, error = %e, "目标仍被占用，待替换继续等待");
                }
            }
        }
        if applied > 0 {
            tracing::info!(folder = %self.folder.name, n = applied, "本轮应用了待替换文件");
        }
    }

    /// 持锁后的同步主体。
    async fn run_cycle_locked(
        &mut self,
        store: &dyn RemoteStore,
        report: &mut CycleReport,
        local: FileTree,
        baseline: FileTree,
        override_direction: Option<crate::sync::types::Direction>,
        bypass_deletion_guard: bool,
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

        let scanner = Scanner::with_options(
            &self.folder.ignore_rules,
            Some(&self.db),
            self.folder.max_file_size_mb,
            self.folder.ignore_hidden,
        )?;

        // 6. 三方对比（传入路径忽略判定闭包，确保排除文件绝不下载到本地，且本地删除后正确推送远端删除）
        let sync_direction = override_direction.unwrap_or(self.folder.direction);
        let mut plan: SyncPlan = plan::compare_with_ignore(
            &local,
            remote.as_ref(),
            &baseline,
            sync_direction,
            self.folder.conflict_policy,
            &|p| scanner.is_path_ignored(p),
        );
        self.events.emit(SyncEvent::Planned { folder_id: fid, stats: plan.stats.clone() });

        // 6.1 大规模误删安全熔断器（对标 GoodSync / Syncthing；强制推送模式跳过此拦截）
        let total_deletes = plan.stats.delete_local + plan.stats.delete_remote;
        let baseline_count = baseline.len();
        let mut uncompleted_remote_deletes = std::collections::HashSet::new();
        if !bypass_deletion_guard
            && self.settings.deletion_guard
            && total_deletes >= self.settings.deletion_guard_threshold as usize
            && (baseline_count > 0 && total_deletes * 100 / baseline_count >= 20)
        {
            let warn_msg = format!(
                "防误删安全熔断：本轮拟删除 {} 项（占基线 {}%），超过安全阈值（{}项）。已自动挂起删除操作，防止灾难性误删！可在差异分析与预览中核验或按需确认。",
                total_deletes,
                total_deletes * 100 / baseline_count,
                self.settings.deletion_guard_threshold
            );
            tracing::warn!(folder = %self.folder.name, "{}", warn_msg);
            self.events.emit(SyncEvent::SafetyBlocked {
                folder_id: fid,
                folder: self.folder.name.clone(),
                deleted_count: total_deletes,
                total_count: baseline_count,
                reason: warn_msg,
            });
            // 记录被熔断挂起的远端删除路径，确保基线中保留该项，避免下一轮误判为远端新文件倒灌下载！
            for a in &plan.actions {
                if let Action::DeleteRemote { path } = a {
                    uncompleted_remote_deletes.insert(path.clone());
                }
            }
            // 过滤掉所有删除动作，仅保留上传/下载/冲突处理，避免整批破坏性删除
            plan.actions.retain(|a| !matches!(a, Action::DeleteLocal { .. } | Action::DeleteRemote { .. }));
        }

        if plan.actions.is_empty() && !remote_dirty {
            // 完全无变化：记录变更戳供下轮快路径使用
            let stamp = remote
                .as_ref()
                .map(manifest_stamp_of)
                .unwrap_or_else(|| STAMP_ABSENT.to_string());
            self.db.kv_set("manifest_stamp", &stamp);
            return Ok(());
        }

        // 7. 传输优先级调度（对标 Syncthing 与 PanguFlow）：
        //    MoveLocal 优先毫秒级完成，删除次之，传输类按配置（默认小文件优先）排序执行
        sort_actions(&mut plan.actions, self.folder.transfer_priority);

        // 8. 并行执行动作（concurrency 上限；每项带指数退避重试应对瞬时文件占用）。
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
                        if engine.is_paused() {
                            return (action, Err(anyhow::anyhow!("任务已暂停")));
                        }
                        let mut attempt: u32 = 0;
                        loop {
                            if engine.is_paused() {
                                return (action, Err(anyhow::anyhow!("任务已暂停")));
                            }
                            match engine.execute_action(store, &action).await {
                                Ok(ops) => {
                                    // 传输完成立即实时上报 FileDone，保证进度条平滑递增
                                    events.emit(SyncEvent::FileDone {
                                        folder_id: fid,
                                        path: action_path(&action),
                                        action: action_kind(&action),
                                    });
                                    break (action, Ok(ops));
                                }
                                Err(e) => {
                                    // 占用挂起类：已入待处理队列，无需重试，立即交给合并阶段
                                    if e.is::<PendingLockError>() {
                                        break (action, Err(e));
                                    }
                                    if attempt >= max_retries {
                                        // 仅最终失败才上报 FileFailed（写入传输历史）；
                                        // 中间重试只记日志，避免重试期间刷屏大量失败记录
                                        events.emit(SyncEvent::FileFailed {
                                            folder_id: fid,
                                            path: action_path(&action),
                                            error: format!("{e:#}"),
                                            retry_in_secs: 0,
                                        });
                                        break (action, Err(e));
                                    }
                                    let delay =
                                        Duration::from_secs(1u64.saturating_pow(attempt + 1).min(60));
                                    tracing::warn!(
                                        path = %action_path(&action),
                                        error = %e,
                                        retry_in_secs = delay.as_secs(),
                                        "动作执行失败，指数退避后重试"
                                    );
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
                    // 下载类与本地移动动作会改动本地文件，基线需要重扫得到真实状态；
                    // 纯上传/删除轮次直接复用扫描结果，省一次全目录遍历。
                    need_rescan |= matches!(
                        action,
                        Action::Download { .. }
                            | Action::RemoteWinsOverDelete { .. }
                            | Action::Conflict { .. }
                            | Action::MoveLocal { .. }
                    );
                    if let Action::DeleteLocal { path } = &action {
                        final_local.remove(path);
                    }
                    if let Action::MoveLocal { from, to, meta } = &action {
                        final_local.remove(from);
                        final_local.insert(to.clone(), meta.clone());
                    }
                }
                Err(e) => {
                    // 若 DeleteRemote 失败，记录未完成删除路径，基线中必须保留！
                    if let Action::DeleteRemote { path } = &action {
                        uncompleted_remote_deletes.insert(path.clone());
                    }
                    if e.is::<PendingLockError>() {
                        // 占用挂起：不计失败（FilePending 事件已在动作内上报），
                        // 基线保持不变，下一轮自动重新规划收敛。
                        // 同时使增量快路径的 stamp 缓存失效：本地与远端的差异
                        // （如待删除的残留源文件）不能被「无变化」跳过掩盖。
                        self.db.kv_set("manifest_stamp", "");
                        tracing::info!(
                            folder = %self.folder.name,
                            path = %action_path(&action),
                            "文件被占用，动作挂起待下一轮重试"
                        );
                        continue;
                    }
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
        // 动作阶段若发生占用挂起（PendingLockError / MoveLocal 源残留），
        // stamp 已被置空作为标记：此处先写入正常 stamp，再恢复失效标记，
        // 确保下一轮不命中增量快路径，残留差异能被重新规划收敛
        let has_deferred = self.db.kv_get("manifest_stamp").as_deref() == Some("");
        self.db.kv_set("manifest_stamp", &stamp);
        if has_deferred {
            self.db.kv_set("manifest_stamp", "");
        }

        // 10. 基线更新：新基线 = 本地实际就位的文件 + 尚未在远端成功删除的基线项
        let mut next_baseline = if need_rescan {
            let (scanned, _) = {
                let scanner = Scanner::with_options(
                    &self.folder.ignore_rules,
                    Some(&self.db),
                    self.folder.max_file_size_mb,
                    self.folder.ignore_hidden,
                )?;
                scanner.scan(&self.root())?
            };
            scanned
        } else {
            final_local
        };

        // 关键安全保证：未完成远端删除的条目在基线中予以保留，
        // 防止基线提前丢失后下一轮被三方对比误判为「远端新文件」倒灌下载回本地！
        for path in &uncompleted_remote_deletes {
            if let Some(m) = baseline.get(path) {
                next_baseline.insert(path.clone(), m.clone());
            }
        }
        self.db.replace_baseline(&next_baseline)?;

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
                crate::sync::transfer::upload_limiter().consume(meta.size as usize).await;
                store.put_file(path, &src).await?;
                Ok(vec![DeltaOp::Put(path.clone(), meta.clone())])
            }
            Action::Download { path, meta } | Action::RemoteWinsOverDelete { path, remote: meta } => {
                let scanner = Scanner::with_options(
                    &self.folder.ignore_rules,
                    None,
                    self.folder.max_file_size_mb,
                    self.folder.ignore_hidden,
                )?;
                if scanner.is_path_ignored(path) {
                    tracing::info!(path = %path, "跳过被排除文件的下载");
                    return Ok(Vec::new());
                }
                self.download_to(store, path, &root, meta).await?;
                match action {
                    // 本地曾删除而远端被他人修改：恢复 manifest 条目
                    Action::RemoteWinsOverDelete { path, remote } => {
                        Ok(vec![DeltaOp::Put(path.clone(), remote.clone())])
                    }
                    _ => Ok(Vec::new()),
                }
            }
            Action::MoveLocal { from, to, meta } => {
                let abs_from = abs_path(&root, from);
                let abs_to = abs_path(&root, to);
                if let Some(parent) = abs_to.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                if abs_to.exists() {
                    let _ = crate::sync::recycle::archive_file(&self.folder.id, &abs_to, to, "overwritten").await;
                }
                match crate::sync::fslock::replace_file(&abs_from, &abs_to).await {
                    Ok(()) => {}
                    Err(e) if crate::sync::fslock::is_lock_error(&e) => {
                        // 源文件被独占锁定（如正被 Office 打开）：降级为共享读复制 + 健壮删除。
                        // 复制成功后目标立即就位；源删除失败时不报错，残留源文件由下一轮
                        // 重新规划为 DeleteLocal 自然收敛（最终一致，无感知、不丢数据）。
                        crate::sync::fslock::copy_shared(&abs_from, &abs_to).await?;
                        match crate::sync::fslock::remove_file_robust(&abs_from).await {
                            Ok(()) => {}
                            Err(_) if !abs_from.exists() => {}
                            Err(del_err) => {
                                self.events.emit(SyncEvent::FilePending {
                                    folder_id: self.folder.id,
                                    path: from.clone(),
                                    reason: format!(
                                        "已复制到新路径，原文件被占用（{del_err}），将在占用释放后自动删除"
                                    ),
                                });
                                // 残留源文件需下一轮重新规划删除：使快路径 stamp 失效，
                                // 避免「本地==基线」跳过掩盖本地与远端的差异
                                self.db.kv_set("manifest_stamp", "");
                            }
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
                self.prune_empty_parents(&root, Path::new(from));
                Ok(vec![
                    DeltaOp::Remove(from.clone()),
                    DeltaOp::Put(to.clone(), meta.clone()),
                ])
            }
            Action::MoveRemote { from, to, meta } => {
                // 优先尝试后端原生服务端移动（S3/WebDAV 等），不支持则回退到 put + delete
                if let Err(e) = store.move_file(from, to).await {
                    tracing::debug!(error = %e, "存储后端服务端移动不可用，回退至上传+删除");
                    let src = abs_path(&root, to);
                    crate::sync::transfer::upload_limiter().consume(meta.size as usize).await;
                    store.put_file(to, &src).await?;
                    let _ = store.delete_file(from).await;
                }
                Ok(vec![
                    DeltaOp::Remove(from.clone()),
                    DeltaOp::Put(to.clone(), meta.clone()),
                ])
            }
            Action::DeleteRemote { path } => {
                store.delete_file(path).await?;
                Ok(vec![DeltaOp::Remove(path.clone())])
            }
            Action::DeleteLocal { path } => {
                let target = abs_path(&root, path);
                if target.exists() {
                    // 被独占锁定的文件直接挂起：不归档（避免下轮重复归档）、不计失败，
                    // 占用释放后下一轮重新规划删除，最终一致
                    if !crate::sync::fslock::can_delete(&target) {
                        self.events.emit(SyncEvent::FilePending {
                            folder_id: self.folder.id,
                            path: path.clone(),
                            reason: "文件被其他程序占用，将在占用释放后自动删除".into(),
                        });
                        return Err(anyhow::Error::new(PendingLockError {
                            path: path.clone(),
                        }));
                    }
                    // 安全防误删保护（FreeFileSync / Syncthing 同款）：
                    // 本地删除时先归档到应用外部回收站，保留防误删备份与元数据索引
                    let _ = crate::sync::recycle::archive_file(&self.folder.id, &target, path, "deleted").await;
                }
                match crate::sync::fslock::remove_file_robust(&target).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) if crate::sync::fslock::is_lock_error(&e) => {
                        // 探测与删除之间的窄窗口内被锁定：已归档过，不再重复归档；
                        // 挂起等待下一轮收敛，不计失败
                        self.events.emit(SyncEvent::FilePending {
                            folder_id: self.folder.id,
                            path: path.clone(),
                            reason: "文件被其他程序占用，将在占用释放后自动删除".into(),
                        });
                        return Err(anyhow::Error::new(PendingLockError {
                            path: path.clone(),
                        }));
                    }
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
    /// 目标被其他程序占用时：优先 POSIX 语义替换（绝大多数应用可原地替换）；
    /// 仍失败则把已下载内容停放到待替换队列，占用释放后自动应用（无感知）。
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

        // 快路径：待替换队列里已有同内容版本（此前被锁定停放），直接再试一次，
        // 免去重复下载。
        if let Some(p) = self.db.pending_get(path) {
            if p.size == expected.size
                && (p.hash == expected.hash || expected.hash.is_empty())
                && Path::new(&p.tmp_path).exists()
            {
                match crate::sync::fslock::replace_file(Path::new(&p.tmp_path), &target).await {
                    Ok(()) => {
                        self.db.pending_remove(p.id);
                        return Ok(());
                    }
                    Err(_) => {
                        // 仍被占用：记录保留，直接返回（内容已安全停放）
                        self.events.emit(SyncEvent::FilePending {
                            folder_id: self.folder.id,
                            path: path.to_string(),
                            reason: "目标文件被占用，新版本已在待替换队列等待".into(),
                        });
                        return Ok(());
                    }
                }
            } else {
                // 远端又有了更新版本：清掉过期停放，重新下载
                let _ = tokio::fs::remove_file(&p.tmp_path).await;
                self.db.pending_remove(p.id);
            }
        }

        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // 临时文件置于专门的应用数据临时目录，绝不出现在同步文件夹中
        let temp_dir = crate::config::app_data_dir().join("temp");
        tokio::fs::create_dir_all(&temp_dir).await?;
        let tmp = temp_dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        store.get_file(path, &tmp).await?;
        crate::sync::transfer::download_limiter().consume(expected.size as usize).await;
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
        // 安全防误删保护：如果目标文件已存在，先安全备份至回收站再替换
        if target.exists() {
            let _ = crate::sync::recycle::archive_file(&self.folder.id, &target, path, "overwritten").await;
        }

        // 落盘：普通 rename → POSIX 语义替换（目标被共享打开也能换）→ 跨卷 staging 副本。
        // 锁定类错误（目标被独占打开）→ 停放进待替换队列，占用释放后自动应用；
        // 其余错误如实上抛，交给引擎动作重试。
        if let Err(e) = crate::sync::fslock::replace_file(&tmp, &target).await {
            if !crate::sync::fslock::is_lock_error(&e) {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(e.into());
            }
            let pending_dir = crate::config::folder_data_dir(&self.folder.id).join("pending");
            tokio::fs::create_dir_all(&pending_dir).await?;
            let parked = pending_dir.join(format!("{}.part", uuid::Uuid::new_v4()));
            if tokio::fs::rename(&tmp, &parked).await.is_err() {
                tokio::fs::copy(&tmp, &parked).await?;
                let _ = tokio::fs::remove_file(&tmp).await;
            }
            // 同路径旧停放（理论上不存在，上面的快路径已清理）防御性删除，避免孤儿文件
            if let Some(old) = self.db.pending_get(path) {
                if old.tmp_path != parked.to_string_lossy() {
                    let _ = tokio::fs::remove_file(&old.tmp_path).await;
                }
            }
            self.db.pending_add(path, &parked.to_string_lossy(), expected.size, &expected.hash);
            tracing::warn!(folder = %self.folder.name, path = path, error = %e, "目标被独占锁定，新版本已入待替换队列");
            self.events.emit(SyncEvent::FilePending {
                folder_id: self.folder.id,
                path: path.to_string(),
                reason: "目标文件被占用，新版本将在占用释放后自动应用".into(),
            });
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
        Action::MoveLocal { .. } | Action::MoveRemote { .. } => {
            report.moved += 1;
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
        Action::MoveLocal { to, .. } | Action::MoveRemote { to, .. } => to.clone(),
    }
}

fn action_kind(a: &Action) -> String {
    match a {
        Action::Upload { .. } => "upload".into(),
        Action::Download { .. } => "download".into(),
        Action::DeleteRemote { .. } => "delete_remote".into(),
        Action::DeleteLocal { .. } => "delete_local".into(),
        Action::MoveLocal { .. } => "move_local".into(),
        Action::MoveRemote { .. } => "move_remote".into(),
        Action::Identical { .. } => "identical".into(),
        Action::Conflict { .. } => "conflict".into(),
        Action::RemoteWinsOverDelete { .. } => "remote_wins".into(),
        Action::LocalWinsOverDelete { .. } => "local_wins".into(),
    }
}

/// 传输动作优先级调度排序（对标 Syncthing 与 PanguFlow）。
fn sort_actions(actions: &mut [Action], priority: crate::sync::types::TransferPriority) {
    actions.sort_by(|a, b| {
        let stage_a = action_stage(a);
        let stage_b = action_stage(b);
        if stage_a != stage_b {
            return stage_a.cmp(&stage_b);
        }
        match priority {
            crate::sync::types::TransferPriority::SmallestFirst => {
                let size_a = action_size(a);
                let size_b = action_size(b);
                size_a.cmp(&size_b).then_with(|| action_path(a).cmp(&action_path(b)))
            }
            crate::sync::types::TransferPriority::LargestFirst => {
                let size_a = action_size(a);
                let size_b = action_size(b);
                size_b.cmp(&size_a).then_with(|| action_path(a).cmp(&action_path(b)))
            }
            crate::sync::types::TransferPriority::NewestFirst => {
                let mtime_a = action_mtime(a);
                let mtime_b = action_mtime(b);
                mtime_b.cmp(&mtime_a).then_with(|| action_path(a).cmp(&action_path(b)))
            }
            crate::sync::types::TransferPriority::OrderAlpha => {
                action_path(a).cmp(&action_path(b))
            }
        }
    });
}

fn action_stage(a: &Action) -> u8 {
    match a {
        Action::MoveLocal { .. } => 0,
        Action::DeleteLocal { .. } | Action::DeleteRemote { .. } => 1,
        Action::Upload { .. }
        | Action::Download { .. }
        | Action::MoveRemote { .. }
        | Action::Conflict { .. }
        | Action::LocalWinsOverDelete { .. }
        | Action::RemoteWinsOverDelete { .. } => 2,
        Action::Identical { .. } => 3,
    }
}

fn action_size(a: &Action) -> u64 {
    match a {
        Action::Upload { meta, .. }
        | Action::Download { meta, .. }
        | Action::MoveLocal { meta, .. }
        | Action::MoveRemote { meta, .. }
        | Action::Identical { meta, .. }
        | Action::LocalWinsOverDelete { local: meta, .. }
        | Action::RemoteWinsOverDelete { remote: meta, .. } => meta.size,
        Action::Conflict { remote, .. } => remote.size,
        Action::DeleteLocal { .. } | Action::DeleteRemote { .. } => 0,
    }
}

fn action_mtime(a: &Action) -> i64 {
    match a {
        Action::Upload { meta, .. }
        | Action::Download { meta, .. }
        | Action::MoveLocal { meta, .. }
        | Action::MoveRemote { meta, .. }
        | Action::Identical { meta, .. }
        | Action::LocalWinsOverDelete { local: meta, .. }
        | Action::RemoteWinsOverDelete { remote: meta, .. } => meta.mtime_ms,
        Action::Conflict { remote, .. } => remote.mtime_ms,
        Action::DeleteLocal { .. } | Action::DeleteRemote { .. } => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendConfig;
    use crate::storage::{manifest_stamp_of, RawIo, RemoteEntry, RemoteStore};
    use crate::sync::state::StateDb;
    use crate::sync::types::{FileMeta, Manifest};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// 内存 Mock 存储：用户文件 + 元数据区两个命名空间，
    /// 覆盖 RemoteStore / RawIo 全部接口，供引擎端到端测试。
    struct MemStore {
        files: Mutex<HashMap<String, Vec<u8>>>,
        meta: Mutex<HashMap<String, String>>,
    }

    impl MemStore {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
                meta: Mutex::new(HashMap::new()),
            }
        }
        fn seed_file(&self, path: &str, content: &[u8]) -> FileMeta {
            let hash = {
                let tmp = std::env::temp_dir().join(format!(
                    "synccloud-mem-{}.seed",
                    uuid::Uuid::new_v4()
                ));
                std::fs::write(&tmp, content).unwrap();
                let h = crate::sync::scanner::hash_file(&tmp).unwrap();
                let _ = std::fs::remove_file(&tmp);
                h
            };
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), content.to_vec());
            FileMeta {
                size: content.len() as u64,
                mtime_ms: chrono::Utc::now().timestamp_millis(),
                hash,
            }
        }
        fn write_manifest_with(&self, files: Vec<(String, FileMeta)>) {
            let mut m = Manifest::new("test-remote");
            for (p, meta) in files {
                m.files.insert(p, meta);
            }
            self.meta
                .lock()
                .unwrap()
                .insert("manifest.json".into(), serde_json::to_string(&m).unwrap());
        }
    }

    #[async_trait]
    impl RemoteStore for MemStore {
        fn kind(&self) -> &'static str {
            "mem"
        }
        fn raw_io(&self) -> &dyn RawIo {
            self
        }
        async fn test(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn read_manifest(&self) -> anyhow::Result<Option<Manifest>> {
            let raw = self.meta.lock().unwrap().get("manifest.json").cloned();
            Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
        }
        async fn write_manifest(&self, manifest: &Manifest) -> anyhow::Result<()> {
            self.meta
                .lock()
                .unwrap()
                .insert("manifest.json".into(), serde_json::to_string(manifest)?);
            Ok(())
        }
        async fn put_file(&self, path: &str, src: &Path) -> anyhow::Result<()> {
            let data = std::fs::read(src)?;
            self.files.lock().unwrap().insert(path.to_string(), data);
            Ok(())
        }
        async fn get_file(&self, path: &str, dest: &Path) -> anyhow::Result<()> {
            let data = self
                .files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("远端文件不存在: {path}"))?;
            std::fs::write(dest, data)?;
            Ok(())
        }
        async fn delete_file(&self, path: &str) -> anyhow::Result<()> {
            self.files.lock().unwrap().remove(path);
            Ok(())
        }
        async fn list_objects(&self) -> anyhow::Result<Vec<RemoteEntry>> {
            Ok(self
                .files
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| RemoteEntry { key: k.clone(), size: v.len() as u64 })
                .collect())
        }
    }

    #[async_trait]
    impl RawIo for MemStore {
        async fn put_text(&self, key: &str, content: &str) -> anyhow::Result<()> {
            self.meta.lock().unwrap().insert(key.to_string(), content.to_string());
            Ok(())
        }
        async fn read_text(&self, key: &str) -> anyhow::Result<Option<String>> {
            Ok(self.meta.lock().unwrap().get(key).cloned())
        }
        async fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.meta.lock().unwrap().remove(key);
            Ok(())
        }
        async fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(self
                .meta
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }
    }

    fn test_folder(local: &Path) -> crate::config::FolderConfig {
        crate::config::FolderConfig {
            id: Uuid::new_v4(),
            name: "e2e".into(),
            local_path: local.to_string_lossy().to_string(),
            backend: BackendConfig::WebDav {
                url: "http://mem".into(),
                username: String::new(),
                password: String::new(),
                root_path: String::new(),
            },
            direction: crate::sync::types::Direction::Bidirectional,
            conflict_policy: crate::sync::types::ConflictPolicy::BothCopies,
            ignore_rules: vec![],
            sync_mode: crate::sync::types::SyncMode::Manual,
            interval_secs: 3600,
            encryption: Default::default(),
            transfer_priority: Default::default(),
            max_file_size_mb: None,
            ignore_hidden: false,
            enabled: true,
        }
    }

    /// 端到端：本地文件被独占锁定时，下载走「待替换队列」，
    /// 占用释放后的下一轮自动应用 —— 同步最终一致、无报错。
    #[tokio::test]
    async fn locked_file_syncs_via_pending_queue() {
        // 两个 e2e 测试并行运行：共享同一数据目录根（set_app_data_dir_in_memory
        // 是进程级全局），内部按各自 UUID 隔离，互不干扰。
        let data_root = std::env::temp_dir().join("synccloud-e2e-shared");
        let local = data_root.join("local-pending");
        let _ = std::fs::remove_dir_all(&local);
        std::fs::create_dir_all(&local).unwrap();
        crate::config::set_app_data_dir_in_memory(data_root.clone());

        std::fs::write(local.join("doc.txt"), b"v1-local").unwrap();

        let folder = test_folder(&local);
        let db = StateDb::open(&crate::config::folder_data_dir(&folder.id).join("state.db")).unwrap();
        let (sink, _rx) = EventSink::new();
        let mut engine = SyncEngine {
            folder: folder.clone(),
            device_id: "device-a".into(),
            device_name: "DeviceA".into(),
            settings: Default::default(),
            db,
            events: sink,
            paused: Arc::new(AtomicBool::new(false)),
        };

        // 预置基线 = 本地 v1（三方对比：本地与基线一致，远端有新版）
        {
            let meta = std::fs::metadata(local.join("doc.txt")).unwrap();
            let mut tree = crate::sync::types::FileTree::new();
            tree.insert(
                "doc.txt".into(),
                FileMeta {
                    size: meta.len(),
                    mtime_ms: meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0),
                    hash: crate::sync::scanner::hash_file(&local.join("doc.txt")).unwrap(),
                },
            );
            engine.db.replace_baseline(&tree).unwrap();
        }

        // 远端有 v2
        let store = MemStore::new();
        let remote_meta = store.seed_file("doc.txt", b"v2-remote");
        store.write_manifest_with(vec![("doc.txt".into(), remote_meta)]);

        // 以「不允许共享删除」的方式持有本地文件句柄 → 替换必然失败
        #[cfg(target_os = "windows")]
        let guard = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0x1 | 0x2) // READ | WRITE，不含 DELETE
                .open(local.join("doc.txt"))
                .unwrap()
        };

        // 第一轮：下载被阻塞，转入待替换队列（不报错、不失败）
        let report = engine.run_cycle(&store).await;
        assert_eq!(report.failed, 0, "被占用文件不应计为失败");
        assert_eq!(
            std::fs::read(local.join("doc.txt")).unwrap(),
            b"v1-local",
            "占用期间本地内容不变"
        );
        let pending = engine.db.pending_list().unwrap();
        assert_eq!(pending.len(), 1, "应有一条待替换记录");
        assert_eq!(pending[0].path, "doc.txt");
        assert!(Path::new(&pending[0].tmp_path).exists(), "停放文件应存在");

        // 释放占用
        #[cfg(target_os = "windows")]
        drop(guard);

        // 第二轮：待替换自动应用，本地与远端一致
        let report2 = engine.run_cycle(&store).await;
        assert_eq!(report2.failed, 0);
        assert_eq!(
            std::fs::read(local.join("doc.txt")).unwrap(),
            b"v2-remote",
            "占用释放后新版本应已应用"
        );
        assert!(engine.db.pending_list().unwrap().is_empty(), "待替换队列应清空");

    }

    /// 端到端：上传路径 —— 远端文件不存在而本地存在（且基线为空）时正常上传。
    #[tokio::test]
    async fn upload_flow_with_memory_store() {
        let data_root = std::env::temp_dir().join("synccloud-e2e-shared");
        let local = data_root.join("local-upload");
        let _ = std::fs::remove_dir_all(&local);
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("a.txt"), b"hello").unwrap();
        crate::config::set_app_data_dir_in_memory(data_root.clone());

        let folder = test_folder(&local);
        let db = StateDb::open(&crate::config::folder_data_dir(&folder.id).join("state.db")).unwrap();
        let (sink, _rx) = EventSink::new();
        let mut engine = SyncEngine {
            folder,
            device_id: "device-a".into(),
            device_name: "DeviceA".into(),
            settings: Default::default(),
            db,
            events: sink,
            paused: Arc::new(AtomicBool::new(false)),
        };

        let store = MemStore::new();
        store.write_manifest_with(vec![]);

        let report = engine.run_cycle(&store).await;
        assert_eq!(report.uploaded, 1, "应上传 1 个文件");
        assert_eq!(store.files.lock().unwrap().get("a.txt").unwrap(), b"hello");
        // manifest 已包含上传的文件
        let m = store.read_manifest().await.unwrap().unwrap();
        assert!(m.files.contains_key("a.txt"));
        assert!(manifest_stamp_of(&m).starts_with("v"));
    }

    fn seed_baseline_from_local(engine: &SyncEngine, local: &Path, rel: &str) {
        let p = local.join(rel);
        let meta = std::fs::metadata(&p).unwrap();
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut tree = crate::sync::types::FileTree::new();
        tree.insert(
            rel.to_string(),
            FileMeta {
                size: meta.len(),
                mtime_ms,
                hash: crate::sync::scanner::hash_file(&p).unwrap(),
            },
        );
        engine.db.replace_baseline(&tree).unwrap();
    }

    /// 端到端：远端重命名 + 本地源文件被占用（无共享删除）→
    /// MoveLocal 降级为「共享读复制 + 源挂起」，不计失败；
    /// 占用释放后下一轮源文件自动删除，最终收敛。
    #[tokio::test]
    async fn locked_source_move_degrades_to_copy() {
        let data_root = std::env::temp_dir().join("synccloud-e2e-shared");
        let local = data_root.join("local-move");
        let _ = std::fs::remove_dir_all(&local);
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("old.txt"), b"shared-content").unwrap();
        crate::config::set_app_data_dir_in_memory(data_root.clone());

        let folder = test_folder(&local);
        let db = StateDb::open(&crate::config::folder_data_dir(&folder.id).join("state.db")).unwrap();
        let (sink, _rx) = EventSink::new();
        let mut engine = SyncEngine {
            folder,
            device_id: "device-a".into(),
            device_name: "DeviceA".into(),
            settings: Default::default(),
            db,
            events: sink,
            paused: Arc::new(AtomicBool::new(false)),
        };
        seed_baseline_from_local(&engine, &local, "old.txt");

        // 远端：old.txt 已重命名为 new.txt（内容相同）
        let store = MemStore::new();
        let meta = store.seed_file("new.txt", b"shared-content");
        store.write_manifest_with(vec![("new.txt".into(), meta)]);

        // 以「允许共享读写但不允许共享删除」的方式持有源文件 → rename/POSIX 替换必败
        #[cfg(target_os = "windows")]
        let guard = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0x1 | 0x2) // READ | WRITE，不含 DELETE
                .open(local.join("old.txt"))
                .unwrap()
        };

        // 第一轮：降级为共享读复制，目标就位，源挂起不计失败
        let report = engine.run_cycle(&store).await;
        assert_eq!(report.failed, 0, "源被占用的移动不应计为失败");
        assert_eq!(
            std::fs::read(local.join("new.txt")).unwrap(),
            b"shared-content",
            "降级复制后目标应就位"
        );
        assert!(local.join("old.txt").exists(), "占用期间源文件保留");
        let m = store.read_manifest().await.unwrap().unwrap();
        assert!(m.files.contains_key("new.txt"), "manifest 应含新路径");
        assert!(!m.files.contains_key("old.txt"), "manifest 不应再含旧路径");

        // 释放占用
        #[cfg(target_os = "windows")]
        drop(guard);

        // 第二轮：残留源文件重新规划为 DeleteLocal，占用释放后删除收敛
        let report2 = engine.run_cycle(&store).await;
        assert_eq!(report2.failed, 0);
        assert!(!local.join("old.txt").exists(), "释放后源文件应被删除");
        assert_eq!(
            std::fs::read(local.join("new.txt")).unwrap(),
            b"shared-content"
        );
    }

    /// 端到端：远端已删 + 本地被独占锁定 → DeleteLocal 挂起不计失败，
    /// 占用释放后下一轮自动删除，最终收敛。
    #[tokio::test]
    async fn locked_delete_defers_without_failure() {
        let data_root = std::env::temp_dir().join("synccloud-e2e-shared");
        let local = data_root.join("local-del");
        let _ = std::fs::remove_dir_all(&local);
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("doomed.txt"), b"to-be-deleted").unwrap();
        crate::config::set_app_data_dir_in_memory(data_root.clone());

        let folder = test_folder(&local);
        let db = StateDb::open(&crate::config::folder_data_dir(&folder.id).join("state.db")).unwrap();
        let (sink, _rx) = EventSink::new();
        let mut engine = SyncEngine {
            folder,
            device_id: "device-a".into(),
            device_name: "DeviceA".into(),
            settings: Default::default(),
            db,
            events: sink,
            paused: Arc::new(AtomicBool::new(false)),
        };
        seed_baseline_from_local(&engine, &local, "doomed.txt");

        // 远端：doomed.txt 已被删除
        let store = MemStore::new();
        store.write_manifest_with(vec![]);

        // 以「允许共享读写但不允许共享删除」的方式持有文件
        #[cfg(target_os = "windows")]
        let guard = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0x1 | 0x2)
                .open(local.join("doomed.txt"))
                .unwrap()
        };

        // 第一轮：删除挂起（不计失败、不归档重复），文件保留
        let report = engine.run_cycle(&store).await;
        assert_eq!(report.failed, 0, "被占用的删除不应计为失败");
        assert!(local.join("doomed.txt").exists(), "占用期间文件应保留");

        // 释放占用
        #[cfg(target_os = "windows")]
        drop(guard);

        // 第二轮：重新规划删除并成功，回收站有归档
        let report2 = engine.run_cycle(&store).await;
        assert_eq!(report2.failed, 0);
        assert!(!local.join("doomed.txt").exists(), "释放后文件应被删除");
    }

    #[tokio::test]
    async fn test_pause_halts_cycle() {
        let data_root = std::env::temp_dir().join("synccloud-test-pause");
        let local = data_root.join("local-pause");
        let _ = std::fs::remove_dir_all(&local);
        std::fs::create_dir_all(&local).unwrap();
        crate::config::set_app_data_dir_in_memory(data_root.clone());
        std::fs::write(local.join("file.txt"), b"data").unwrap();

        let folder = test_folder(&local);
        let db = StateDb::open(&crate::config::folder_data_dir(&folder.id).join("state.db")).unwrap();
        let (sink, _rx) = EventSink::new();
        let paused = Arc::new(AtomicBool::new(true)); // 初始即为暂停状态
        let mut engine = SyncEngine {
            folder,
            device_id: "device-pause".into(),
            device_name: "DevicePause".into(),
            settings: Default::default(),
            db,
            events: sink,
            paused: paused.clone(),
        };

        let store = MemStore::new();
        let r = engine.run_cycle(&store).await;
        assert_eq!(r.skipped, 1, "处于暂停状态的引擎执行 run_cycle 应当直接跳过");
        assert_eq!(r.uploaded, 0);

        // 恢复后可正常同步
        paused.store(false, Ordering::Relaxed);
        let r2 = engine.run_cycle(&store).await;
        assert_eq!(r2.uploaded, 1);
    }

    #[tokio::test]
    async fn test_force_push_overwrites_remote_bypassing_guard() {
        let data_root = std::env::temp_dir().join("synccloud-test-force-push");
        let local = data_root.join("local-fp");
        let _ = std::fs::remove_dir_all(&local);
        std::fs::create_dir_all(&local).unwrap();
        crate::config::set_app_data_dir_in_memory(data_root.clone());

        // 本地只保留 1 个文件
        std::fs::write(local.join("keep.txt"), b"local-content").unwrap();

        let folder = test_folder(&local);
        let db = StateDb::open(&crate::config::folder_data_dir(&folder.id).join("state.db")).unwrap();
        let (sink, _rx) = EventSink::new();
        let mut engine = SyncEngine {
            folder,
            device_id: "device-fp".into(),
            device_name: "DeviceFP".into(),
            settings: GlobalSettings {
                deletion_guard: true,
                deletion_guard_threshold: 10, // 阈值设小方便测试
                ..Default::default()
            },
            db,
            events: sink,
            paused: Arc::new(AtomicBool::new(false)),
        };

        // 模拟远端与旧基线有 60 个文件（其中 59 个在本地已删除）
        let mut old_tree = crate::sync::types::FileTree::new();
        let mut remote_files = Vec::new();
        for i in 0..60 {
            let name = format!("file_{i}.txt");
            let m = FileMeta { size: 10, mtime_ms: 100, hash: format!("h{i}") };
            old_tree.insert(name.clone(), m.clone());
            remote_files.push((name, m));
        }
        engine.db.replace_baseline(&old_tree).unwrap();

        let store = MemStore::new();
        store.write_manifest_with(remote_files);

        // 1. 普通同步：由于删除了 60 个文件，触发安全熔断，远端删除被拦截挂起，但本地新增文件可正常上传
        let r_normal = engine.run_cycle(&store).await;
        assert_eq!(r_normal.deleted_remote, 0, "普通同步应被安全防误删拦截");
        assert_eq!(r_normal.uploaded, 1, "普通同步先行上传了本地新文件");

        // 2. 强制推送：用户显式授权覆盖远端，绕过熔断，将远端 60 个文件彻底删除
        let r_forced = engine.run_cycle_force_push(&store).await;
        assert_eq!(r_forced.deleted_remote, 60, "强制推送应成功删除远端 60 项");

        // 验证远端 manifest 只剩下本地保留的这 1 项
        let m = store.read_manifest().await.unwrap().unwrap();
        assert_eq!(m.files.len(), 1);
        assert!(m.files.contains_key("keep.txt"));
    }
}
