//! 坚果云同款云桥按需存储管理（保持离线可用 / 释放空间不占用电脑空间）。
//! 负责本地文件的脱机占位截断、Windows 脱机属性打标，以及按需回拉下载。

use std::path::{Path, PathBuf};
use tauri::{Emitter, Manager};
use tracing::{info, warn};

use crate::config::{folder_data_dir, FolderConfig};
use crate::storage::build_store;
use crate::sync::fslock::{set_storage_mode_attributes, replace_file};
use crate::sync::scanner::{abs_path, rel_path, hash_file};
use crate::sync::state::StateDb;
use crate::AppState;

/// 对单个文件执行「保持离线可用」：将云端完整数据拉取并恢复本地物理大小。
pub async fn keep_offline_file(
    folder: &FolderConfig,
    db: &StateDb,
    rel: &str,
) -> anyhow::Result<bool> {
    let root = PathBuf::from(&folder.local_path);
    let target = abs_path(&root, rel);

    // 检查是否已经是本地完整文件且不在占位列表中
    if !db.is_placeholder(rel) && target.is_file() {
        if let Ok(m) = std::fs::metadata(&target) {
            if m.len() > 0 {
                // 已是完整离线文件，确保脱机属性已清除并保持实心绿圈白勾
                set_storage_mode_attributes(&target, "keep_offline");
                return Ok(false);
            }
        }
    }

    // 从存储后端下载真实内容
    let store = build_store(&folder.backend).await?;
    let tmp = std::env::temp_dir().join(format!("synccloud-hydrate-{}", uuid::Uuid::new_v4()));
    store.get_file(rel, &tmp).await?;

    if let Some(parent) = target.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    replace_file(&tmp, &target).await?;
    set_storage_mode_attributes(&target, "keep_offline");
    db.remove_placeholder(rel);

    info!(folder = %folder.name, path = %rel, "已设为保持离线可用（完整下载恢复）");
    Ok(true)
}

/// 对单个文件执行「释放空间 (不占用电脑空间)」：
/// 校验云端已有完整安全备份后，将本地文件截断为 0 字节并设置 Windows 脱机标记。
pub async fn free_space_file(
    folder: &FolderConfig,
    db: &StateDb,
    rel: &str,
) -> anyhow::Result<bool> {
    let root = PathBuf::from(&folder.local_path);
    let target = abs_path(&root, rel);

    if !target.is_file() {
        return Ok(false);
    }

    // 安全前置校验：必须确认该文件在云端已经成功上传且哈希一致！
    let store = build_store(&folder.backend).await?;
    let manifest = match store.read_manifest().await? {
        Some(m) => m,
        None => {
            anyhow::bail!("云端清单尚未生成，暂无法释放本地空间，请等待首次同步完成");
        }
    };

    let remote_meta = match manifest.files.get(rel) {
        Some(m) => m,
        None => {
            anyhow::bail!("文件尚未上传至云端，无法释放本地空间，请先同步");
        }
    };

    // 获取本地真实大小与哈希（若本地已经是 0 字节占位符，无需重复处理）
    let meta = std::fs::metadata(&target)?;
    let local_size = meta.len();
    if local_size == 0 && db.is_placeholder(rel) {
        set_storage_mode_attributes(&target, "free_space");
        return Ok(false);
    }

    // 校验哈希：确保本地最新内容已同步至云端
    let local_hash = hash_file(&target)?;
    if local_hash != remote_meta.hash {
        anyhow::bail!("本地内容与云端不一致，请先同步上传后再释放空间");
    }

    // 1. 记录真实元数据到数据库占位符表
    db.add_placeholder(rel, remote_meta.size, remote_meta.mtime_ms, &remote_meta.hash);

    // 2. 将本地文件清空截断为 0 字节，释放实际磁盘块
    tokio::fs::write(&target, b"").await?;

    // 3. 赋予 Windows 原生脱机与云端状态属性（FILE_ATTRIBUTE_UNPINNED + FILE_ATTRIBUTE_OFFLINE）
    set_storage_mode_attributes(&target, "free_space");

    info!(folder = %folder.name, path = %rel, freed_bytes = local_size, "已释放电脑本地空间");
    Ok(true)
}

/// 对指定相对路径或全目录批量应用存储模式（保持离线可用或释放空间）。
pub async fn apply_storage_mode_path(
    folder: &FolderConfig,
    db: &StateDb,
    rel_path: &str,
    mode: &str,
) -> anyhow::Result<usize> {
    let root = PathBuf::from(&folder.local_path);
    let target = if rel_path.trim().is_empty() || rel_path == "." || rel_path == "/" {
        root.clone()
    } else {
        abs_path(&root, rel_path)
    };

    let mut files = Vec::new();
    if target.is_file() {
        files.push(rel_path.to_string());
    } else if target.is_dir() {
        for entry in walkdir::WalkDir::new(&target)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            if entry.file_type().is_file() {
                let r = crate::sync::scanner::rel_path(&root, entry.path());
                files.push(r);
            }
        }
    } else {
        anyhow::bail!("目标路径不存在: {rel_path}");
    }

    let mut success_count = 0;
    for rel in files {
        let res = match mode {
            "keep_offline" => keep_offline_file(folder, db, &rel).await,
            "free_space" => free_space_file(folder, db, &rel).await,
            _ => anyhow::bail!("未知的存储模式: {mode}"),
        };
        if let Ok(true) = res {
            success_count += 1;
        }
    }

    // 若目标为目录，对目录本身也设置对应的 Windows 属性
    if target.is_dir() {
        set_storage_mode_attributes(&target, mode);
    }
    Ok(success_count)
}

/// Windows 路径前缀匹配（不区分大小写与正反斜杠）。
fn path_starts_with_case_insensitive(path: &Path, prefix: &Path) -> bool {
    let p_str = path.to_string_lossy().replace('/', "\\").to_lowercase();
    let pre_str = prefix.to_string_lossy().replace('/', "\\").to_lowercase();
    let pre_clean = pre_str.trim_end_matches('\\');
    p_str == pre_clean || p_str.starts_with(&format!("{pre_clean}\\"))
}

/// 处理 Windows 资源管理器右键传参执行（Shell 扩展命令分发）。
pub async fn handle_shell_command(
    app: &tauri::AppHandle,
    command: &str,
    target_path_str: &str,
) {
    let target_path = PathBuf::from(target_path_str.trim().trim_matches('"'));
    if !target_path.exists() {
        warn!(path = %target_path.display(), "目标路径不存在");
        return;
    }

    // 从全局配置查找包含该路径的同步文件夹
    let state = match app.try_state::<AppState>() {
        Some(s) => s,
        None => return,
    };
    let cfg = state.config.lock().await.clone();

    // 采用 Windows 大小写不敏感与斜杠标准化前缀匹配
    let matched = cfg.folders.iter().find(|f| {
        let root = Path::new(&f.local_path);
        path_starts_with_case_insensitive(&target_path, root)
    });

    let folder = match matched {
        Some(f) => f.clone(),
        None => {
            warn!(path = %target_path.display(), "所选文件不在任何已启用的同步文件夹内");
            return;
        }
    };

    let data_dir = folder_data_dir(&folder.id);
    let db = match StateDb::open(&data_dir.join("state.db")) {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "打开状态库失败");
            return;
        }
    };

    let root = PathBuf::from(&folder.local_path);

    // 区分文件还是目录处理
    let mut files_to_process = Vec::new();
    if target_path.is_file() {
        let rel = rel_path(&root, &target_path);
        files_to_process.push(rel);
    } else if target_path.is_dir() {
        for entry in walkdir::WalkDir::new(&target_path)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            if entry.file_type().is_file() {
                let rel = rel_path(&root, entry.path());
                files_to_process.push(rel);
            }
        }
    }

    let mut count = 0;
    for rel in files_to_process {
        match command {
            "--keep-offline" => {
                if let Ok(true) = keep_offline_file(&folder, &db, &rel).await {
                    count += 1;
                }
            }
            "--free-space" => {
                if let Ok(true) = free_space_file(&folder, &db, &rel).await {
                    count += 1;
                }
            }
            _ => {}
        }
    }

    let target_name = target_path.file_name().and_then(|n| n.to_str()).unwrap_or("目标文件");
    let notify_msg = match command {
        "--keep-offline" => format!("已将「{target_name}」设为保持离线可用（恢复 {count} 项完整文件）"),
        "--free-space" => format!("已将「{target_name}」设为不占用电脑空间（已释放 {count} 项本地磁盘占用）"),
        _ => format!("操作已完成（处理 {count} 项）"),
    };

    info!(folder = %folder.name, command, processed = count, "{notify_msg}");
    let _ = app.emit("synccloud://toast", notify_msg);
}
