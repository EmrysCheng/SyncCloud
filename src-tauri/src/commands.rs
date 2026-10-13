//! Tauri IPC 命令层：前端 ⇄ 后端的全部接口。

use std::path::PathBuf;
use serde::Serialize;
use tauri::State;
use uuid::Uuid;

use crate::config::{self, app_data_dir, AppConfig, BackendConfig, FolderConfig, GlobalSettings};
use crate::sync::engine::SyncEvent;
use crate::sync::manager::{FolderManager, FolderStatus};
use crate::sync::state::ConflictRecord;
use crate::AppState;

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub device_name: String,
    pub device_id: String,
    pub version: String,
    pub data_dir: String,
    pub default_data_dir: String,
}

#[tauri::command]
pub async fn get_app_info(state: State<'_, AppState>) -> Result<AppInfo, String> {
    let cfg = state.config.lock().await;
    Ok(AppInfo {
        device_name: cfg.device_name.clone(),
        device_id: cfg.device_id.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        data_dir: app_data_dir().to_string_lossy().to_string(),
        default_data_dir: crate::config::default_data_dir().to_string_lossy().to_string(),
    })
}

#[tauri::command]
pub async fn save_settings(
    state: State<'_, AppState>,
    device_name: String,
    settings: GlobalSettings,
) -> Result<(), String> {
    let mut cfg = state.config.lock().await;
    cfg.device_name = device_name.trim().to_string();
    if cfg.device_name.is_empty() {
        cfg.device_name = "Device".into();
    }
    cfg.settings = settings;
    cfg.save(&app_data_dir()).map_err(err)?;
    // 关窗到托盘的开关在窗口事件（同步上下文）里读取，用原子变量即时生效
    crate::CLOSE_TO_TRAY.store(cfg.settings.close_to_tray, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<GlobalSettings, String> {
    Ok(state.config.lock().await.settings.clone())
}

/// 在系统默认浏览器打开外部链接（设置页的开放平台注册入口等）。
#[tauri::command]
pub async fn open_external(url: String) -> Result<(), String> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("仅允许打开 http(s) 链接".into());
    }
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| format!("打开链接失败: {e}"))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderSummary {
    #[serde(flatten)]
    pub config: FolderConfig,
    pub status: Option<FolderStatus>,
}

#[tauri::command]
pub async fn list_folders(state: State<'_, AppState>) -> Result<Vec<FolderSummary>, String> {
    let cfg = state.config.lock().await;
    let statuses = state.manager.statuses().await;
    let out = cfg
        .folders
        .iter()
        .map(|f| FolderSummary {
            config: f.clone(),
            status: statuses.iter().find(|s| s.folder_id == f.id).cloned(),
        })
        .collect();
    Ok(out)
}

#[tauri::command]
pub async fn add_folder(state: State<'_, AppState>, folder: FolderConfig) -> Result<String, String> {
    // 合法性检查
    if folder.name.trim().is_empty() {
        return Err("名称不能为空".into());
    }
    let local = std::path::PathBuf::from(&folder.local_path);
    if !local.is_dir() {
        return Err(format!("本地路径不存在或不是目录: {}", folder.local_path));
    }
    let mut cfg = state.config.lock().await;
    if cfg.folders.iter().any(|f| f.local_path == folder.local_path) {
        return Err("该本地目录已被其他同步任务使用".into());
    }
    let settings = cfg.settings.clone();
    let folder = FolderConfig { id: Uuid::new_v4(), ..folder };
    let id = folder.id.to_string();
    cfg.folders.push(folder.clone());
    cfg.save(&app_data_dir()).map_err(err)?;
    drop(cfg);
    state.manager.start_folder(&folder, &settings).await.map_err(err)?;
    Ok(id)
}

#[tauri::command]
pub async fn update_folder(state: State<'_, AppState>, folder: FolderConfig) -> Result<(), String> {
    let mut cfg = state.config.lock().await;
    match cfg.folders.iter_mut().find(|f| f.id == folder.id) {
        Some(slot) => *slot = folder.clone(),
        None => return Err("同步文件夹不存在".into()),
    }
    let settings = cfg.settings.clone();
    cfg.save(&app_data_dir()).map_err(err)?;
    drop(cfg);
    // 重启任务使配置生效
    state.manager.stop_folder(&folder.id).await;
    if folder.enabled {
        state.manager.start_folder(&folder, &settings).await.map_err(err)?;
    }
    Ok(())
}

#[tauri::command]
pub async fn remove_folder(state: State<'_, AppState>, folder_id: String) -> Result<(), String> {
    let id: Uuid = folder_id.parse().map_err(|_| "ID 非法".to_string())?;
    state.manager.stop_folder(&id).await;
    let mut cfg = state.config.lock().await;
    cfg.folders.retain(|f| f.id != id);
    cfg.save(&app_data_dir()).map_err(err)?;
    // 状态库保留，便于用户反悔后恢复
    Ok(())
}

#[tauri::command]
pub async fn sync_now(state: State<'_, AppState>, folder_id: String) -> Result<bool, String> {
    let id: Uuid = folder_id.parse().map_err(|_| "ID 非法".to_string())?;
    Ok(state.manager.sync_now(&id).await)
}

#[tauri::command]
pub async fn pause_folder(state: State<'_, AppState>, folder_id: String) -> Result<(), String> {
    let id: Uuid = folder_id.parse().map_err(|_| "ID 非法".to_string())?;
    state.manager.pause_folder(&id).await;
    Ok(())
}

#[tauri::command]
pub async fn resume_folder(state: State<'_, AppState>, folder_id: String) -> Result<(), String> {
    let id: Uuid = folder_id.parse().map_err(|_| "ID 非法".to_string())?;
    state.manager.resume_folder(&id).await;
    Ok(())
}

/// 后端连通性测试（添加向导中调用）。
#[tauri::command]
pub async fn test_backend(backend: BackendConfig) -> Result<String, String> {
    let store = crate::storage::build_store(&backend).await.map_err(err)?;
    store.test().await.map_err(err)?;
    Ok("连接成功，读写权限正常".into())
}

#[tauri::command]
pub async fn list_conflicts(
    _state: State<'_, AppState>,
    folder_id: String,
    include_resolved: bool,
) -> Result<Vec<ConflictRecord>, String> {
    let id: Uuid = folder_id.parse().map_err(|_| "ID 非法".to_string())?;
    let db = crate::sync::state::StateDb::open(&config::folder_data_dir(&id).join("state.db")).map_err(err)?;
    db.list_conflicts(include_resolved).map_err(err)
}

#[tauri::command]
pub async fn resolve_conflict(
    state: State<'_, AppState>,
    folder_id: String,
    conflict_id: i64,
    resolution: String,
) -> Result<(), String> {
    let id: Uuid = folder_id.parse().map_err(|_| "ID 非法".to_string())?;
    let db = crate::sync::state::StateDb::open(&config::folder_data_dir(&id).join("state.db")).map_err(err)?;
    
    if let Ok(Some(record)) = db.get_conflict(conflict_id) {
        let cfg = state.config.lock().await;
        let folder = cfg.folders.iter().find(|f| f.id == id).cloned();
        drop(cfg);

        if let Some(folder) = folder {
            let root = std::path::PathBuf::from(&folder.local_path);
            let orig_path = root.join(&record.path);
            let copy_path = record.conflict_copy.as_ref().map(|c| root.join(c));

            match resolution.as_str() {
                "keep_local" => {
                    if let Some(cp) = copy_path {
                        let _ = std::fs::remove_file(cp);
                    }
                    db.resolve_conflict(conflict_id, "保留本地原版（已清理冲突副本）").map_err(err)?;
                }
                "keep_copy" => {
                    if let Some(cp) = copy_path {
                        if cp.exists() {
                            let _ = std::fs::copy(&cp, &orig_path);
                            let _ = std::fs::remove_file(&cp);
                        }
                    }
                    db.resolve_conflict(conflict_id, "保留冲突副本（已覆盖原版）").map_err(err)?;
                }
                _ => {
                    db.resolve_conflict(conflict_id, "保留双方版本").map_err(err)?;
                }
            }
        } else {
            db.resolve_conflict(conflict_id, &resolution).map_err(err)?;
        }
    } else {
        db.resolve_conflict(conflict_id, &resolution).map_err(err)?;
    }

    // 标记后触发一轮同步（清理或重新上传按需进行）
    let _ = state.manager.sync_now(&id).await;
    Ok(())
}

#[tauri::command]
pub async fn get_recent_events(
    state: State<'_, AppState>,
    limit: usize,
) -> Result<Vec<SyncEvent>, String> {
    let buf = state.recent_events.lock().await;
    let skip = buf.len().saturating_sub(limit);
    Ok(buf.iter().skip(skip).cloned().collect())
}

// ---- 云盘 OAuth 登录 ----

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaiduLoginInfo {
    pub user_code: String,
    pub verification_url: String,
    pub qrcode_url: String,
    pub device_code: String,
    pub interval: u64,
}

#[tauri::command]
pub async fn baidu_login_start(app_key: String, secret_key: String) -> Result<BaiduLoginInfo, String> {
    let store = crate::storage::baidu::BaiduStore::new(String::new(), app_key, secret_key, "/apps/synccloud".into());
    let resp = store.start_device_login().await.map_err(err)?;
    Ok(BaiduLoginInfo {
        user_code: resp.user_code,
        verification_url: resp.verification_url,
        qrcode_url: resp.qrcode_url,
        device_code: resp.device_code,
        interval: resp.interval,
    })
}

#[tauri::command]
pub async fn baidu_login_poll(
    app_key: String,
    secret_key: String,
    device_code: String,
    auth_id: Option<String>,
    account_name: Option<String>,
) -> Result<Option<crate::config::CloudAccount>, String> {
    let id = auth_id.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let store = crate::storage::baidu::BaiduStore::new(id.clone(), app_key.clone(), secret_key.clone(), "/apps/synccloud".into());
    let done = store.poll_device_login(&device_code).await.map_err(err)?;
    if done {
        let name = account_name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("百度账号-{}", &id[..6]));
        let account = crate::config::CloudAccount {
            id: id.clone(),
            provider: "baidu".into(),
            name,
            app_key: Some(app_key),
            secret_key: Some(secret_key),
            created_at: chrono::Local::now().to_rfc3339(),
        };
        let mut list = crate::config::load_cloud_accounts(&app_data_dir());
        list.retain(|a| a.id != account.id);
        list.push(account.clone());
        let _ = crate::config::save_cloud_accounts(&app_data_dir(), &list);
        Ok(Some(account))
    } else {
        Ok(None)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AliyunQrInfo {
    /// 二维码内容（前端渲染为二维码图片）
    pub qr_url: String,
    /// 浏览器可打开的二维码页面（兜底）
    pub link_url: String,
    pub sid: String,
}

#[tauri::command]
pub async fn aliyun_qr_start() -> Result<AliyunQrInfo, String> {
    let (qr, link, sid) = crate::storage::aliyun::start_qr_login().await.map_err(err)?;
    Ok(AliyunQrInfo {
        qr_url: qr,
        link_url: link,
        sid,
    })
}

#[tauri::command]
pub async fn aliyun_qr_poll(
    sid: String,
    auth_id: Option<String>,
    account_name: Option<String>,
) -> Result<Option<crate::config::CloudAccount>, String> {
    let id = auth_id.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let done = crate::storage::aliyun::poll_qr_login(&sid, Some(&id)).await.map_err(err)?;
    if done {
        let name = account_name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("阿里云盘-{}", &id[..6]));
        let account = crate::config::CloudAccount {
            id: id.clone(),
            provider: "aliyun".into(),
            name,
            app_key: None,
            secret_key: None,
            created_at: chrono::Local::now().to_rfc3339(),
        };
        let mut list = crate::config::load_cloud_accounts(&app_data_dir());
        list.retain(|a| a.id != account.id);
        list.push(account.clone());
        let _ = crate::config::save_cloud_accounts(&app_data_dir(), &list);
        Ok(Some(account))
    } else {
        Ok(None)
    }
}

// ---- 云盘授权状态与账号管理 ----

#[tauri::command]
pub async fn list_cloud_accounts(provider: Option<String>) -> Result<Vec<crate::config::CloudAccount>, String> {
    let list = crate::config::load_cloud_accounts(&app_data_dir());
    if let Some(p) = provider {
        Ok(list.into_iter().filter(|a| a.provider == p).collect())
    } else {
        Ok(list)
    }
}

#[tauri::command]
pub async fn delete_cloud_account(id: String) -> Result<(), String> {
    let mut list = crate::config::load_cloud_accounts(&app_data_dir());
    if let Some(acc) = list.iter().find(|a| a.id == id).cloned() {
        if acc.provider == "baidu" {
            let key = format!("baidu:account:{}", acc.id);
            let _ = crate::credential::save_secret(&key, "");
        } else if acc.provider == "aliyun" {
            let key = format!("aliyun:account:{}", acc.id);
            let _ = crate::credential::save_secret(&key, "");
        }
    }
    list.retain(|a| a.id != id);
    crate::config::save_cloud_accounts(&app_data_dir(), &list).map_err(err)?;
    Ok(())
}

#[tauri::command]
pub async fn save_cloud_account(account: crate::config::CloudAccount) -> Result<(), String> {
    let mut list = crate::config::load_cloud_accounts(&app_data_dir());
    if let Some(slot) = list.iter_mut().find(|a| a.id == account.id) {
        *slot = account;
    } else {
        list.push(account);
    }
    crate::config::save_cloud_accounts(&app_data_dir(), &list).map_err(err)?;
    Ok(())
}

#[tauri::command]
pub async fn aliyun_authorized(auth_id: Option<String>) -> Result<bool, String> {
    let id = auth_id.unwrap_or_default();
    Ok(crate::storage::aliyun::AliyunStore::authorized(&id))
}

#[tauri::command]
pub async fn baidu_authorized(auth_id: Option<String>, app_key: Option<String>) -> Result<bool, String> {
    let id = auth_id.unwrap_or_default();
    let ak = app_key.unwrap_or_default();
    Ok(crate::storage::baidu::BaiduStore::authorized(&id, &ak))
}

// ---- 开机自启动 ----

#[tauri::command]
pub async fn is_autostart() -> Result<bool, String> {
    Ok(crate::autostart::is_autostart_enabled())
}

#[tauri::command]
pub async fn set_autostart(enabled: bool) -> Result<(), String> {
    crate::autostart::set_autostart_enabled(enabled).map_err(err)?;
    crate::update_autostart_menu(enabled);
    Ok(())
}

// ---- 数据目录管理 ----

#[tauri::command]
pub async fn change_data_dir(new_path: String, copy_existing: bool) -> Result<String, String> {
    let p = std::path::PathBuf::from(new_path.trim());
    if p.as_os_str().is_empty() {
        return Err("数据目录路径不能为空".into());
    }
    crate::config::set_app_data_dir(p.clone(), copy_existing).map_err(err)?;
    Ok(crate::config::app_data_dir().to_string_lossy().to_string())
}

#[tauri::command]
pub fn restart_app(app: tauri::AppHandle) {
    app.restart();
}

/// 在操作系统文件资源管理器中打开指定目录或选中指定文件
#[tauri::command]
pub async fn open_path_in_explorer(path: String, select: bool) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err("指定的路径不存在".into());
    }
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        if select {
            let canonical = p.canonicalize().unwrap_or(p);
            let s = canonical.to_string_lossy().replace("\\\\?\\", "");
            Command::new("explorer")
                .args(["/select,", &s])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map_err(|e| format!("打开资源管理器失败: {e}"))?;
        } else {
            let canonical = p.canonicalize().unwrap_or(p);
            let s = canonical.to_string_lossy().replace("\\\\?\\", "");
            Command::new("explorer")
                .arg(&s)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map_err(|e| format!("打开资源管理器失败: {e}"))?;
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        tauri_plugin_opener::open_path(path, None::<&str>).map_err(|e| e.to_string())
    }
}

/// 打开当前应用的数据存储目录
#[tauri::command]
pub async fn open_data_dir() -> Result<(), String> {
    let dir = app_data_dir();
    let s = dir.to_string_lossy().to_string();
    open_path_in_explorer(s, false).await
}

/// 立即同步全部启用的文件夹
#[tauri::command]
pub async fn sync_all_folders(state: State<'_, AppState>) -> Result<(), String> {
    for s in state.manager.statuses().await {
        let _ = state.manager.sync_now(&s.folder_id).await;
    }
    Ok(())
}

/// 暂停所有文件夹的同步
#[tauri::command]
pub async fn pause_all_folders(state: State<'_, AppState>) -> Result<(), String> {
    for s in state.manager.statuses().await {
        let _ = state.manager.pause_folder(&s.folder_id).await;
    }
    Ok(())
}

/// 恢复所有文件夹的同步
#[tauri::command]
pub async fn resume_all_folders(state: State<'_, AppState>) -> Result<(), String> {
    for s in state.manager.statuses().await {
        let _ = state.manager.resume_folder(&s.folder_id).await;
    }
    Ok(())
}

// ---- 内部工具 ----

impl AppState {
    pub fn new() -> Self {
        let manager = FolderManager::new();
        Self {
            manager,
            config: tokio::sync::Mutex::new(AppConfig {
                device_name: String::new(),
                device_id: String::new(),
                folders: vec![],
                settings: GlobalSettings::default(),
            }),
            recent_events: tokio::sync::Mutex::new(Vec::new()),
        }
    }
}
