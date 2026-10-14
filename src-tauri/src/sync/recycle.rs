//! 安全防误删回收站（对标 FreeFileSync 与 Syncthing 版本控制 / 回收站管理）。
//! 将覆盖或删除的文件安全保存在外部 `%APPDATA%/SyncCloud/data/<folder_id>/recycle_bin/`，
//! 不在用户同步文件夹内产生任何垃圾，支持元数据查询、一键恢复与安全清空。

use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
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
pub fn list_items(folder_id: &Uuid) -> Vec<RecycleItem> {
    let path = meta_file(folder_id);
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(items) = serde_json::from_slice::<Vec<RecycleItem>>(&bytes) {
            let r_dir = recycle_dir(folder_id);
            // 过滤出磁盘上物理文件实际存在的条目
            return items
                .into_iter()
                .filter(|it| r_dir.join(&it.file_name).exists())
                .collect();
        }
    }
    Vec::new()
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

    let size = tokio::fs::metadata(src_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    tokio::fs::copy(src_path, &dst_path).await?;

    let item = RecycleItem {
        id,
        file_name: bak_file_name,
        original_path: rel_path.to_string(),
        size,
        action: action.to_string(),
        archived_at: chrono::Local::now().to_rfc3339(),
    };

    // 更新索引（最多保留 MAX_RECYCLE_ITEMS 项）
    let mut items = list_items(folder_id);
    items.insert(0, item);
    if items.len() > MAX_RECYCLE_ITEMS {
        for old in items.split_off(MAX_RECYCLE_ITEMS) {
            let _ = tokio::fs::remove_file(r_dir.join(&old.file_name)).await;
        }
    }
    let _ = save_items(folder_id, &items);

    Ok(())
}

/// 将回收站中的指定项目还原到用户的原始相对路径。
pub async fn restore_item(
    folder_id: &Uuid,
    item_id: &str,
    local_root: &Path,
) -> anyhow::Result<String> {
    let items = list_items(folder_id);
    let item = items
        .iter()
        .find(|it| it.id == item_id)
        .ok_or_else(|| anyhow::anyhow!("未找到该回收站归档条目"))?;

    let r_dir = recycle_dir(folder_id);
    let src_backup = r_dir.join(&item.file_name);
    if !src_backup.exists() {
        anyhow::bail!("回收站物理备份文件已不存在");
    }

    let target_path = local_root.join(&item.original_path);
    if let Some(parent) = target_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::fs::copy(&src_backup, &target_path).await?;
    tracing::info!(folder_id = %folder_id, file = %item.original_path, "已成功从回收站还原文件");

    Ok(item.original_path.clone())
}

/// 清空某个文件夹的回收站。
pub async fn clean_all(folder_id: &Uuid) -> anyhow::Result<usize> {
    let r_dir = recycle_dir(folder_id);
    if !r_dir.exists() {
        return Ok(0);
    }
    let mut count = 0;
    let mut dir = tokio::fs::read_dir(&r_dir).await?;
    while let Ok(Some(entry)) = dir.next_entry().await {
        let p = entry.path();
        if p.is_file() {
            let _ = tokio::fs::remove_file(p).await;
            count += 1;
        }
    }
    let _ = save_items(folder_id, &[]);
    Ok(count)
}

/// 删除单个回收站条目。
pub async fn delete_item(folder_id: &Uuid, item_id: &str) -> anyhow::Result<()> {
    let mut items = list_items(folder_id);
    if let Some(idx) = items.iter().position(|it| it.id == item_id) {
        let removed = items.remove(idx);
        let r_dir = recycle_dir(folder_id);
        let _ = tokio::fs::remove_file(r_dir.join(&removed.file_name)).await;
        save_items(folder_id, &items)?;
    }
    Ok(())
}
