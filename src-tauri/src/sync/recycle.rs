//! 安全防误删回收站（对标 FreeFileSync 与 Syncthing 版本控制 / 回收站管理）。
//! 将覆盖或删除的文件安全保存在外部 `%APPDATA%/SyncCloud/data/<folder_id>/recycle_bin/`，
//! 不在用户同步文件夹内产生任何垃圾，支持元数据查询、一键恢复与安全清空。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_RECYCLE_ITEMS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecycleItem {
    pub id: String,
    pub file_name: String,
    pub original_path: String,
    pub size: u64,
    /// 操作类型：deleted (本地被删时安全归档) | overwritten (被远端新版本覆盖前归档)
    pub action: String,
    pub archived_at: String,
}

/// 某文件夹的回收站目录路径。
pub fn recycle_dir(folder_id: &Uuid) -> PathBuf {
    crate::config::folder_data_dir(folder_id).join("recycle_bin")
}

fn meta_file(folder_id: &Uuid) -> PathBuf {
    recycle_dir(folder_id).join("recycle_meta.json")
}

/// 加载回收站条目列表。
pub fn list_items(folder_id: &Uuid) -> anyhow::Result<Vec<RecycleItem>> {
    let path = meta_file(folder_id);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let items: Vec<RecycleItem> = serde_json::from_slice(&bytes)?;
    for item in &items {
        crate::sync::scanner::safe_path(&recycle_dir(folder_id), &item.file_name)?;
    }
    Ok(items)
}

async fn index_lock(id: &Uuid) -> tokio::sync::OwnedMutexGuard<()> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<Uuid, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    let lock = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(*id)
        .or_default()
        .clone();
    lock.lock_owned().await
}

fn save_items(folder_id: &Uuid, items: &[RecycleItem]) -> anyhow::Result<()> {
    let r_dir = recycle_dir(folder_id);
    std::fs::create_dir_all(&r_dir)?;
    let path = meta_file(folder_id);
    let tmp = r_dir.join("recycle_meta.json.tmp");
    let data = serde_json::to_vec_pretty(items)?;
    std::fs::write(&tmp, data)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// 安全归档文件到回收站。
pub async fn archive_file(
    folder_id: &Uuid,
    src_path: &Path,
    rel_path: &str,
    action: &str,
) -> anyhow::Result<()> {
    let _index = index_lock(folder_id).await;
    if !src_path.exists() {
        return Ok(());
    }
    let r_dir = recycle_dir(folder_id);
    tokio::fs::create_dir_all(&r_dir).await?;

    let file_stem = src_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let id = Uuid::new_v4().to_string();
    let bak_file_name = format!("{file_stem}.{ts}_{}.{action}", &id[..6]);
    let dst_path = r_dir.join(&bak_file_name);

    let size = tokio::fs::metadata(src_path).await?.len();

    tokio::fs::copy(src_path, &dst_path).await?;
    let source = src_path.to_path_buf();
    let backup = dst_path.clone();
    let stable = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let source_hash = crate::sync::scanner::hash_file(&source)?;
        let backup_hash = crate::sync::scanner::hash_file(&backup)?;
        Ok(std::fs::metadata(&source)?.len() == size
            && std::fs::metadata(&backup)?.len() == size
            && source_hash == backup_hash)
    })
    .await??;
    anyhow::ensure!(
        stable,
        "归档期间文件内容发生变化，停止覆盖或删除，原文件与备份已保留"
    );

    let item = RecycleItem {
        id,
        file_name: bak_file_name,
        original_path: rel_path.to_string(),
        size,
        action: action.to_string(),
        archived_at: chrono::Local::now().to_rfc3339(),
    };

    // 更新索引（最多保留 MAX_RECYCLE_ITEMS 项）
    let mut items = list_items(folder_id)?;
    items.insert(0, item);
    let old = if items.len() > MAX_RECYCLE_ITEMS {
        items.split_off(MAX_RECYCLE_ITEMS)
    } else {
        Vec::new()
    };
    save_items(folder_id, &items)?;
    for item in old {
        tokio::fs::remove_file(r_dir.join(item.file_name)).await?;
    }

    Ok(())
}

/// 将回收站中的指定项目还原到用户的原始相对路径。
pub async fn restore_item(
    folder_id: &Uuid,
    item_id: &str,
    local_root: &Path,
) -> anyhow::Result<String> {
    let pinned = index_lock(folder_id).await;
    let items = list_items(folder_id)?;
    let item = items
        .iter()
        .find(|it| it.id == item_id)
        .ok_or_else(|| anyhow::anyhow!("未找到该回收站归档条目"))?;
    let item = item.clone();

    let r_dir = recycle_dir(folder_id);
    let src_backup = r_dir.join(&item.file_name);
    if !src_backup.exists() {
        anyhow::bail!("回收站物理备份文件已不存在");
    }

    let target_path = crate::sync::scanner::safe_path(local_root, &item.original_path)?;
    // Pin the requested bytes before capacity pruning, clean_all or delete_item can remove them.
    let staged = crate::config::app_data_dir()
        .join("temp")
        .join(format!("restore-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(staged.parent().unwrap()).await?;
    tokio::fs::copy(&src_backup, &staged).await?;
    let source = src_backup.clone();
    let copy = staged.clone();
    let size = item.size;
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        anyhow::ensure!(
            std::fs::metadata(&copy)?.len() == size
                && crate::sync::scanner::hash_file(&source)?
                    == crate::sync::scanner::hash_file(&copy)?,
            "回收站恢复副本校验失败"
        );
        Ok(())
    })
    .await??;
    drop(pinned);
    if let Some(parent) = target_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    if target_path.exists() {
        archive_file(folder_id, &target_path, &item.original_path, "overwritten").await?;
    }
    crate::sync::fslock::replace_file(&staged, &target_path).await?;
    tracing::info!(folder_id = %folder_id, file = %item.original_path, "已成功从回收站还原文件");

    Ok(item.original_path.clone())
}

/// 清空某个文件夹的回收站。
pub async fn clean_all(folder_id: &Uuid) -> anyhow::Result<usize> {
    let _index = index_lock(folder_id).await;
    let r_dir = recycle_dir(folder_id);
    if !r_dir.exists() {
        return Ok(0);
    }
    let mut count = 0;
    let mut dir = tokio::fs::read_dir(&r_dir).await?;
    while let Some(entry) = dir.next_entry().await? {
        let p = entry.path();
        if p.is_file() {
            tokio::fs::remove_file(p).await?;
            count += 1;
        }
    }
    save_items(folder_id, &[])?;
    Ok(count)
}

/// 删除单个回收站条目。
pub async fn delete_item(folder_id: &Uuid, item_id: &str) -> anyhow::Result<()> {
    let _index = index_lock(folder_id).await;
    let mut items = list_items(folder_id)?;
    if let Some(idx) = items.iter().position(|it| it.id == item_id) {
        let removed = items.remove(idx);
        let r_dir = recycle_dir(folder_id);
        tokio::fs::remove_file(r_dir.join(&removed.file_name)).await?;
        save_items(folder_id, &items)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn restore_pins_source_before_concurrent_clean() {
        let root = std::env::temp_dir().join(format!("synccloud-restore-clean-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("local")).unwrap();
        crate::config::set_app_data_dir_in_memory(root.clone());
        let id = Uuid::new_v4();
        let item = RecycleItem {
            id: "item".into(),
            file_name: "backup".into(),
            original_path: "doc.txt".into(),
            size: 4,
            action: "deleted".into(),
            archived_at: String::new(),
        };
        save_items(&id, &[item]).unwrap();
        std::fs::write(recycle_dir(&id).join("backup"), b"safe").unwrap();
        let local = root.join("local");
        let (restored, cleaned) = tokio::join!(restore_item(&id, "item", &local), clean_all(&id));
        restored.unwrap();
        cleaned.unwrap();
        assert_eq!(std::fs::read(local.join("doc.txt")).unwrap(), b"safe");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn concurrent_archives_keep_all_index_entries_and_corruption_blocks_changes() {
        let root = std::env::temp_dir().join(format!("synccloud-recycle-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        crate::config::set_app_data_dir_in_memory(root.clone());
        let id = Uuid::new_v4();
        let source = root.join("source");
        std::fs::write(&source, b"original").unwrap();
        let results = futures::future::join_all(
            (0..12).map(|_| archive_file(&id, &source, "a.txt", "deleted")),
        )
        .await;
        for result in results {
            result.unwrap();
        }
        assert_eq!(list_items(&id).unwrap().len(), 12);
        for item in list_items(&id).unwrap() {
            assert_eq!(
                std::fs::read(recycle_dir(&id).join(item.file_name)).unwrap(),
                b"original"
            );
        }
        std::fs::write(meta_file(&id), b"broken JSON").unwrap();
        assert!(archive_file(&id, &source, "a.txt", "deleted")
            .await
            .is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"original");
        assert_eq!(std::fs::read(meta_file(&id)).unwrap(), b"broken JSON");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn restore_rejects_paths_outside_sync_root() {
        let root = std::env::temp_dir().join(format!("synccloud-restore-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        crate::config::set_app_data_dir_in_memory(root.clone());
        let id = Uuid::new_v4();
        let item = RecycleItem {
            id: "item".into(),
            file_name: "backup".into(),
            original_path: "../escape".into(),
            size: 4,
            action: "deleted".into(),
            archived_at: String::new(),
        };
        save_items(&id, &[item]).unwrap();
        std::fs::write(recycle_dir(&id).join("backup"), b"safe").unwrap();
        let local = root.join("local");
        std::fs::create_dir_all(&local).unwrap();
        assert!(restore_item(&id, "item", &local).await.is_err());
        assert!(!root.join("escape").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
