//! Windows 开机自启动管理（通过注册表 HKCU\Software\Microsoft\Windows\CurrentVersion\Run）。

#[cfg(target_os = "windows")]
const REG_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(target_os = "windows")]
const APP_NAME: &str = "SyncCloud";

/// 检查当前是否已启用开机自启动。
pub fn is_autostart_enabled() -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let output = std::process::Command::new("reg")
            .args(["query", REG_KEY, "/v", APP_NAME])
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

/// 启用或禁用开机自启动。
pub fn set_autostart_enabled(enabled: bool) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        if enabled {
            let exe = std::env::current_exe()?;
            let exe_str = exe.to_string_lossy().to_string();
            let val = format!("\"{exe_str}\"");
            let output = std::process::Command::new("reg")
                .args(["add", REG_KEY, "/v", APP_NAME, "/t", "REG_SZ", "/d", &val, "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()?;
            if !output.status.success() {
                let err = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("设置开机自启动失败: {err}");
            }
        } else {
            let output = std::process::Command::new("reg")
                .args(["delete", REG_KEY, "/v", APP_NAME, "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()?;
            // 若键本就不存在，reg delete 返回 1，无需视为错误
            let _ = output;
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        anyhow::bail!("当前操作系统暂不支持开机自启动设置");
    }
}
