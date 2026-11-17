//! 每文件夹一个串行任务；停止等待在途提交完成，命令受理不等待网络。
use crate::config::{self, folder_data_dir, AppConfig, FolderConfig, GlobalSettings};
use crate::storage::{self, RemoteStore};
use crate::sync::{
    engine::{EventSink, SyncEngine, SyncEvent, SyncTrigger},
    state::StateDb,
    types::CycleReport,
    watcher,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderStatus {
    pub folder_id: Uuid,
    pub state: String,
    pub last_sync_at: Option<String>,
    pub last_error: Option<String>,
    pub last_report: Option<CycleReport>,
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn fixture() -> (FolderManager, Uuid, mpsc::Receiver<FolderCmd>) {
        let manager = FolderManager::new();
        let id = Uuid::new_v4();
        let (tx, rx) = mpsc::channel(4);
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let (settings, _) = watch::channel(GlobalSettings::default());
        let task = tokio::spawn(async move {
            stopped.cancelled().await;
        });
        manager.folders.lock().await.insert(
            id,
            FolderHandle {
                cmd_tx: tx,
                status: Arc::new(Mutex::new(FolderStatus {
                    folder_id: id,
                    state: "paused".into(),
                    last_sync_at: None,
                    last_error: None,
                    last_report: None,
                })),
                paused: Arc::new(AtomicBool::new(true)),
                pending: [
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(AtomicBool::new(false)),
                ],
                stop,
                task,
                settings,
            },
        );
        (manager, id, rx)
    }
    #[tokio::test]
    async fn enqueue_resumes_and_coalesces_without_waiting_for_network() {
        let (manager, id, mut rx) = fixture().await;
        assert_eq!(manager.sync_now(&id).await.unwrap().disposition, "queued");
        assert!(!manager.folders.lock().await[&id]
            .paused
            .load(Ordering::SeqCst));
        assert!(matches!(rx.try_recv(), Ok(FolderCmd::SyncNow)));
        assert_eq!(
            manager.sync_now(&id).await.unwrap().disposition,
            "already_queued"
        );
        assert_eq!(manager.statuses().await[0].state, "queued");
        manager.stop_folder(&id).await;
        assert!(manager.statuses().await.is_empty());
    }
    #[tokio::test]
    async fn closed_or_full_queue_does_not_report_success_or_unpause() {
        let (manager, id, rx) = fixture().await;
        {
            let folders = manager.folders.lock().await;
            for _ in 0..4 {
                folders[&id].cmd_tx.try_send(FolderCmd::SyncNow).unwrap();
            }
        }
        assert!(manager.sync_now(&id).await.is_err());
        assert_eq!(manager.statuses().await[0].state, "paused");
        assert!(manager.folders.lock().await[&id]
            .paused
            .load(Ordering::SeqCst));
        drop(rx);
        assert!(manager.sync_now(&id).await.is_err());
        manager.stop_folder(&id).await;
        assert!(manager.sync_now(&id).await.is_err());
    }
    #[tokio::test]
    async fn queued_request_resumes_after_pause() {
        let (manager, id, _rx) = fixture().await;
        manager.sync_now(&id).await.unwrap();
        manager.pause_folder(&id).await;
        manager.resume_folder(&id).await.unwrap();
        assert!(!manager.folders.lock().await[&id]
            .paused
            .load(Ordering::SeqCst));
        assert_eq!(manager.statuses().await[0].state, "queued");
        manager.stop_folder(&id).await;
    }
}

async fn build_store_with_cache(
    folder: &FolderConfig,
    data_dir: &std::path::Path,
) -> anyhow::Result<Box<dyn RemoteStore>> {
    folder.backend.validate()?;
    let mut backend = folder.backend.clone();
    backend.resolve_credentials()?;
    match &backend {
        crate::config::BackendConfig::Git {
            remote_url,
            branch,
            username,
            password,
        } => Ok(Box::new(
            crate::storage::git_backend::GitStore::new(
                remote_url.clone(),
                branch.clone(),
                username.clone(),
                password.clone(),
            )
            .with_cache_dir(data_dir.join("git-cache")),
        )),
        other => storage::build_store(other).await,
    }
}

fn device_info() -> (String, String) {
    let cfg = config::AppConfig::load_or_create(&config::app_data_dir());
    match cfg {
        Ok(c) => (c.device_id, c.device_name),
        Err(e) => {
            tracing::error!(error=%e,"读取设备信息失败");
            ("unknown-device".into(), "Device".into())
        }
    }
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncReceipt {
    pub folder_id: Uuid,
    pub disposition: &'static str,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Copy)]
pub enum FolderCmd {
    SyncNow,
    ForcePush,
}
struct FolderHandle {
    cmd_tx: mpsc::Sender<FolderCmd>,
    status: Arc<Mutex<FolderStatus>>,
    paused: Arc<AtomicBool>,
    pending: [Arc<AtomicBool>; 2],
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    settings: watch::Sender<GlobalSettings>,
}
pub struct FolderManager {
    folders: Mutex<HashMap<Uuid, FolderHandle>>,
    lifecycle: Mutex<()>,
    pub event_tx: tokio::sync::broadcast::Sender<SyncEvent>,
}
impl FolderManager {
    pub fn new() -> Self {
        let (event_tx, _) = tokio::sync::broadcast::channel(1024);
        Self {
            folders: Mutex::new(HashMap::new()),
            lifecycle: Mutex::new(()),
            event_tx,
        }
    }
    pub async fn start_all(&self, cfg: &AppConfig) {
        for folder in &cfg.folders {
            if folder.enabled {
                if let Err(e) = self.start_folder(folder, &cfg.settings).await {
                    tracing::error!(folder=%folder.name, error=%e, "启动同步任务失败");
                }
            }
        }
    }
    pub async fn start_folder(
        &self,
        folder: &FolderConfig,
        settings: &GlobalSettings,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !crate::sync::MAINTENANCE.load(Ordering::SeqCst),
            "数据迁移后请重启程序"
        );
        let _lifecycle = self.lifecycle.lock().await;
        let mut folders = self.folders.lock().await;
        if folders
            .get(&folder.id)
            .is_some_and(|h| !h.task.is_finished())
        {
            return Ok(());
        }
        folders.remove(&folder.id);
        let folder = folder.clone();
        let data_dir = folder_data_dir(&folder.id);
        let db = StateDb::open(&data_dir.join("state.db"))?;
        crate::shell_menu::register_sync_root(
            &folder.id.to_string(),
            &folder.name,
            &folder.local_path,
        );
        let (cmd_tx, mut commands) = mpsc::channel(4);
        let (settings_tx, mut settings_rx) = watch::channel(settings.clone());
        let paused = Arc::new(AtomicBool::new(false));
        let pending = [
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ];
        let stop = CancellationToken::new();
        let status = Arc::new(Mutex::new(FolderStatus {
            folder_id: folder.id,
            state: "queued".into(),
            last_sync_at: None,
            last_error: None,
            last_report: None,
        }));
        let (sink, mut events) = EventSink::new();
        let status_events = status.clone();
        let paused_events = paused.clone();
        let pending_events = pending.clone();
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                let mut s = status_events.lock().await;
                match &ev {
                    SyncEvent::CycleStarted { .. } => {
                        if !paused_events.load(Ordering::SeqCst) {
                            s.state = "syncing".into();
                        }
                        s.last_error = None;
                    }
                    SyncEvent::Error { message, .. } => {
                        s.last_error = Some(message.clone());
                        s.state = "error".into();
                        tracing::error!(error=%message,"同步轮次失败");
                    }
                    SyncEvent::SafetyBlocked { reason, .. } => {
                        s.state = "blocked".into();
                        s.last_error = Some(reason.clone());
                    }
                    SyncEvent::CycleDone { report, .. } => {
                        if report.failed > 0 {
                            s.state = "error".into();
                            s.last_error =
                                Some(format!("本轮 {} 项失败，请查看传输历史", report.failed));
                        } else if report.pending > 0 {
                            s.state = "pending".into();
                            s.last_error =
                                Some(format!("{} 项等待文件占用释放后重试", report.pending));
                        } else if s.state != "blocked" {
                            s.state = "idle".into();
                            s.last_error = None;
                        }
                        s.last_sync_at = Some(chrono::Local::now().to_rfc3339());
                        s.last_report = Some(report.clone());
                        if pending_events.iter().any(|p| p.load(Ordering::SeqCst)) {
                            s.state = "queued".into();
                        }
                    }
                    _ => {}
                }
                if paused_events.load(Ordering::SeqCst) {
                    s.state = "paused".into();
                }
                drop(s);
                let _ = event_tx.send(ev);
            }
        });
        let paused_task = paused.clone();
        let pending_task = pending.clone();
        let stop_task = stop.clone();
        let folder_task = folder.clone();
        let status_task = status.clone();
        let task = tokio::spawn(async move {
            let info = device_info();
            let mut engine = SyncEngine {
                folder: folder_task.clone(),
                device_id: info.0,
                device_name: info.1,
                settings: settings_rx.borrow().clone(),
                db,
                events: sink.clone(),
                paused: paused_task.clone(),
            };
            let mut store: Option<Box<dyn RemoteStore>> = None;
            let mut watcher = if folder_task.sync_mode == crate::sync::types::SyncMode::Realtime {
                match watcher::watch(folder_task.local_path.clone().into()) {
                    Ok(w) => Some(w),
                    Err(e) => {
                        sink.emit(SyncEvent::Error {
                            folder_id: folder_task.id,
                            folder: folder_task.name.clone(),
                            message: format!("文件监控启动失败，将使用定时扫描: {e:#}"),
                        });
                        None
                    }
                }
            } else {
                None
            };
            let mut first = true;
            loop {
                let seconds = match folder_task.sync_mode {
                    crate::sync::types::SyncMode::Realtime => settings_rx
                        .borrow()
                        .full_scan_interval_min
                        .saturating_mul(60)
                        .max(1),
                    crate::sync::types::SyncMode::Interval => folder_task.interval_secs.max(30),
                    crate::sync::types::SyncMode::Manual => 31536000,
                };
                let trigger = if first {
                    first = false;
                    SyncTrigger::Watcher
                } else {
                    tokio::select! { biased;
                        _=stop_task.cancelled()=>break,
                        command=commands.recv()=>match command {
                            Some(FolderCmd::SyncNow)=>{ pending_task[0].store(false,Ordering::SeqCst); SyncTrigger::Manual },
                            Some(FolderCmd::ForcePush)=>{ pending_task[1].store(false,Ordering::SeqCst); SyncTrigger::ForcePush },
                            None=>break,
                        },
                        changed=settings_rx.changed()=>{ if changed.is_err(){break;} continue; },
                        event=async { match watcher.as_mut(){Some(w)=>w.trigger.recv().await,None=>std::future::pending().await} }, if !paused_task.load(Ordering::SeqCst)=>{
                            if event.is_none(){watcher=None;continue;} SyncTrigger::Watcher
                        },
                        _=tokio::time::sleep(Duration::from_secs(seconds)), if !paused_task.load(Ordering::SeqCst)=>SyncTrigger::Periodic,
                    }
                };
                if stop_task.is_cancelled() {
                    break;
                }
                if paused_task.load(Ordering::SeqCst) {
                    continue;
                }
                engine.settings = settings_rx.borrow().clone();
                engine.device_name = device_info().1;
                if store.is_none() {
                    match build_store_with_cache(&folder_task, &data_dir).await {
                        Ok(s) => store = Some(s),
                        Err(e) => {
                            sink.emit(SyncEvent::Error {
                                folder_id: folder_task.id,
                                folder: folder_task.name.clone(),
                                message: format!("初始化存储后端失败: {e:#}"),
                            });
                            continue;
                        }
                    }
                }
                engine
                    .run_cycle_with_trigger(store.as_ref().unwrap().as_ref(), trigger)
                    .await;
            }
            status_task.lock().await.state = "paused".into();
        });
        folders.insert(
            folder.id,
            FolderHandle {
                cmd_tx,
                status,
                paused,
                pending,
                stop,
                task,
                settings: settings_tx,
            },
        );
        Ok(())
    }
    pub async fn analyze(
        &self,
        folder: &FolderConfig,
        settings: &GlobalSettings,
    ) -> anyhow::Result<crate::sync::plan::SyncPlan> {
        let data_dir = folder_data_dir(&folder.id);
        let store = build_store_with_cache(folder, &data_dir).await?;
        let db = StateDb::open(&data_dir.join("state.db"))?;
        let device_info = device_info();
        let (sink, _rx) = EventSink::new();
        let mut engine = SyncEngine {
            folder: folder.clone(),
            device_id: device_info.0,
            device_name: device_info.1,
            settings: settings.clone(),
            db,
            events: sink,
            paused: Arc::new(AtomicBool::new(false)),
        };
        engine.analyze(store.as_ref()).await
    }

    pub async fn stop_folder(&self, id: &Uuid) {
        let _lifecycle = self.lifecycle.lock().await;
        let handle = self.folders.lock().await.remove(id);
        if let Some(h) = handle {
            h.paused.store(true, Ordering::SeqCst);
            h.stop.cancel();
            drop(h.cmd_tx);
            if let Err(e) = h.task.await {
                tracing::error!(error=%e,"同步任务异常退出");
            }
        }
    }
    async fn enqueue(&self, id: &Uuid, force: bool) -> Result<SyncReceipt, String> {
        let folders = self.folders.lock().await;
        let h = folders.get(id).ok_or("同步任务不存在或未启用")?;
        if h.task.is_finished() || h.cmd_tx.is_closed() {
            return Err("同步任务已退出，请重新启用任务".into());
        }
        let index = usize::from(force);
        let mut status = h.status.lock().await;
        if crate::sync::MAINTENANCE.load(Ordering::SeqCst) {
            return Err("数据迁移后请重启程序".into());
        }
        let was_paused = h.paused.swap(false, Ordering::SeqCst);
        if h.pending[index].swap(true, Ordering::SeqCst) {
            if status.state != "syncing" {
                status.state = "queued".into();
            }
            return Ok(SyncReceipt {
                folder_id: *id,
                disposition: "already_queued",
                error: None,
            });
        }
        if let Err(e) = h.cmd_tx.try_send(if force {
            FolderCmd::ForcePush
        } else {
            FolderCmd::SyncNow
        }) {
            h.paused.store(was_paused, Ordering::SeqCst);
            h.pending[index].store(false, Ordering::SeqCst);
            return Err(format!("同步请求未受理: {e}"));
        }
        if status.state != "syncing" {
            status.state = "queued".into();
        }
        Ok(SyncReceipt {
            folder_id: *id,
            disposition: "queued",
            error: None,
        })
    }
    pub async fn sync_now(&self, id: &Uuid) -> Result<SyncReceipt, String> {
        self.enqueue(id, false).await
    }
    pub async fn force_push(&self, id: &Uuid) -> Result<SyncReceipt, String> {
        self.enqueue(id, true).await
    }
    pub async fn pause_folder(&self, id: &Uuid) {
        if let Some(h) = self.folders.lock().await.get(id) {
            h.paused.store(true, Ordering::SeqCst);
            h.status.lock().await.state = "paused".into();
        }
    }
    pub async fn resume_folder(&self, id: &Uuid) -> Result<(), String> {
        self.sync_now(id).await.map(|_| ())
    }
    pub async fn pause_all(&self) {
        for s in self.statuses().await {
            self.pause_folder(&s.folder_id).await;
        }
    }
    pub async fn resume_all(&self) {
        for s in self.statuses().await {
            if let Err(e) = self.resume_folder(&s.folder_id).await {
                tracing::error!(error=%e,"恢复同步失败");
            }
        }
    }
    pub async fn is_all_paused(&self) -> bool {
        let statuses = self.statuses().await;
        !statuses.is_empty() && statuses.iter().all(|s| s.state == "paused")
    }
    pub async fn statuses(&self) -> Vec<FolderStatus> {
        let folders = self.folders.lock().await;
        let mut statuses = Vec::new();
        for h in folders.values() {
            let mut s = h.status.lock().await.clone();
            if h.task.is_finished() {
                s.state = "error".into();
                s.last_error = Some("同步任务异常退出，请重新启用".into());
            }
            statuses.push(s);
        }
        statuses.sort_by_key(|s| s.folder_id);
        statuses
    }
    pub async fn apply_settings(&self, settings: &GlobalSettings) {
        for h in self.folders.lock().await.values() {
            h.settings.send_replace(settings.clone());
        }
    }
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<SyncEvent> {
        self.event_tx.subscribe()
    }
}
