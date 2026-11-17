//! Tauri IPC 命令层：前端 ⇄ 后端的全部接口。

use serde::Serialize;
use std::path::PathBuf;
use tauri::State;
use uuid::Uuid;

use crate::config::{self, app_data_dir, AppConfig, BackendConfig, FolderConfig, GlobalSettings};
use crate::storage::RemoteStore;
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
        default_data_dir: crate::config::default_data_dir()
            .to_string_lossy()
            .to_string(),
    })
}

#[tauri::command]
pub async fn save_settings(
    state: State<'_, AppState>,
    device_name: String,
    settings: GlobalSettings,
) -> Result<(), String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    if crate::sync::MAINTENANCE.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("迁移后请重启程序再修改配置".into());
    }
    let mut cfg = state.config.lock().await;
    settings.validate().map_err(err)?;
    let old = cfg.clone();
    cfg.device_name = device_name.trim().to_string();
    if cfg.device_name.is_empty() {
        cfg.device_name = "Device".into();
    }
    cfg.settings = settings.clone();
    if let Err(e) = cfg.protect_credentials() {
        *cfg = old;
        return Err(err(e));
    }
    if let Err(e) = cfg.save(&app_data_dir()) {
        *cfg = old;
        return Err(err(e));
    }
    state.manager.apply_settings(&settings).await;
    // 关窗到托盘的开关在窗口事件（同步上下文）里读取，用原子变量即时生效
    crate::CLOSE_TO_TRAY.store(
        cfg.settings.close_to_tray,
        std::sync::atomic::Ordering::Relaxed,
    );
    // 网络带宽限速即刻热生效
    crate::sync::transfer::apply_bandwidth_limits(
        cfg.settings.max_upload_kbps,
        cfg.settings.max_download_kbps,
    );
    // 日志等级、文件开关与自定义目录热生效（无需重启）
    crate::logging::apply_settings(
        &cfg.settings.log_level,
        cfg.settings.log_to_file,
        cfg.settings.custom_log_dir.as_deref(),
    );
    Ok(())
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<GlobalSettings, String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let mut cfg = state.config.lock().await;
    cfg.protect_credentials().map_err(err)?;
    Ok(cfg.settings.clone())
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
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let mut cfg = state.config.lock().await;
    cfg.protect_credentials().map_err(err)?;
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
pub async fn add_folder(
    state: State<'_, AppState>,
    folder: FolderConfig,
) -> Result<String, String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    if crate::sync::MAINTENANCE.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("迁移后请重启程序再修改配置".into());
    }
    folder.validate().map_err(err)?;
    // 合法性检查
    if folder.name.trim().is_empty() {
        return Err("名称不能为空".into());
    }
    let local = std::path::PathBuf::from(&folder.local_path);
    if !local.is_dir() {
        return Err(format!("本地路径不存在或不是目录: {}", folder.local_path));
    }
    let mut cfg = state.config.lock().await;
    if cfg
        .folders
        .iter()
        .any(|f| f.local_path == folder.local_path)
    {
        return Err("该本地目录已被其他同步任务使用".into());
    }
    let settings = cfg.settings.clone();
    let folder = FolderConfig {
        id: Uuid::new_v4(),
        ..folder
    };
    let id = folder.id.to_string();
    let mut next = cfg.clone();
    next.folders.push(folder.clone());
    next.protect_credentials().map_err(err)?;
    next.save(&app_data_dir()).map_err(err)?;
    *cfg = next;
    if folder.enabled {
        state
            .manager
            .start_folder(&folder, &settings)
            .await
            .map_err(err)?;
    }
    Ok(id)
}

#[tauri::command]
pub async fn update_folder(state: State<'_, AppState>, folder: FolderConfig) -> Result<(), String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    if crate::sync::MAINTENANCE.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("迁移后请重启程序再修改配置".into());
    }
    let mut cfg = state.config.lock().await;
    folder.validate().map_err(err)?;
    let mut next = cfg.clone();
    let slot = next
        .folders
        .iter_mut()
        .find(|f| f.id == folder.id)
        .ok_or("同步文件夹不存在")?;
    *slot = folder.clone();
    let settings = next.settings.clone();
    next.protect_credentials().map_err(err)?;
    next.save(&app_data_dir()).map_err(err)?;
    *cfg = next;
    // 重启任务使配置生效
    state.manager.stop_folder(&folder.id).await;
    if folder.enabled {
        state
            .manager
            .start_folder(&folder, &settings)
            .await
            .map_err(err)?;
    }
    Ok(())
}

#[tauri::command]
pub async fn remove_folder(state: State<'_, AppState>, folder_id: String) -> Result<(), String> {
    remove_folder_inner(&state, folder_id).await
}
async fn remove_folder_inner(state: &AppState, folder_id: String) -> Result<(), String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    if crate::sync::MAINTENANCE.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("迁移后请重启程序再修改配置".into());
    }
    let id: Uuid = folder_id.parse().map_err(|_| "ID 非法".to_string())?;
    let mut cfg = state.config.lock().await;
    let mut next = cfg.clone();
    next.folders.retain(|f| f.id != id);
    next.save(&app_data_dir()).map_err(err)?;
    *cfg = next;
    state.manager.stop_folder(&id).await;
    // 状态库保留，便于用户反悔后恢复
    Ok(())
}

#[tauri::command]
pub async fn sync_now(
    state: State<'_, AppState>,
    folder_id: String,
) -> Result<crate::sync::manager::SyncReceipt, String> {
    let id: Uuid = folder_id.parse().map_err(|_| "ID 非法".to_string())?;
    state.manager.sync_now(&id).await
}

#[tauri::command]
pub async fn force_push_folder(
    state: State<'_, AppState>,
    folder_id: String,
) -> Result<crate::sync::manager::SyncReceipt, String> {
    let id: Uuid = folder_id.parse().map_err(|_| "ID 非法".to_string())?;
    state.manager.force_push(&id).await
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
    state.manager.resume_folder(&id).await
}

/// 后端连通性测试（添加向导中调用）。
#[tauri::command]
pub async fn test_backend(backend: BackendConfig) -> Result<String, String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
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
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let id: Uuid = folder_id.parse().map_err(|_| "ID 非法".to_string())?;
    let db = crate::sync::state::StateDb::open(&config::folder_data_dir(&id).join("state.db"))
        .map_err(err)?;
    db.list_conflicts(include_resolved).map_err(err)
}

#[tauri::command]
pub async fn resolve_conflict(
    state: State<'_, AppState>,
    folder_id: String,
    conflict_id: i64,
    resolution: String,
) -> Result<(), String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let id: Uuid = folder_id.parse().map_err(|_| "ID 非法".to_string())?;
    let db = crate::sync::state::StateDb::open(&config::folder_data_dir(&id).join("state.db"))
        .map_err(err)?;

    let folder = state
        .config
        .lock()
        .await
        .folders
        .iter()
        .find(|f| f.id == id)
        .cloned()
        .ok_or("文件夹不存在")?;
    let _operation = crate::sync::operation_lock(&id).await.map_err(err)?;
    let record = db
        .get_conflict(conflict_id)
        .map_err(err)?
        .ok_or("冲突记录不存在")?;
    let root = std::path::Path::new(&folder.local_path);
    let orig = crate::sync::scanner::safe_path(root, &record.path).map_err(err)?;
    let copy = record
        .conflict_copy
        .as_ref()
        .map(|p| crate::sync::scanner::safe_path(root, p))
        .transpose()
        .map_err(err)?;
    match resolution.as_str() {
        "keep_local" => {
            if let Some(p) = copy {
                crate::sync::recycle::archive_file(&id, &p, &record.path, "conflict")
                    .await
                    .map_err(err)?;
                tokio::fs::remove_file(p).await.map_err(|e| e.to_string())?;
            }
        }
        "keep_copy" => {
            let copy = copy.ok_or("冲突副本路径缺失")?;
            if orig.exists() {
                crate::sync::recycle::archive_file(&id, &orig, &record.path, "overwritten")
                    .await
                    .map_err(err)?;
            }
            // Keep the conflict copy available until the database commit succeeds.
            let staged = orig.with_extension(format!("sc-stage-{}", Uuid::new_v4()));
            let result = async {
                tokio::fs::copy(copy, &staged).await?;
                crate::sync::fslock::replace_file(&staged, &orig).await
            }
            .await;
            if let Err(e) = result {
                let _ = tokio::fs::remove_file(staged).await;
                return Err(e.to_string());
            }
        }
        "keep_both" => {}
        _ => return Err("未知冲突处理方式".into()),
    }
    db.resolve_conflict(conflict_id, &resolution).map_err(err)?;
    drop(_operation);
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
pub async fn baidu_login_start(
    app_key: String,
    secret_key: String,
) -> Result<BaiduLoginInfo, String> {
    let secret_key = crate::credential::resolve(&secret_key).map_err(err)?;
    let store = crate::storage::baidu::BaiduStore::new(
        String::new(),
        app_key,
        secret_key,
        "/apps/synccloud".into(),
    );
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
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let id = auth_id
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let secret_key = crate::credential::resolve(&secret_key).map_err(err)?;
    let store = crate::storage::baidu::BaiduStore::new(
        id.clone(),
        app_key.clone(),
        secret_key.clone(),
        "/apps/synccloud".into(),
    );
    let account_lock = crate::credential::account_lock(&format!("baidu:account:{id}"));
    let _account = account_lock.lock().await;
    let done = store
        .poll_device_login_unlocked(&device_code)
        .await
        .map_err(err)?;
    if done {
        let name = account_name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("百度账号-{}", id.chars().take(6).collect::<String>()));
        let account = crate::config::CloudAccount {
            id: id.clone(),
            provider: "baidu".into(),
            name,
            app_key: Some(app_key),
            secret_key: Some(secret_key),
            created_at: chrono::Local::now().to_rfc3339(),
        };
        let persisted =
            crate::config::upsert_cloud_account(&app_data_dir(), account).map_err(err)?;
        Ok(Some(persisted))
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
    let (qr, link, sid) = crate::storage::aliyun::start_qr_login()
        .await
        .map_err(err)?;
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
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let id = auth_id
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let account_lock = crate::credential::account_lock(&format!("aliyun:account:{id}"));
    let _account = account_lock.lock().await;
    let done = crate::storage::aliyun::poll_qr_login_unlocked(&sid, Some(&id))
        .await
        .map_err(err)?;
    if done {
        let name = account_name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("阿里云盘-{}", id.chars().take(6).collect::<String>()));
        let account = crate::config::CloudAccount {
            id: id.clone(),
            provider: "aliyun".into(),
            name,
            app_key: None,
            secret_key: None,
            created_at: chrono::Local::now().to_rfc3339(),
        };
        let persisted =
            crate::config::upsert_cloud_account(&app_data_dir(), account).map_err(err)?;
        Ok(Some(persisted))
    } else {
        Ok(None)
    }
}

// ---- 云盘授权状态与账号管理 ----

#[tauri::command]
pub async fn list_cloud_accounts(
    provider: Option<String>,
) -> Result<Vec<crate::config::CloudAccount>, String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let list = crate::config::load_cloud_accounts(&app_data_dir()).map_err(err)?;
    if let Some(p) = provider {
        Ok(list.into_iter().filter(|a| a.provider == p).collect())
    } else {
        Ok(list)
    }
}

#[tauri::command]
pub async fn delete_cloud_account(id: String) -> Result<(), String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let account = crate::config::load_cloud_accounts(&app_data_dir())
        .map_err(err)?
        .into_iter()
        .find(|a| a.id == id);
    if let Some(acc) = account {
        let key = format!("{}:account:{}", acc.provider, acc.id);
        let lock = crate::credential::account_lock(&key);
        let _guard = lock.lock().await;
        crate::config::remove_cloud_account(&app_data_dir(), &id).map_err(err)?;
        crate::credential::delete_secret(&key).map_err(err)?;
    }

    Ok(())
}

#[tauri::command]
pub async fn save_cloud_account(account: crate::config::CloudAccount) -> Result<(), String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let lock =
        crate::credential::account_lock(&format!("{}:account:{}", account.provider, account.id));
    let _account = lock.lock().await;
    crate::config::upsert_cloud_account(&app_data_dir(), account).map_err(err)?;
    Ok(())
}

#[tauri::command]
pub async fn aliyun_authorized(auth_id: Option<String>) -> Result<bool, String> {
    let id = auth_id.unwrap_or_default();
    Ok(crate::storage::aliyun::AliyunStore::authorized(&id))
}

#[tauri::command]
pub async fn baidu_authorized(
    auth_id: Option<String>,
    app_key: Option<String>,
) -> Result<bool, String> {
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

// ---- Windows 资源管理器右键扩展（坚果云同款云桥功能） ----

#[tauri::command]
pub async fn is_shell_menu_enabled() -> Result<bool, String> {
    Ok(crate::shell_menu::is_shell_menu_enabled())
}

#[tauri::command]
pub async fn set_shell_menu_enabled(enabled: bool) -> Result<(), String> {
    crate::shell_menu::set_shell_menu_enabled(enabled).map_err(err)
}

#[tauri::command]
pub async fn set_file_storage_mode(
    state: State<'_, AppState>,
    folder_id: String,
    path: String,
    mode: String,
) -> Result<usize, String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let id: Uuid = folder_id.parse().map_err(|_| "ID 非法".to_string())?;
    let cfg = state.config.lock().await;
    let folder = cfg
        .folders
        .iter()
        .find(|f| f.id == id)
        .cloned()
        .ok_or_else(|| "文件夹不存在".to_string())?;
    drop(cfg);

    let data_dir = crate::config::folder_data_dir(&folder.id);
    let db = crate::sync::state::StateDb::open(&data_dir.join("state.db")).map_err(err)?;

    crate::sync::storage_mode::apply_storage_mode_path(&folder, &db, &path, &mode)
        .await
        .map_err(err)
}

// ---- 数据目录管理 ----

#[tauri::command]
pub async fn change_data_dir(
    state: State<'_, AppState>,
    new_path: String,
    copy_existing: bool,
) -> Result<String, String> {
    let _migration = crate::sync::data_migration_guard().await;
    let p = std::path::PathBuf::from(new_path.trim());
    if p.as_os_str().is_empty() {
        return Err("数据目录路径不能为空".into());
    }
    let cfg = state.config.lock().await;
    if crate::sync::MAINTENANCE.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err("数据目录正在迁移，请重启程序".into());
    }
    for folder in &cfg.folders {
        state.manager.stop_folder(&folder.id).await;
    }
    // Drain shell/hydration/restore operations that started before maintenance.
    let mut guards = Vec::new();
    for folder in &cfg.folders {
        guards.push(crate::sync::drain_operation_lock(&folder.id).await);
    }
    let target = p.clone();
    let result = match tokio::task::spawn_blocking(move || {
        crate::config::set_app_data_dir(target, copy_existing)
    })
    .await
    {
        Ok(r) => r,
        Err(e) => Err(e.into()),
    };
    if let Err(e) = result {
        crate::sync::MAINTENANCE.store(false, std::sync::atomic::Ordering::SeqCst);
        drop(guards);
        state.manager.start_all(&cfg).await;
        return Err(err(e));
    }
    Ok(p.to_string_lossy().to_string())
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
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
        let s = canonical.to_string_lossy().replace("\\\\?\\", "");
        let res = if select {
            Command::new("explorer")
                .args(["/select,", &s])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
        } else {
            Command::new("explorer")
                .arg(&s)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
        };
        match res {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "explorer 调用失败，尝试 opener");
                tauri_plugin_opener::open_path(path, None::<&str>).map_err(|e2| e2.to_string())
            }
        }
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
    let _ = std::fs::create_dir_all(&dir);
    let s = dir.to_string_lossy().to_string();
    open_path_in_explorer(s, false).await
}

/// 立即同步全部启用的文件夹
#[tauri::command]
pub async fn sync_all_folders(
    state: State<'_, AppState>,
) -> Result<Vec<crate::sync::manager::SyncReceipt>, String> {
    let mut receipts = Vec::new();
    let cfg = state.config.lock().await;
    for folder in cfg.folders.iter().filter(|f| f.enabled) {
        receipts.push(match state.manager.sync_now(&folder.id).await {
            Ok(receipt) => receipt,
            Err(error) => crate::sync::manager::SyncReceipt {
                folder_id: folder.id,
                disposition: "rejected",
                error: Some(error),
            },
        });
    }
    Ok(receipts)
}

/// 分析并预览指定文件夹的同步差异计划（对标 GoodSync / EchoSync）。
#[tauri::command]
pub async fn analyze_folder(
    state: State<'_, AppState>,
    folder_id: String,
) -> Result<crate::sync::plan::SyncPlan, String> {
    let id: Uuid = folder_id.parse().map_err(|_| "ID 非法".to_string())?;
    let cfg = state.config.lock().await;
    let folder = cfg
        .folders
        .iter()
        .find(|f| f.id == id)
        .cloned()
        .ok_or_else(|| "未找到该文件夹".to_string())?;
    let settings = cfg.settings.clone();
    drop(cfg);

    state.manager.analyze(&folder, &settings).await.map_err(err)
}

/// 打开防误删保护回收站目录（对标 Syncthing / GoodSync）。
#[tauri::command]
pub async fn open_recycle_bin(folder_id: Option<String>) -> Result<(), String> {
    let dir = if let Some(fid_str) = folder_id {
        let id: Uuid = fid_str.parse().map_err(|_| "ID 非法".to_string())?;
        config::folder_data_dir(&id).join("recycle_bin")
    } else {
        config::app_data_dir().join("recycle_bin")
    };
    let _ = std::fs::create_dir_all(&dir);
    open_path_in_explorer(dir.to_string_lossy().to_string(), false).await
}

// ---- 授权账户状态探测 ----

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudAccountStatus {
    pub account_id: String,
    /// token 是否仍存于系统凭据管理器
    pub authorized: bool,
    /// 实际连通性探测是否成功
    pub ok: bool,
    /// 成功时的摘要（如账户空间）或失败的错误信息
    pub detail: String,
    pub checked_at: String,
}

/// 探测授权账户的实时可用性：令牌存在性 + 一次轻量远端调用。
#[tauri::command]
pub async fn cloud_account_status(account_id: String) -> Result<CloudAccountStatus, String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let acc = crate::config::load_cloud_accounts(&app_data_dir())
        .map_err(err)?
        .into_iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| "账户不存在".to_string())?;
    let checked_at = chrono::Local::now().to_rfc3339();

    let (authorized, probe) = match acc.provider.as_str() {
        "baidu" => {
            let app_key = acc.app_key.clone().unwrap_or_default();
            let secret_key =
                crate::credential::resolve(&acc.secret_key.clone().unwrap_or_default())
                    .map_err(err)?;
            let authorized = crate::storage::baidu::BaiduStore::authorized(&acc.id, &app_key);
            if !authorized {
                (false, Err(anyhow::anyhow!("授权令牌缺失，请重新扫码授权")))
            } else {
                let store = crate::storage::baidu::BaiduStore::new(
                    acc.id.clone(),
                    app_key,
                    secret_key,
                    "/apps/synccloud".into(),
                );
                (true, store.test().await)
            }
        }
        "aliyun" => {
            let authorized = crate::storage::aliyun::AliyunStore::authorized(&acc.id);
            if !authorized {
                (false, Err(anyhow::anyhow!("授权令牌缺失，请重新扫码授权")))
            } else {
                let store = crate::storage::aliyun::AliyunStore::new(
                    acc.id.clone(),
                    "/apps/synccloud".into(),
                );
                (true, store.test().await)
            }
        }
        other => (false, Err(anyhow::anyhow!("未知的账户类型 {other}"))),
    };

    let (ok, detail) = match probe {
        Ok(()) => (true, "连接正常，读写权限有效".into()),
        Err(e) => (false, format!("{e:#}")),
    };
    Ok(CloudAccountStatus {
        account_id: acc.id,
        authorized,
        ok,
        detail,
        checked_at,
    })
}

// ---- 日志管理 ----

#[tauri::command]
pub async fn list_log_files() -> Result<Vec<crate::logging::LogFileInfo>, String> {
    Ok(crate::logging::list_log_files())
}

#[tauri::command]
pub async fn read_log_tail(file: String, lines: usize) -> Result<Vec<String>, String> {
    crate::logging::read_log_tail(&file, lines.clamp(10, 2000)).map_err(err)
}

/// 获取当前生效的日志输出目录完整路径
#[tauri::command]
pub async fn get_current_log_dir() -> Result<String, String> {
    Ok(crate::logging::log_dir().to_string_lossy().to_string())
}

/// 在资源管理器中打开日志目录
#[tauri::command]
pub async fn open_log_dir() -> Result<(), String> {
    let dir = crate::logging::log_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
    open_path_in_explorer(dir.to_string_lossy().to_string(), false).await
}

/// 恢复所有文件夹的同步
#[tauri::command]
pub async fn resume_all_folders(state: State<'_, AppState>) -> Result<(), String> {
    state.manager.resume_all().await;
    Ok(())
}

/// 全局暂停所有文件夹的同步（对标 Syncthing / Resilio Sync）
#[tauri::command]
pub async fn pause_all_folders(state: State<'_, AppState>) -> Result<(), String> {
    state.manager.pause_all().await;
    Ok(())
}

/// 查询是否所有同步文件夹均处于暂停状态
#[tauri::command]
pub async fn is_all_paused(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(state.manager.is_all_paused().await)
}

// ---- 安全防误删回收站管理（对标 FreeFileSync / Syncthing） ----

/// 获取指定文件夹的安全回收站归档列表
#[tauri::command]
pub async fn list_recycle_bin(
    folder_id: String,
) -> Result<Vec<crate::sync::recycle::RecycleItem>, String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let id: Uuid = folder_id
        .parse()
        .map_err(|_| "Folder ID 非法".to_string())?;
    crate::sync::recycle::list_items(&id).map_err(err)
}

/// 从安全回收站一键还原指定文件到原工作路径
#[tauri::command]
pub async fn restore_recycle_file(
    state: State<'_, AppState>,
    folder_id: String,
    item_id: String,
) -> Result<String, String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let id: Uuid = folder_id
        .parse()
        .map_err(|_| "Folder ID 非法".to_string())?;
    let cfg = state.config.lock().await;
    let folder = cfg
        .folders
        .iter()
        .find(|f| f.id == id)
        .cloned()
        .ok_or_else(|| "未找到对应的同步文件夹配置".to_string())?;
    drop(cfg);

    let local_root = std::path::PathBuf::from(&folder.local_path);
    let _operation = crate::sync::operation_lock(&id).await.map_err(err)?;
    crate::sync::recycle::restore_item(&id, &item_id, &local_root)
        .await
        .map_err(err)
}

/// 清空指定文件夹的安全回收站
#[tauri::command]
pub async fn clean_recycle_bin(folder_id: String) -> Result<usize, String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let id: Uuid = folder_id
        .parse()
        .map_err(|_| "Folder ID 非法".to_string())?;
    crate::sync::recycle::clean_all(&id).await.map_err(err)
}

/// 删除指定回收站条目
#[tauri::command]
pub async fn delete_recycle_item(folder_id: String, item_id: String) -> Result<(), String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let id: Uuid = folder_id
        .parse()
        .map_err(|_| "Folder ID 非法".to_string())?;
    crate::sync::recycle::delete_item(&id, &item_id)
        .await
        .map_err(err)
}

/// 在 Windows 资源管理器中打开安全回收站目录
#[tauri::command]
pub async fn open_recycle_bin_dir(folder_id: String) -> Result<(), String> {
    let id: Uuid = folder_id
        .parse()
        .map_err(|_| "Folder ID 非法".to_string())?;
    let dir = crate::sync::recycle::recycle_dir(&id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建回收站目录失败: {e}"))?;
    open_path_in_explorer(dir.to_string_lossy().to_string(), false).await
}

// ---- 传输历史管理（支持自定义保留周期与自动清理） ----

#[tauri::command]
pub async fn get_activity_history(
    folder_id: Option<String>,
    action: Option<String>,
    keyword: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<crate::sync::history::ActivityPage, String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let limit = limit.unwrap_or(50).clamp(1, 200);
    let offset = offset.unwrap_or(0);
    Ok(crate::sync::history::ActivityPage {
        items: crate::sync::history::query_history(
            folder_id.as_deref(),
            action.as_deref(),
            keyword.as_deref(),
            limit,
            offset,
        ),
        total: crate::sync::history::count_history(
            folder_id.as_deref(),
            action.as_deref(),
            keyword.as_deref(),
        ),
    })
}

#[tauri::command]
pub async fn clean_expired_activity(state: State<'_, AppState>) -> Result<usize, String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    let days = {
        let guard = state.config.lock().await;
        guard.settings.activity_retention_days
    };
    Ok(crate::sync::history::clean_expired_history(days))
}

#[tauri::command]
pub async fn clear_activity_history(folder_id: Option<String>) -> Result<usize, String> {
    let _data_write = crate::sync::data_write_guard().await.map_err(err)?;
    Ok(crate::sync::history::clear_all_history(
        folder_id.as_deref(),
    ))
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

#[cfg(test)]
mod reliability_tests {
    use super::*;
    #[tokio::test]
    async fn removal_waits_for_config_transaction_and_stops_restarted_task() {
        let root = std::env::temp_dir().join(format!("synccloud-lifecycle-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("local")).unwrap();
        crate::config::set_app_data_dir_in_memory(root.clone());
        let state = AppState::new();
        let id = Uuid::new_v4();
        let folder:FolderConfig=serde_json::from_value(serde_json::json!({"id":id,"name":"fixture","localPath":root.join("local").to_string_lossy(),"syncMode":"manual","backend":{"type":"webdav","url":"http://127.0.0.1:1","username":"","password":"","rootPath":""}})).unwrap();
        let mut cfg = state.config.lock().await;
        cfg.device_id = "fixture".into();
        cfg.folders.push(folder.clone());
        cfg.save(&root).unwrap();
        state
            .manager
            .start_folder(&folder, &cfg.settings)
            .await
            .unwrap();
        // Poll a deletion while update owns the config transaction, then perform
        // update's stop/start before releasing that transaction.
        let mut removal = Box::pin(remove_folder_inner(&state, id.to_string()));
        assert!(futures::poll!(removal.as_mut()).is_pending());
        state.manager.stop_folder(&id).await;
        state
            .manager
            .start_folder(&folder, &cfg.settings)
            .await
            .unwrap();
        drop(cfg);
        removal.await.unwrap();
        assert!(state.config.lock().await.folders.is_empty());
        assert!(state.manager.statuses().await.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
