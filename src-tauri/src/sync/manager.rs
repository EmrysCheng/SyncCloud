//! 多文件夹任务管理：每个同步文件夹一个独立的 tokio 任务（引擎 + watcher + 命令循环），
//! 互不干扰，支持暂停/恢复/立即同步/停止。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::{self, folder_data_dir, AppConfig, FolderConfig, GlobalSettings};
use crate::storage::{self, RemoteStore};
use crate::sync::engine::{EventSink, SyncEngine, SyncEvent};
use crate::sync::state::StateDb;
use crate::sync::types::CycleReport;
use crate::sync::watcher;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderStatus {
    pub folder_id: Uuid,
    pub state: String, // idle | syncing | paused | error
    pub last_sync_at: Option<String>,
    pub last_error: Option<String>,
    pub last_report: Option<CycleReport>,
}

#[derive(Clone)]
pub enum FolderCmd {
    SyncNow,
    ForcePush,
    Pause,
    Resume,
}

struct FolderHandle {
    cmd_tx: tokio::sync::mpsc::Sender<FolderCmd>,
    status: Arc<Mutex<FolderStatus>>,
    #[allow(dead_code)]
    paused: Arc<AtomicBool>,
}

pub struct FolderManager {
    /// 文件夹任务句柄（含命令通道与状态），仅管理器内部使用
    folders: Mutex<HashMap<Uuid, FolderHandle>>,
    pub event_tx: tokio::sync::broadcast::Sender<SyncEvent>,
}

impl FolderManager {
    pub fn new() -> Self {
        let (event_tx, _) = tokio::sync::broadcast::channel(1024);
        Self {
            folders: Mutex::new(HashMap::new()),
            event_tx,
        }
    }

    /// 应用启动时为所有已启用文件夹启动任务。
    pub async fn start_all(&self, cfg: &AppConfig) {
        for folder in &cfg.folders {
            if folder.enabled {
                if let Err(e) = self.start_folder(folder, &cfg.settings).await {
                    tracing::error!(folder = %folder.name, error = %e, "启动同步任务失败");
                }
            }
        }
    }

    pub async fn start_folder(
        &self,
        folder: &FolderConfig,
        settings: &GlobalSettings,
    ) -> anyhow::Result<()> {
        let mut folders = self.folders.lock().await;
        if folders.contains_key(&folder.id) {
            return Ok(());
        }
        let folder = folder.clone();

        let data_dir = folder_data_dir(&folder.id);
        std::fs::create_dir_all(&data_dir)?;
        let db = StateDb::open(&data_dir.join("state.db"))?;

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<FolderCmd>(16);
        let paused = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(FolderStatus {
            folder_id: folder.id,
            state: "idle".into(),
            last_sync_at: None,
            last_error: None,
            last_report: None,
        }));

        let status_task = status.clone();
        let paused_task = paused.clone();
        let event_tx = self.event_tx.clone();

        // 事件转发：引擎事件 → broadcast（UI 订阅）+ 状态更新
        let (sink, mut sink_rx) = EventSink::new();
        let forward_folder = folder.clone();
        let paused_event = paused.clone();
        tokio::spawn(async move {
            while let Some(ev) = sink_rx.recv().await {
                // 状态机推进
                match &ev {
                    SyncEvent::CycleStarted { .. } => {
                        let mut s = status_task.lock().await;
                        if !paused_event.load(Ordering::Relaxed) {
                            s.state = "syncing".into();
                        }
                    }
                    SyncEvent::Error { message, .. } => {
                        let mut s = status_task.lock().await;
                        s.state = if s.state == "paused" || paused_event.load(Ordering::Relaxed) {
                            "paused".into()
                        } else {
                            "error".into()
                        };
                        s.last_error = Some(message.clone());
                    }
                    SyncEvent::CycleDone { report, .. } => {
                        let mut s = status_task.lock().await;
                        if s.state != "paused" && !paused_event.load(Ordering::Relaxed) {
                            s.state = "idle".into();
                        }
                        s.last_sync_at = Some(chrono::Local::now().to_rfc3339());
                        s.last_report = Some(report.clone());
                        s.last_error = None;
                    }
                    _ => {}
                }
                let _ = event_tx.send(ev);
            }
        });

        // 文件夹主任务
        let folder_for_task = forward_folder;
        let status_task = status.clone();
        let settings = settings.clone();
        tokio::spawn(async move {
            let cfg_snap = settings;

            // 构造存储后端（Git 后端注入缓存目录）
            let store: Box<dyn RemoteStore> = match build_store_with_cache(&folder_for_task, &data_dir).await {
                Ok(s) => s,
                Err(e) => {
                    sink.emit(SyncEvent::Error {
                        folder_id: folder_for_task.id,
                        folder: folder_for_task.name.clone(),
                        message: format!("初始化存储后端失败: {e:#}"),
                    });
                    return;
                }
            };

            // watcher（realtime 模式）
            let mut watch = if folder_for_task.sync_mode == crate::sync::types::SyncMode::Realtime {
                watcher::watch(std::path::PathBuf::from(&folder_for_task.local_path)).ok()
            } else {
                None
            };

            // 周期兜底 / interval 模式计时器
            let interval_secs = match folder_for_task.sync_mode {
                crate::sync::types::SyncMode::Realtime => cfg_snap.full_scan_interval_min * 60,
                crate::sync::types::SyncMode::Interval => folder_for_task.interval_secs.max(30),
                crate::sync::types::SyncMode::Manual => 3600 * 24 * 365,
            };
            let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
            // interval 的第一个 tick 立即触发；启动同步已在下方执行过一次，
            // 先消费掉首个 tick，避免每次启动连续跑两轮完整同步
            ticker.tick().await;

            let device_info = device_info();
            let mut engine = SyncEngine {
                folder: folder_for_task.clone(),
                device_id: device_info.0,
                device_name: device_info.1,
                settings: cfg_snap,
                db,
                events: sink.clone(),
                paused: paused_task.clone(),
            };

            // 启动即同步一次
            engine.run_cycle(store.as_ref()).await;

            loop {
                let is_paused = paused_task.load(Ordering::Relaxed);
                tokio::select! {
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(FolderCmd::SyncNow) => {
                                // 用户显式点击「立即同步」，强制执行全量检查，若原先为暂停则自动恢复
                                if paused_task.load(Ordering::Relaxed) {
                                    paused_task.store(false, Ordering::Relaxed);
                                    status_task.lock().await.state = "idle".into();
                                }
                                engine.run_cycle_forced(store.as_ref()).await;
                            }
                            Some(FolderCmd::ForcePush) => {
                                // 用户显式点击「强制推送」，以本地完全覆盖远端
                                if paused_task.load(Ordering::Relaxed) {
                                    paused_task.store(false, Ordering::Relaxed);
                                    status_task.lock().await.state = "idle".into();
                                }
                                engine.run_cycle_force_push(store.as_ref()).await;
                            }
                            Some(FolderCmd::Pause) => {
                                paused_task.store(true, Ordering::Relaxed);
                                status_task.lock().await.state = "paused".into();
                            }
                            Some(FolderCmd::Resume) => {
                                paused_task.store(false, Ordering::Relaxed);
                                status_task.lock().await.state = "idle".into();
                            }
                            None => break,
                        }
                    }
                    triggered = async {
                        match watch.as_mut() {
                            Some(w) => w.trigger.recv().await,
                            None => std::future::pending::<Option<()>>().await,
                        }
                    }, if !is_paused => {
                        if triggered.is_some()
                            && engine.folder.sync_mode == crate::sync::types::SyncMode::Realtime
                        {
                            engine.run_cycle_with_trigger(store.as_ref(), crate::sync::engine::SyncTrigger::Watcher).await;
                        }
                    }
                    _ = ticker.tick(), if !is_paused => {
                        engine.run_cycle_with_trigger(store.as_ref(), crate::sync::engine::SyncTrigger::Periodic).await;
                    }
                }
            }
        });

        folders.insert(
            folder.id,
            FolderHandle { cmd_tx, status, paused },
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
        let mut folders = self.folders.lock().await;
        folders.remove(id); // drop cmd_tx → 任务循环退出
    }

    pub async fn sync_now(&self, id: &Uuid) -> bool {
        let folders = self.folders.lock().await;
        if let Some(h) = folders.get(id) {
            // 用户主动点击同步：自动解除暂停并给前端即时的 syncing 状态反馈
            h.paused.store(false, Ordering::Relaxed);
            h.status.lock().await.state = "syncing".into();
            h.cmd_tx.try_send(FolderCmd::SyncNow).is_ok()
        } else {
            false
        }
    }

    /// 强制推送同步：以本地当前状态强制完全覆盖远端。
    pub async fn force_push(&self, id: &Uuid) -> bool {
        let folders = self.folders.lock().await;
        if let Some(h) = folders.get(id) {
            h.paused.store(false, Ordering::Relaxed);
            h.status.lock().await.state = "syncing".into();
            h.cmd_tx.try_send(FolderCmd::ForcePush).is_ok()
        } else {
            false
        }
    }

    pub async fn pause_folder(&self, id: &Uuid) {
        let folders = self.folders.lock().await;
        if let Some(h) = folders.get(id) {
            h.paused.store(true, Ordering::Relaxed);
            h.status.lock().await.state = "paused".into();
            let _ = h.cmd_tx.try_send(FolderCmd::Pause);
        }
    }

    pub async fn resume_folder(&self, id: &Uuid) {
        let folders = self.folders.lock().await;
        if let Some(h) = folders.get(id) {
            h.paused.store(false, Ordering::Relaxed);
            h.status.lock().await.state = "idle".into();
            let _ = h.cmd_tx.try_send(FolderCmd::Resume);
        }
    }

    /// 全局暂停所有同步任务（对标 Syncthing / Resilio Sync）。
    pub async fn pause_all(&self) {
        let folders = self.folders.lock().await;
        for h in folders.values() {
            h.paused.store(true, Ordering::Relaxed);
            h.status.lock().await.state = "paused".into();
            let _ = h.cmd_tx.try_send(FolderCmd::Pause);
        }
    }

    /// 全局恢复所有同步任务。
    pub async fn resume_all(&self) {
        let folders = self.folders.lock().await;
        for h in folders.values() {
            h.paused.store(false, Ordering::Relaxed);
            h.status.lock().await.state = "idle".into();
            let _ = h.cmd_tx.try_send(FolderCmd::Resume);
        }
    }

    /// 当前是否全部处于暂停状态。
    pub async fn is_all_paused(&self) -> bool {
        let folders = self.folders.lock().await;
        if folders.is_empty() {
            return false;
        }
        for h in folders.values() {
            if h.status.lock().await.state != "paused" {
                return false;
            }
        }
        true
    }

    pub async fn statuses(&self) -> Vec<FolderStatus> {
        let folders = self.folders.lock().await;
        let mut out = Vec::new();
        for h in folders.values() {
            out.push(h.status.lock().await.clone());
        }
        out.sort_by_key(|s| s.folder_id);
        out
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<SyncEvent> {
        self.event_tx.subscribe()
    }
}

/// Git 后端需要注入本地缓存目录。
async fn build_store_with_cache(
    folder: &FolderConfig,
    data_dir: &std::path::Path,
) -> anyhow::Result<Box<dyn RemoteStore>> {
    match &folder.backend {
        crate::config::BackendConfig::Git { remote_url, branch, username, password } => {
            let s = crate::storage::git_backend::GitStore::new(
                remote_url.clone(),
                branch.clone(),
                username.clone(),
                password.clone(),
            )
            .with_cache_dir(data_dir.join("git-cache"));
            Ok(Box::new(s))
        }
        other => storage::build_store(other).await,
    }
}

fn device_info() -> (String, String) {
    let cfg = config::AppConfig::load_or_create(&config::app_data_dir())
        .unwrap_or_else(|_| AppConfig {
            device_name: "Device".into(),
            device_id: Uuid::new_v4().to_string(),
            folders: vec![],
            settings: Default::default(),
        });
    (cfg.device_id, cfg.device_name)
}
