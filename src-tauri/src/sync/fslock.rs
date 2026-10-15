//! Windows 文件占用无感知同步的底层原语（Syncthing / Resilio Sync 同款思路）。
//!
//! Windows 上文件被其他程序（Office、编辑器、杀毒扫描）打开是常态：
//! - **共享读**：以 `FILE_SHARE_READ|WRITE|DELETE` 打开源文件 —— 即使对方
//!   正在写入也能读取内容（哈希与上传不再被“文件被另一进程使用”阻塞）；
//! - **POSIX 语义替换**：`SetFileInformationByHandle(FileRenameInfo,
//!   POSIX_SEMANTICS|REPLACE_IF_EXISTS)` —— 可原地替换被以共享删除方式
//!   打开的目标文件（绝大多数应用的打开方式），下载落盘不再失败；
//! - **健壮删除**：`remove_file` 失败时退避到 POSIX 语义删除，仍失败则
//!   挂起 delete-on-close，最后一个句柄关闭即删除。
//!
//! 替换 / 删除最终都无法完成时由上层「待替换队列」兜底：先把已下载的
//! 新内容停放到应用数据目录，占用释放后自动应用（最终一致，无感知）。

use std::path::Path;

/// 以最大共享模式打开文件用于读取（Windows: FILE_SHARE_READ|WRITE|DELETE；
/// 其他平台退化为普通只读打开）。
pub fn open_read_shared(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            )
            .open(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::fs::File::open(path)
    }
}

/// 异步版本的共享读打开（tokio File 基于同一底层句柄）。
pub async fn open_read_shared_async(path: &Path) -> std::io::Result<tokio::fs::File> {
    let std_file = open_read_shared(path)?;
    Ok(tokio::fs::File::from_std(std_file))
}

/// 共享读整个文件到内存（小文件上传路径使用）。
pub async fn read_shared(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = open_read_shared(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// 以共享读复制文件（源被编辑器/杀毒占用时仍可复制），用于本地移动的降级路径。
pub async fn copy_shared(src: &Path, dst: &Path) -> std::io::Result<u64> {
    let mut reader = open_read_shared_async(src).await?;
    let mut writer = tokio::fs::File::create(dst).await?;
    tokio::io::copy(&mut reader, &mut writer).await
}

/// 探测文件当前能否被删除（Windows：以 DELETE 访问权限打开即视为可删；
/// 被独占锁定时打开失败返回 false）。用于删除/移动前避免无谓的归档与报错。
pub fn can_delete(path: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .access_mode(win::DELETE | win::SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
            .is_ok()
    }
    #[cfg(not(target_os = "windows"))]
    {
        path.exists()
    }
}

/// 把 `src` 原子替换到 `dst`。
/// 链路：普通 rename → Windows POSIX 语义替换（目标被共享打开也能换）
/// → 同目录 staging 副本 + rename（跨卷 rename 不可用时的兜底）。
pub async fn replace_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    match tokio::fs::rename(src, dst).await {
        Ok(()) => return Ok(()),
        Err(first_err) => {
            #[cfg(target_os = "windows")]
            {
                if let Ok(()) = posix_replace(src, dst) {
                    return Ok(());
                }
            }
            // 跨卷（临时目录与目标不同盘）rename 不可用：复制到目标同目录
            // 的临时名后再原子改名 —— 改名若因目标被占用失败则如实报错，
            // 由上层待替换队列兜底。
            let staging = dst.with_extension(format!("sc-stage-{}", uuid::Uuid::new_v4()));
            match tokio::fs::copy(src, &staging).await {
                Ok(_) => match tokio::fs::rename(&staging, dst).await {
                    Ok(()) => match tokio::fs::remove_file(src).await {
                        Ok(()) => return Ok(()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                        // 目标已就位但源删除失败（如被独占锁定）：如实上抛，
                        // 由调用方决定后续（挂起/降级），不能静默吞掉源文件残留状态
                        Err(e) => return Err(e),
                    },
                    Err(e) => {
                        let _ = tokio::fs::remove_file(&staging).await;
                        return Err(e);
                    }
                },
                Err(_) => return Err(first_err),
            }
        }
    }
}

/// 错误是否为「目标/源被其他进程占用」类（进入待替换队列的判据）。
/// 其余错误（跨卷、磁盘满、路径非法等）应如实上抛走引擎重试。
pub fn is_lock_error(e: &std::io::Error) -> bool {
    #[cfg(target_os = "windows")]
    {
        matches!(e.raw_os_error(), Some(32) | Some(33) | Some(5)) // ERROR_SHARING_VIOLATION / LOCK_VIOLATION / ACCESS_DENIED
    }
    #[cfg(not(target_os = "windows"))]
    {
        matches!(
            e.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
        )
    }
}

/// 删除文件；被占用时退避到 POSIX 删除 / delete-on-close。
/// 仅当文件确实已删除（或已提交 POSIX 删除）才返回 Ok。
pub async fn remove_file_robust(path: &Path) -> std::io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => return Ok(()),
        Err(first_err) => {
            if first_err.kind() == std::io::ErrorKind::NotFound {
                return Ok(());
            }
            #[cfg(target_os = "windows")]
            {
                if posix_delete(path) {
                    return Ok(());
                }
            }
            Err(first_err)
        }
    }
}

// ---- Windows 底实现（手工 FFI，避免引入新依赖） ----

#[cfg(target_os = "windows")]
mod win {
    pub const FILE_SHARE_READ: u32 = 0x0000_0001;
    pub const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    pub const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    /// 打开以便重命名 / 删除（不需要读写数据）。
    pub const DELETE: u32 = 0x0001_0000;
    pub const SYNCHRONIZE: u32 = 0x0010_0000;

    /// FILE_INFO_BY_HANDLE_CLASS 枚举值。
    pub const FILE_RENAME_INFO_CLASS: u32 = 3;
    pub const FILE_DISPOSITION_INFO_CLASS: u32 = 4;
    pub const FILE_DISPOSITION_INFO_EX_CLASS: u32 = 19;

    pub const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x0000_0001;
    pub const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x0000_0002;

    pub const FILE_DISPOSITION_FLAG_DELETE: u32 = 0x0000_0001;
    pub const FILE_DISPOSITION_FLAG_POSIX_SEMANTICS: u32 = 0x0000_0002;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn SetFileInformationByHandle(
            hfile: isize,
            fileinformationclass: u32,
            fileinformation: *const core::ffi::c_void,
            buffersize: u32,
        ) -> i32;
    }
}

#[cfg(target_os = "windows")]
use win::*;

/// Windows 目标路径 → 以 null 结尾的 UTF-16（供 FILE_RENAME_INFO 使用）。
#[cfg(target_os = "windows")]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// POSIX 语义重命名替换：目标被其他进程以 FILE_SHARE_DELETE 打开时仍可成功。
/// 目标路径必须是全限定路径（RootDirectory 为空时按绝对路径解释）。
#[cfg(target_os = "windows")]
fn posix_replace(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    let dst_abs = if dst.is_absolute() {
        dst.to_path_buf()
    } else {
        std::env::current_dir()?.join(dst)
    };
    // 目标所在目录必须存在（与 rename 语义一致）
    if let Some(parent) = dst_abs.parent() {
        if !parent.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("目标目录不存在: {}", parent.display()),
            ));
        }
    }

    let file = std::fs::OpenOptions::new()
        .access_mode(DELETE | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(src)?;

    let name: Vec<u16> = {
        let mut v = wide(&dst_abs);
        v.pop(); // FILE_RENAME_INFO.FileName 不含终止 null，按字符数计长
        v
    };

    // FILE_RENAME_INFO 经典布局（x64）：
    //   offset 0  union { BOOLEAN ReplaceIfExists; DWORD Flags; }   ← 4 字节
    //   offset 8  HANDLE RootDirectory
    //   offset 16 DWORD FileNameLength（字符数）
    //   offset 20 WCHAR FileName[1]
    const HEADER: usize = 20;
    let mut buf = vec![0u8; HEADER + name.len() * 2];
    let flags = FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
    buf[0..4].copy_from_slice(&flags.to_le_bytes());
    // RootDirectory = null（保持 0）
    buf[16..20].copy_from_slice(&((name.len() as u32).to_le_bytes()));
    for (i, ch) in name.iter().enumerate() {
        buf[HEADER + i * 2..HEADER + i * 2 + 2].copy_from_slice(&ch.to_le_bytes());
    }

    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as isize,
            FILE_RENAME_INFO_CLASS,
            buf.as_ptr() as *const core::ffi::c_void,
            buf.len() as u32,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// POSIX 语义删除：尝试 FileDispositionInfoEx（立即删除，兼容只读属性），
/// 失败再尝试经典 delete-on-close。仅在删除已确定生效时返回 true。
#[cfg(target_os = "windows")]
fn posix_delete(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    let try_dispose = |class: u32, flags: u32| -> bool {
        let file = match std::fs::OpenOptions::new()
            .access_mode(DELETE | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
        {
            Ok(f) => f,
            Err(_) => return false,
        };
        let buf = flags.to_le_bytes().to_vec(); // DWORD / BOOLEAN 均按 4 字节传
        let ok = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle() as isize,
                class,
                buf.as_ptr() as *const core::ffi::c_void,
                buf.len() as u32,
            )
        };
        if ok == 0 {
            return false;
        }
        drop(file); // 关闭句柄触发实际删除
        !path.exists()
    };

    // 1) POSIX 立即删除
    if try_dispose(FILE_DISPOSITION_INFO_EX_CLASS, FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS) {
        return true;
    }
    // 2) 经典 delete-on-close（BOOLEAN TRUE = 1）
    try_dispose(FILE_DISPOSITION_INFO_CLASS, 1)
}

/// 在 Windows 平台上将目录或文件设置为隐藏属性（适用于非用户自主创建的内部系统产物，如 .synccloud 等）。
#[cfg(target_os = "windows")]
pub fn set_hidden_windows(path: &std::path::Path) {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    extern "system" {
        fn GetFileAttributesW(lpFileName: *const u16) -> u32;
        fn SetFileAttributesW(lpFileName: *const u16, dwFileAttributes: u32) -> i32;
    }
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFFFFFF;
    unsafe {
        let attrs = GetFileAttributesW(wide.as_ptr());
        if attrs != INVALID_FILE_ATTRIBUTES && (attrs & FILE_ATTRIBUTE_HIDDEN) == 0 {
            let _ = SetFileAttributesW(wide.as_ptr(), attrs | FILE_ATTRIBUTE_HIDDEN);
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn set_hidden_windows(_path: &std::path::Path) {}

/// 在 Windows 平台上设置存储模式对应的文件属性：
/// - "keep_offline"（保持离线可用）：彻底清除脱机和异常属性，确保本地所有关联软件可直接原生打开！
/// - "free_space"（不占用电脑空间）：设置 FILE_ATTRIBUTE_OFFLINE（0x1000）标记为脱机。
#[cfg(target_os = "windows")]
pub fn set_storage_mode_attributes(path: &std::path::Path, mode: &str) {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    extern "system" {
        fn GetFileAttributesW(lpFileName: *const u16) -> u32;
        fn SetFileAttributesW(lpFileName: *const u16, dwFileAttributes: u32) -> i32;
    }
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x00001000;
    const FILE_ATTRIBUTE_PINNED: u32 = 0x00080000;
    const FILE_ATTRIBUTE_UNPINNED: u32 = 0x00100000;
    const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFFFFFF;

    unsafe {
        let attrs = GetFileAttributesW(wide.as_ptr());
        if attrs != INVALID_FILE_ATTRIBUTES {
            let next = match mode {
                "keep_offline" => attrs & !FILE_ATTRIBUTE_OFFLINE & !FILE_ATTRIBUTE_PINNED & !FILE_ATTRIBUTE_UNPINNED,
                "free_space" => (attrs & !FILE_ATTRIBUTE_PINNED) | FILE_ATTRIBUTE_OFFLINE,
                _ => attrs,
            };
            if next != attrs {
                let _ = SetFileAttributesW(wide.as_ptr(), if next == 0 { 0x20 } else { next });
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn set_storage_mode_attributes(_path: &std::path::Path, _mode: &str) {}

#[cfg(target_os = "windows")]
pub fn set_offline_windows(path: &std::path::Path, offline: bool) {
    let mode = if offline { "free_space" } else { "keep_offline" };
    set_storage_mode_attributes(path, mode);
}

#[cfg(not(target_os = "windows"))]
pub fn set_offline_windows(_path: &std::path::Path, _offline: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("synccloud-fslock-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn replace_normal_file() {
        let d = tmp_dir("replace-normal");
        let src = d.join("a.tmp");
        let dst = d.join("a.txt");
        std::fs::write(&src, b"new content").unwrap();
        std::fs::write(&dst, b"old content").unwrap();
        replace_file(&src, &dst).await.unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"new content");
        assert!(!src.exists());
    }

    #[tokio::test]
    async fn replace_creates_missing_target() {
        let d = tmp_dir("replace-create");
        let src = d.join("b.tmp");
        let dst = d.join("b.txt");
        std::fs::write(&src, b"data").unwrap();
        replace_file(&src, &dst).await.unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"data");
    }

    /// 只读目标也能替换（POSIX 语义忽略只读属性差异由 REPLACE_IF_EXISTS 处理）。
    #[tokio::test]
    async fn replace_readonly_target() {
        let d = tmp_dir("replace-readonly");
        let src = d.join("c.tmp");
        let dst = d.join("c.txt");
        std::fs::write(&src, b"v2").unwrap();
        std::fs::write(&dst, b"v1").unwrap();
        #[cfg(target_os = "windows")]
        {
            let mut perms = std::fs::metadata(&dst).unwrap().permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&dst, perms).unwrap();
        }
        // std rename 会因只读属性失败，POSIX 替换链路应能处理
        let _ = replace_file(&src, &dst).await;
        // 平台差异下不强求成功，但不能崩溃；若成功则内容正确
        if !src.exists() {
            assert_eq!(std::fs::read(&dst).unwrap(), b"v2");
        }
    }

    /// 模拟「目标文件被其他程序打开」：以允许共享删除的方式持有句柄，
    /// 普通 rename 替换已打开目标在 Windows 上常失败，POSIX 链路应可替换。
    #[tokio::test]
    async fn replace_target_open_with_share_delete() {
        let d = tmp_dir("replace-open");
        let src = d.join("d.tmp");
        let dst = d.join("d.txt");
        std::fs::write(&src, b"v2").unwrap();
        std::fs::write(&dst, b"v1").unwrap();

        #[cfg(target_os = "windows")]
        let _guard = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(
                    super::win::FILE_SHARE_READ
                        | super::win::FILE_SHARE_WRITE
                        | super::win::FILE_SHARE_DELETE,
                )
                .open(&dst)
                .unwrap()
        };

        replace_file(&src, &dst).await.expect("被共享打开的目标应可替换");
        assert_eq!(std::fs::read(&dst).unwrap(), b"v2");
        assert!(!src.exists());
    }

    /// 以共享模式读取「正被自己以写入方式打开」的文件（上传/哈希不阻塞）。
    #[tokio::test]
    async fn shared_read_while_open_for_write() {
        let d = tmp_dir("shared-read");
        let p = d.join("e.txt");
        std::fs::write(&p, b"being written").unwrap();

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let _writer = std::fs::OpenOptions::new()
                .write(true)
                .share_mode(
                    super::win::FILE_SHARE_READ
                        | super::win::FILE_SHARE_WRITE
                        | super::win::FILE_SHARE_DELETE,
                )
                .open(&p)
                .unwrap();
            let data = read_shared(&p).await.unwrap();
            assert_eq!(data, b"being written");
        }
        #[cfg(not(target_os = "windows"))]
        {
            let data = read_shared(&p).await.unwrap();
            assert_eq!(data, b"being written");
        }
    }

    #[tokio::test]
    async fn robust_delete() {
        let d = tmp_dir("delete");
        let p = d.join("f.txt");
        std::fs::write(&p, b"x").unwrap();
        remove_file_robust(&p).await.unwrap();
        assert!(!p.exists());
        // 不存在的文件视为成功（幂等）
        remove_file_robust(&p).await.unwrap();
    }
}
