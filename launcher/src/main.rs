// Prevents console window on Windows release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use flate2::read::DeflateDecoder;

include!(concat!(env!("OUT_DIR"), "/version_hash.rs"));

const SYNCCLOUD_DEFLATE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/synccloud.deflate"));
const WEBVIEW2_DEFLATE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/webview2.deflate"));

/// 解压指定数据流到目标文件。支持 Windows 运行中文件重命名更新技巧。
fn decompress_to_file(compressed: &[u8], target_path: &Path) -> std::io::Result<()> {
    let mut decoder = DeflateDecoder::new(compressed);
    let temp_path = target_path.with_extension("tmp");
    {
        let mut out_file = File::create(&temp_path)?;
        let mut buf = [0u8; 65536];
        loop {
            let n = decoder.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out_file.write_all(&buf[..n])?;
        }
        out_file.flush()?;
    }

    if target_path.exists() {
        // 如果文件存在，先尝试直接覆盖或删除
        if let Err(_) = fs::remove_file(target_path) {
            // 若被运行中的旧进程锁定，尝试将其重命名为 .old
            let old_path = target_path.with_extension("old");
            let _ = fs::remove_file(&old_path);
            let _ = fs::rename(target_path, &old_path);
        }
    }

    fs::rename(&temp_path, target_path)?;
    Ok(())
}

/// 获取应用核心执行目录（优先 %LOCALAPPDATA%\SyncCloud\app）
fn get_app_dir() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local_app_data).join("SyncCloud").join("app");
        if fs::create_dir_all(&p).is_ok() {
            return p;
        }
    }
    if let Ok(user_profile) = std::env::var("USERPROFILE") {
        let p = PathBuf::from(user_profile).join("AppData").join("Local").join("SyncCloud").join("app");
        if fs::create_dir_all(&p).is_ok() {
            return p;
        }
    }
    let p = std::env::temp_dir().join("SyncCloud").join("app");
    let _ = fs::create_dir_all(&p);
    p
}

/// 弹出系统错误弹窗（在启动异常时）
#[cfg(target_os = "windows")]
fn show_error(msg: &str) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    let wide_msg: Vec<u16> = OsStr::new(msg).encode_wide().chain(Some(0)).collect();
    let wide_title: Vec<u16> = OsStr::new("SyncCloud 启动失败").encode_wide().chain(Some(0)).collect();

    extern "system" {
        fn MessageBoxW(
            hwnd: *mut std::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            flags: u32,
        ) -> i32;
    }

    unsafe {
        MessageBoxW(std::ptr::null_mut(), wide_msg.as_ptr(), wide_title.as_ptr(), 0x10);
    }
}

#[cfg(not(target_os = "windows"))]
fn show_error(msg: &str) {
    eprintln!("{}", msg);
}

fn main() {
    let app_dir = get_app_dir();
    let target_exe = app_dir.join("synccloud.exe");
    let target_dll = app_dir.join("WebView2Loader.dll");
    let version_file = app_dir.join("version.txt");

    // 检查是否已经在解压后的核心目录直接运行（防止自身递归拉起）
    if let Ok(current_exe) = std::env::current_exe() {
        if current_exe == target_exe {
            return;
        }
    }

    // 尝试清理上一次遗留的 .old 文件
    let _ = fs::remove_file(target_exe.with_extension("old"));
    let _ = fs::remove_file(target_dll.with_extension("old"));

    // 检查是否需要释放或更新文件
    let mut need_extract = true;
    if target_exe.exists() && target_dll.exists() && version_file.exists() {
        if let Ok(existing_hash) = fs::read_to_string(&version_file) {
            if existing_hash.trim() == VERSION_HASH.trim() {
                need_extract = false;
            }
        }
    }

    if need_extract {
        // 如果需要更新版本，先结束可能残留的旧版本运行进程（排除自身 PID）
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            let my_pid = std::process::id();
            let filter = format!("PID ne {}", my_pid);
            let _ = Command::new("taskkill")
                .args(["/F", "/FI", &filter, "/IM", "synccloud.exe"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        if let Err(e) = decompress_to_file(SYNCCLOUD_DEFLATE, &target_exe) {
            if !target_exe.exists() {
                show_error(&format!("无法初始化 SyncCloud 主程序：\n{}", e));
                return;
            }
        }
        if let Err(e) = decompress_to_file(WEBVIEW2_DEFLATE, &target_dll) {
            if !target_dll.exists() {
                show_error(&format!("无法初始化 WebView2Loader 依赖：\n{}", e));
                return;
            }
        }
        let _ = fs::write(&version_file, VERSION_HASH);
    }

    // 启动主程序并透传所有参数与工作目录
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let mut cmd = Command::new(&target_exe);
    cmd.current_dir(&app_dir);
    cmd.args(&args);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP = 0x00000200
        cmd.creation_flags(0x00000200);
    }

    match cmd.spawn() {
        Ok(_) => {
            // 成功启动，退出当前启动器进程
            std::process::exit(0);
        }
        Err(e) => {
            show_error(&format!("无法启动 SyncCloud 核心服务：\n{}", e));
        }
    }
}
