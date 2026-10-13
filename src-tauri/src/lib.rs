//! SyncCloud 应用装配。

pub mod config;
pub mod credential;
pub mod crypto;
pub mod storage;
pub mod sync;
pub mod commands;
pub mod autostart;

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, WindowEvent};
use tokio::sync::Mutex;

use crate::config::{app_data_dir, AppConfig};
use crate::sync::engine::SyncEvent;
use crate::sync::manager::FolderManager;

pub struct AppState {
    pub manager: FolderManager,
    pub config: Mutex<AppConfig>,
    /// 最近事件环形缓冲（前端实时走 Tauri event，此处供补拉）
    pub recent_events: Mutex<Vec<SyncEvent>>,
}

/// 「关闭窗口 → 最小化到托盘」的运行时开关。
/// 窗口事件回调是同步上下文拿不到异步配置锁，用原子变量镜像配置，
/// 保存设置时即时更新。
pub(crate) static CLOSE_TO_TRAY: AtomicBool = AtomicBool::new(true);

static AUTOSTART_ITEM: std::sync::Mutex<Option<CheckMenuItem<tauri::Wry>>> = std::sync::Mutex::new(None);

pub fn update_autostart_menu(checked: bool) {
    if let Ok(guard) = AUTOSTART_ITEM.lock() {
        if let Some(item) = guard.as_ref() {
            let _ = item.set_checked(checked);
        }
    }
}

const MAX_RECENT_EVENTS: usize = 500;

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_dir = app_data_dir();
            std::fs::create_dir_all(&data_dir)?;
            let cfg = AppConfig::load_or_create(&data_dir)?;
            CLOSE_TO_TRAY.store(cfg.settings.close_to_tray, Ordering::Relaxed);

            let state = AppState::new();
            // 先订阅再移交所有权
            let mut rx = state.manager.subscribe();
            app.manage(state);

            // 事件转发：引擎 broadcast → 窗口事件 + 环形缓冲
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            let _ = handle.emit("synccloud://event", &ev);
                            if let Some(state) = handle.try_state::<AppState>() {
                                let mut buf = state.recent_events.lock().await;
                                buf.push(ev);
                                if buf.len() > MAX_RECENT_EVENTS {
                                    let drop_n = buf.len() - MAX_RECENT_EVENTS;
                                    buf.drain(0..drop_n);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });

            // 启动所有已启用文件夹的同步任务
            let handle2 = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle2.state::<AppState>();
                let cfg_snapshot = {
                    let mut guard = state.config.lock().await;
                    *guard = cfg;
                    guard.clone()
                };
                state.manager.start_all(&cfg_snapshot).await;
            });

            // 系统托盘：显示主界面 / 立即同步全部 / 开机启动 / 退出
            let show_item = MenuItem::with_id(app, "tray-show", "显示主界面", true, None::<&str>)?;
            let sync_item = MenuItem::with_id(app, "tray-sync", "立即同步全部", true, None::<&str>)?;
            let data_dir_item = MenuItem::with_id(app, "tray-data-dir", "打开数据目录", true, None::<&str>)?;
            let autostart_enabled = crate::autostart::is_autostart_enabled();
            let autostart_item = CheckMenuItem::with_id(
                app,
                "tray-autostart",
                "开机自启动",
                true,
                autostart_enabled,
                None::<&str>,
            )?;
            if let Ok(mut guard) = AUTOSTART_ITEM.lock() {
                *guard = Some(autostart_item.clone());
            }
            let quit_item = MenuItem::with_id(app, "tray-quit", "退出 SyncCloud", true, None::<&str>)?;
            let sep1 = PredefinedMenuItem::separator(app)?;
            let sep2 = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(
                app,
                &[&show_item, &sync_item, &data_dir_item, &sep1, &autostart_item, &sep2, &quit_item],
            )?;
            let icon = app
                .default_window_icon()
                .ok_or_else(|| anyhow::anyhow!("缺少应用图标，无法创建托盘"))?
                .clone();
            TrayIconBuilder::with_id("synccloud-tray")
                .icon(icon)
                .tooltip("SyncCloud 文件同步")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "tray-show" => show_main_window(app),
                    "tray-sync" => {
                        let handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state = handle.state::<AppState>();
                            for s in state.manager.statuses().await {
                                let _ = state.manager.sync_now(&s.folder_id).await;
                            }
                        });
                    }
                    "tray-data-dir" => {
                        let _ = commands::open_data_dir();
                    }
                    "tray-autostart" => {
                        let current = crate::autostart::is_autostart_enabled();
                        let next = !current;
                        if let Err(e) = crate::autostart::set_autostart_enabled(next) {
                            tracing::error!(error = %e, "切换开机启动失败");
                        } else {
                            update_autostart_menu(next);
                        }
                    }
                    "tray-quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // 左键单击：显示/聚焦主窗口（与主流云盘客户端一致）
                    if matches!(
                        event,
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        }
                    ) {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // 坚果云式行为：关窗退到托盘常驻同步；退出走托盘菜单
                if CLOSE_TO_TRAY.load(Ordering::Relaxed) && window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_app_info,
            commands::save_settings,
            commands::get_settings,
            commands::open_external,
            commands::list_folders,
            commands::add_folder,
            commands::update_folder,
            commands::remove_folder,
            commands::sync_now,
            commands::pause_folder,
            commands::resume_folder,
            commands::test_backend,
            commands::list_conflicts,
            commands::resolve_conflict,
            commands::get_recent_events,
            commands::baidu_login_start,
            commands::baidu_login_poll,
            commands::aliyun_qr_start,
            commands::aliyun_qr_poll,
            commands::aliyun_authorized,
            commands::baidu_authorized,
            commands::is_autostart,
            commands::set_autostart,
            commands::list_cloud_accounts,
            commands::delete_cloud_account,
            commands::save_cloud_account,
            commands::change_data_dir,
            commands::restart_app,
            commands::open_path_in_explorer,
            commands::open_data_dir,
            commands::sync_all_folders,
            commands::pause_all_folders,
            commands::resume_all_folders,
        ])
        .run(tauri::generate_context!())
        .expect("SyncCloud 启动失败");
}
