//! Windows 资源管理器右键上下文菜单注册与云同步空间关联（坚果云同款云桥功能）。
//! 注册在 HKCU\Software\Classes 下的 *、Directory、Folder 及 Directory\Background，
//! 严格对标坚果云实图样式：
//!   SyncCloud >
//!     ✓ 保持离线可用
//!     ☁ 不占用电脑空间

#[cfg(target_os = "windows")]
const SHELL_ROOTS: &[&str] = &[
    r"HKCU\Software\Classes\*\shell\SyncCloud",
    r"HKCU\Software\Classes\Directory\shell\SyncCloud",
    r"HKCU\Software\Classes\Directory\Background\shell\SyncCloud",
];

/// 检查当前是否已启用 Windows 资源管理器右键快捷菜单。
pub fn is_shell_menu_enabled() -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let output = std::process::Command::new("reg")
            .args(["query", SHELL_ROOTS[1]])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        if let Ok(out) = output {
            return out.status.success();
        }
        false
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// 启用或禁用 Windows 资源管理器右键快捷菜单。
pub fn set_shell_menu_enabled(enabled: bool) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        if enabled {
            // 清理可能导致文件夹右键菜单重复的旧版 Folder 项
            let _ = std::process::Command::new("reg")
                .args(["delete", r"HKCU\Software\Classes\Folder\shell\SyncCloud", "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();

            let exe = std::env::current_exe()?;
            let exe_str = exe.to_string_lossy().to_string();

            for root_key in SHELL_ROOTS {
                // 清理旧可能存在的 ExtendedSubCommandsKey（避免 Windows Explorer 重复枚举导致子菜单项翻倍）
                let _ = std::process::Command::new("reg")
                    .args(["delete", root_key, "/v", "ExtendedSubCommandsKey", "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output();
                let _ = std::process::Command::new("reg")
                    .args(["delete", &format!("{root_key}\\Shell"), "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output();

                // 1. 主菜单项（SyncCloud + 图标，标准的纯静态级联子键规范）
                let _ = std::process::Command::new("reg")
                    .args(["add", root_key, "/v", "MUIVerb", "/t", "REG_SZ", "/d", "SyncCloud", "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;
                let _ = std::process::Command::new("reg")
                    .args(["add", root_key, "/v", "Icon", "/t", "REG_SZ", "/d", &format!("\"{exe_str}\""), "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;
                let _ = std::process::Command::new("reg")
                    .args(["add", root_key, "/v", "SubCommands", "/t", "REG_SZ", "/d", "", "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;

                // 2. 子菜单 1: ✓ 保持离线可用（排第 1 项，坚果云实图同款）
                let sub_keep = format!("{root_key}\\Shell\\01_KeepOffline");
                let _ = std::process::Command::new("reg")
                    .args(["add", &sub_keep, "/ve", "/t", "REG_SZ", "/d", "✓ 保持离线可用", "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;
                let _ = std::process::Command::new("reg")
                    .args(["add", &sub_keep, "/v", "Icon", "/t", "REG_SZ", "/d", &format!("\"{exe_str}\""), "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;
                let sub_keep_cmd = format!("{sub_keep}\\command");
                let keep_cmd_val = format!("\"{exe_str}\" --keep-offline \"%1\"");
                let _ = std::process::Command::new("reg")
                    .args(["add", &sub_keep_cmd, "/ve", "/t", "REG_SZ", "/d", &keep_cmd_val, "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;

                // 3. 子菜单 2: ☁ 不占用电脑空间（排第 2 项，坚果云实图同款）
                let sub_free = format!("{root_key}\\Shell\\02_FreeSpace");
                let _ = std::process::Command::new("reg")
                    .args(["add", &sub_free, "/ve", "/t", "REG_SZ", "/d", "☁ 不占用电脑空间", "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;
                let _ = std::process::Command::new("reg")
                    .args(["add", &sub_free, "/v", "Icon", "/t", "REG_SZ", "/d", &format!("\"{exe_str}\""), "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;
                let sub_free_cmd = format!("{sub_free}\\command");
                let free_cmd_val = format!("\"{exe_str}\" --free-space \"%1\"");
                let _ = std::process::Command::new("reg")
                    .args(["add", &sub_free_cmd, "/ve", "/t", "REG_SZ", "/d", &free_cmd_val, "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;
            }

            // 注册 .synccloud 脱机占位文件类型（双击自动下载恢复并正常打开）
            let dot_key = r"HKCU\Software\Classes\.synccloud";
            let _ = std::process::Command::new("reg")
                .args(["add", dot_key, "/ve", "/t", "REG_SZ", "/d", "SyncCloud.Placeholder", "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()?;

            let prog_key = r"HKCU\Software\Classes\SyncCloud.Placeholder";
            let _ = std::process::Command::new("reg")
                .args(["add", prog_key, "/ve", "/t", "REG_SZ", "/d", "SyncCloud 云占位文件", "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()?;
            let _ = std::process::Command::new("reg")
                .args(["add", prog_key, "/v", "NeverShowExt", "/t", "REG_SZ", "/d", "", "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()?;
            let _ = std::process::Command::new("reg")
                .args(["add", prog_key, "/v", "Icon", "/t", "REG_SZ", "/d", &format!("\"{exe_str}\""), "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()?;

            let open_cmd_key = format!("{prog_key}\\shell\\open\\command");
            let open_cmd_val = format!("\"{exe_str}\" --open-hydrate \"%1\"");
            let _ = std::process::Command::new("reg")
                .args(["add", &open_cmd_key, "/ve", "/t", "REG_SZ", "/d", &open_cmd_val, "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()?;
        } else {
            let _ = std::process::Command::new("reg")
                .args(["delete", r"HKCU\Software\Classes\Folder\shell\SyncCloud", "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            for root_key in SHELL_ROOTS {
                let _ = std::process::Command::new("reg")
                    .args(["delete", root_key, "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output();
            }
            let _ = std::process::Command::new("reg")
                .args(["delete", r"HKCU\Software\Classes\.synccloud", "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            let _ = std::process::Command::new("reg")
                .args(["delete", r"HKCU\Software\Classes\SyncCloud.Placeholder", "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        anyhow::bail!("当前操作系统暂不支持资源管理器右键扩展");
    }
}

/// 启动时确保右键菜单与占位符关联已就绪（开箱即用，对齐坚果云体验）。
pub fn ensure_shell_menu_registered() {
    #[cfg(target_os = "windows")]
    {
        let _ = set_shell_menu_enabled(true);
    }
}

#[cfg(target_os = "windows")]
fn current_user_sid() -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let out = std::process::Command::new("whoami")
        .arg("/user")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[1].starts_with("S-1-5-") {
            return Some(parts[1].to_string());
        }
    }
    None
}

/// 将同步文件夹注册到 Windows 10/11 原生 SyncRootManager（坚果云同款，自动激活资源管理器「状态」列）。
#[cfg(target_os = "windows")]
pub fn register_sync_root(folder_id: &str, folder_name: &str, local_path: &str) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    if let Ok(exe) = std::env::current_exe() {
        let exe_str = exe.to_string_lossy().to_string();
        let key = format!(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager\SyncCloud!{folder_id}");
        let _ = std::process::Command::new("reg")
            .args(["add", &key, "/v", "DisplayNameResource", "/t", "REG_SZ", "/d", &format!("SyncCloud ({folder_name})"), "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        let _ = std::process::Command::new("reg")
            .args(["add", &key, "/v", "IconResource", "/t", "REG_SZ", "/d", &exe_str, "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        let user_key = format!("{key}\\UserSyncRoots");
        if let Some(sid) = current_user_sid() {
            let _ = std::process::Command::new("reg")
                .args(["add", &user_key, "/v", &sid, "/t", "REG_SZ", "/d", local_path, "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
        let _ = std::process::Command::new("reg")
            .args(["add", &user_key, "/ve", "/t", "REG_SZ", "/d", local_path, "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }
}

#[cfg(not(target_os = "windows"))]
pub fn register_sync_root(_folder_id: &str, _folder_name: &str, _local_path: &str) {}
