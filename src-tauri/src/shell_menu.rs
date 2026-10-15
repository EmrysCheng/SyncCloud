//! Windows 资源管理器右键上下文菜单注册（坚果云同款云桥功能）。
//! 写入 HKCU\Software\Classes\*\shell\SyncCloud 与 Directory\shell\SyncCloud，
//! 提供「保持离线可用」与「释放空间 (不占用电脑空间)」操作。

#[cfg(target_os = "windows")]
const FILE_SHELL_KEY: &str = r"HKCU\Software\Classes\*\shell\SyncCloud";
#[cfg(target_os = "windows")]
const DIR_SHELL_KEY: &str = r"HKCU\Software\Classes\Directory\shell\SyncCloud";

/// 检查当前是否已启用 Windows 资源管理器右键快捷菜单。
pub fn is_shell_menu_enabled() -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let output = std::process::Command::new("reg")
            .args(["query", FILE_SHELL_KEY])
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
            let exe = std::env::current_exe()?;
            let exe_str = exe.to_string_lossy().to_string();

            for root_key in [FILE_SHELL_KEY, DIR_SHELL_KEY] {
                // 1. 主菜单项
                let _ = std::process::Command::new("reg")
                    .args(["add", root_key, "/v", "MUIVerb", "/t", "REG_SZ", "/d", "SyncCloud", "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;
                let _ = std::process::Command::new("reg")
                    .args(["add", root_key, "/v", "Icon", "/t", "REG_SZ", "/d", &format!("\"{exe_str}\""), "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;
                let _ = std::process::Command::new("reg")
                    .args(["add", root_key, "/v", "subcommands", "/t", "REG_SZ", "/d", "", "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;

                // 2. 子菜单 1: 保持离线可用
                let sub_keep = format!("{root_key}\\shell\\cmd_keep");
                let _ = std::process::Command::new("reg")
                    .args(["add", &sub_keep, "/ve", "/t", "REG_SZ", "/d", "保持离线可用", "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;
                let sub_keep_cmd = format!("{sub_keep}\\command");
                let keep_cmd_val = format!("\"{exe_str}\" --keep-offline \"%1\"");
                let _ = std::process::Command::new("reg")
                    .args(["add", &sub_keep_cmd, "/ve", "/t", "REG_SZ", "/d", &keep_cmd_val, "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;

                // 3. 子菜单 2: 释放空间 (不占用电脑空间)
                let sub_free = format!("{root_key}\\shell\\cmd_free");
                let _ = std::process::Command::new("reg")
                    .args(["add", &sub_free, "/ve", "/t", "REG_SZ", "/d", "释放空间 (不占用电脑空间)", "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;
                let sub_free_cmd = format!("{sub_free}\\command");
                let free_cmd_val = format!("\"{exe_str}\" --free-space \"%1\"");
                let _ = std::process::Command::new("reg")
                    .args(["add", &sub_free_cmd, "/ve", "/t", "REG_SZ", "/d", &free_cmd_val, "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()?;
            }
        } else {
            for root_key in [FILE_SHELL_KEY, DIR_SHELL_KEY] {
                let _ = std::process::Command::new("reg")
                    .args(["delete", root_key, "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output();
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        anyhow::bail!("当前操作系统暂不支持资源管理器右键扩展");
    }
}
