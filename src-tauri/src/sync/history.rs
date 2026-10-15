//! 传输历史与事件持久化数据库（基于应用数据目录的 history.db）
//! 支持自定义保留周期（3天/7天/14天/30天/永久），自动清理过期数据。

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use crate::sync::engine::SyncEvent;

static DB_CONN: OnceLock<Mutex<Connection>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityItem {
    pub id: i64,
    pub folder_id: String,
    pub folder_name: String,
    pub folder_local_path: String,
    pub event_type: String,
    pub path: Option<String>,
    pub action: Option<String>,
    pub error: Option<String>,
    pub reason: Option<String>,
    pub copy: Option<String>,
    pub timestamp: i64,
}

fn db_path() -> PathBuf {
    crate::config::app_data_dir().join("history.db")
}

fn get_conn() -> anyhow::Result<&'static Mutex<Connection>> {
    if let Some(conn) = DB_CONN.get() {
        return Ok(conn);
    }
    let p = db_path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(&p)?;
    conn.execute_batch("
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        CREATE TABLE IF NOT EXISTS activity_history (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            folder_id         TEXT NOT NULL,
            folder_name       TEXT NOT NULL,
            folder_local_path TEXT NOT NULL DEFAULT '',
            event_type        TEXT NOT NULL,
            path              TEXT,
            action            TEXT,
            error             TEXT,
            reason            TEXT,
            copy              TEXT,
            timestamp         INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_activity_time ON activity_history(timestamp);
        CREATE INDEX IF NOT EXISTS idx_activity_folder ON activity_history(folder_id);
    ")?;
    let _ = DB_CONN.set(Mutex::new(conn));
    Ok(DB_CONN.get().unwrap())
}

/// 将同步事件转化为传输历史记录并持久化
pub fn record_event(
    folder_id: &str,
    folder_name: &str,
    folder_local_path: &str,
    ev: &SyncEvent,
) {
    let now = chrono::Utc::now().timestamp_millis();
    let conn_mutex = match get_conn() {
        Ok(c) => c,
        Err(_) => return,
    };

    match ev {
        SyncEvent::FileDone { path, action, .. } => {
            // 节约存储空间：内容一致无需传输的 identical 状态不占用历史记录
            if action == "identical" {
                return;
            }
            let _ = conn_mutex.lock().unwrap().execute(
                "INSERT INTO activity_history (folder_id, folder_name, folder_local_path, event_type, path, action, timestamp)
                 VALUES (?1, ?2, ?3, 'file_done', ?4, ?5, ?6)",
                params![folder_id, folder_name, folder_local_path, path, action, now],
            );
        }
        SyncEvent::FileFailed { path, error, retry_in_secs, .. } => {
            // 重试中间态（retry_in_secs > 0）不入库：避免同一文件一次同步产生多条失败记录；
            // 仅最终失败（retry_in_secs == 0）才写入传输历史
            if *retry_in_secs > 0 {
                return;
            }
            let _ = conn_mutex.lock().unwrap().execute(
                "INSERT INTO activity_history (folder_id, folder_name, folder_local_path, event_type, path, error, timestamp)
                 VALUES (?1, ?2, ?3, 'file_failed', ?4, ?5, ?6)",
                params![folder_id, folder_name, folder_local_path, path, error, now],
            );
        }
        SyncEvent::FilePending { path, reason, .. } => {
            let _ = conn_mutex.lock().unwrap().execute(
                "INSERT INTO activity_history (folder_id, folder_name, folder_local_path, event_type, path, reason, timestamp)
                 VALUES (?1, ?2, ?3, 'file_pending', ?4, ?5, ?6)",
                params![folder_id, folder_name, folder_local_path, path, reason, now],
            );
        }
        SyncEvent::ConflictCreated { path, copy, .. } => {
            let _ = conn_mutex.lock().unwrap().execute(
                "INSERT INTO activity_history (folder_id, folder_name, folder_local_path, event_type, path, copy, timestamp)
                 VALUES (?1, ?2, ?3, 'conflict_created', ?4, ?5, ?6)",
                params![folder_id, folder_name, folder_local_path, path, copy, now],
            );
        }
        SyncEvent::SafetyBlocked { reason, deleted_count, total_count, .. } => {
            let desc = format!("拟删除 {deleted_count} 项 / 占比 {}%", (deleted_count * 100) / total_count.max(&1));
            let _ = conn_mutex.lock().unwrap().execute(
                "INSERT INTO activity_history (folder_id, folder_name, folder_local_path, event_type, path, reason, timestamp)
                 VALUES (?1, ?2, ?3, 'safety_blocked', ?4, ?5, ?6)",
                params![folder_id, folder_name, folder_local_path, desc, reason, now],
            );
        }
        _ => {}
    }
}

/// 分页结果：当前页条目 + 筛选条件下总条数
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityPage {
    pub items: Vec<ActivityItem>,
    pub total: u64,
}

/// 查询条件（WHERE 子句与参数）——分页查询与总数统计共用，保证口径一致。
struct HistoryFilter {
    clause: String,
    params: Vec<Box<dyn rusqlite::ToSql>>,
}

fn build_history_filter(
    folder_id: Option<&str>,
    action: Option<&str>,
    keyword: Option<&str>,
) -> HistoryFilter {
    let mut clause = " WHERE 1=1".to_string();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(fid) = folder_id {
        if !fid.is_empty() && fid != "all" {
            clause.push_str(" AND folder_id = ?");
            params.push(Box::new(fid.to_string()));
        }
    }
    if let Some(act) = action {
        if !act.is_empty() && act != "all" {
            clause.push_str(" AND (action = ? OR event_type = ?)");
            params.push(Box::new(act.to_string()));
            params.push(Box::new(act.to_string()));
        }
    }
    if let Some(kw) = keyword {
        let trimmed = kw.trim();
        if !trimmed.is_empty() {
            clause.push_str(" AND (path LIKE ? OR folder_name LIKE ? OR error LIKE ? OR reason LIKE ?)");
            let like = format!("%{trimmed}%");
            params.push(Box::new(like.clone()));
            params.push(Box::new(like.clone()));
            params.push(Box::new(like.clone()));
            params.push(Box::new(like));
        }
    }
    HistoryFilter { clause, params }
}

/// 查询传输历史（分页：按 id 倒序 + offset 偏移）
pub fn query_history(
    folder_id: Option<&str>,
    action: Option<&str>,
    keyword: Option<&str>,
    limit: usize,
    offset: usize,
) -> Vec<ActivityItem> {
    let conn_mutex = match get_conn() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let conn = conn_mutex.lock().unwrap();

    let filter = build_history_filter(folder_id, action, keyword);
    let sql = format!(
        "SELECT id, folder_id, folder_name, folder_local_path, event_type, path, action, error, reason, copy, timestamp \
         FROM activity_history{} ORDER BY id DESC LIMIT ? OFFSET ?",
        filter.clause
    );
    let mut params = filter.params;
    params.push(Box::new(limit.clamp(1, 2000) as i64));
    params.push(Box::new(offset as i64));
    let rusqlite_params: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let rows = stmt.query_map(&rusqlite_params[..], |row| {
        Ok(ActivityItem {
            id: row.get(0)?,
            folder_id: row.get(1)?,
            folder_name: row.get(2)?,
            folder_local_path: row.get(3)?,
            event_type: row.get(4)?,
            path: row.get(5)?,
            action: row.get(6)?,
            error: row.get(7)?,
            reason: row.get(8)?,
            copy: row.get(9)?,
            timestamp: row.get(10)?,
        })
    });

    match rows {
        Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// 统计筛选条件下的记录总数（用于分页）
pub fn count_history(
    folder_id: Option<&str>,
    action: Option<&str>,
    keyword: Option<&str>,
) -> u64 {
    let conn_mutex = match get_conn() {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let conn = conn_mutex.lock().unwrap();
    let filter = build_history_filter(folder_id, action, keyword);
    let sql = format!("SELECT COUNT(*) FROM activity_history{}", filter.clause);
    let rusqlite_params: Vec<&dyn rusqlite::ToSql> =
        filter.params.iter().map(|b| b.as_ref()).collect();
    conn.query_row(&sql, &rusqlite_params[..], |row| row.get::<_, i64>(0))
        .map(|n| n.max(0) as u64)
        .unwrap_or(0)
}

/// 自动清理超过指定保留周期的历史数据并释放磁盘物理空间。
/// retention_days: 0 表示永久保留；>0 表示保留天数。
pub fn clean_expired_history(retention_days: u32) -> usize {
    if retention_days == 0 {
        return 0; // 永久保留
    }
    let conn_mutex = match get_conn() {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let cutoff = chrono::Utc::now().timestamp_millis() - (retention_days as i64 * 86400 * 1000);
    let conn = conn_mutex.lock().unwrap();
    let n = conn.execute(
        "DELETE FROM activity_history WHERE timestamp < ?1",
        params![cutoff],
    )
    .unwrap_or(0);
    if n > 0 {
        // 清理后主动释放空间，避免 history.db 膨胀
        let _ = conn.execute("VACUUM", []);
    }
    n
}

/// 用户手动清空所有或指定文件夹的传输历史并物理收缩数据库
pub fn clear_all_history(folder_id: Option<&str>) -> usize {
    let conn_mutex = match get_conn() {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let conn = conn_mutex.lock().unwrap();
    let n = if let Some(fid) = folder_id {
        if !fid.is_empty() && fid != "all" {
            conn.execute("DELETE FROM activity_history WHERE folder_id = ?1", params![fid])
                .unwrap_or(0)
        } else {
            conn.execute("DELETE FROM activity_history", []).unwrap_or(0)
        }
    } else {
        conn.execute("DELETE FROM activity_history", []).unwrap_or(0)
    };
    if n > 0 {
        let _ = conn.execute("VACUUM", []);
    }
    n
}
