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
            let _ = conn_mutex.lock().unwrap().execute(
                "INSERT INTO activity_history (folder_id, folder_name, folder_local_path, event_type, path, action, timestamp)
                 VALUES (?1, ?2, ?3, 'file_done', ?4, ?5, ?6)",
                params![folder_id, folder_name, folder_local_path, path, action, now],
            );
        }
        SyncEvent::FileFailed { path, error, .. } => {
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

/// 查询传输历史
pub fn query_history(
    folder_id: Option<&str>,
    action: Option<&str>,
    keyword: Option<&str>,
    limit: usize,
) -> Vec<ActivityItem> {
    let conn_mutex = match get_conn() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let conn = conn_mutex.lock().unwrap();

    let mut sql = "SELECT id, folder_id, folder_name, folder_local_path, event_type, path, action, error, reason, copy, timestamp
                   FROM activity_history WHERE 1=1".to_string();
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(fid) = folder_id {
        if !fid.is_empty() && fid != "all" {
            sql.push_str(" AND folder_id = ?");
            params_vec.push(Box::new(fid.to_string()));
        }
    }
    if let Some(act) = action {
        if !act.is_empty() && act != "all" {
            sql.push_str(" AND (action = ? OR event_type = ?)");
            params_vec.push(Box::new(act.to_string()));
            params_vec.push(Box::new(act.to_string()));
        }
    }
    if let Some(kw) = keyword {
        let trimmed = kw.trim();
        if !trimmed.is_empty() {
            sql.push_str(" AND (path LIKE ? OR folder_name LIKE ? OR error LIKE ? OR reason LIKE ?)");
            let like = format!("%{trimmed}%");
            params_vec.push(Box::new(like.clone()));
            params_vec.push(Box::new(like.clone()));
            params_vec.push(Box::new(like.clone()));
            params_vec.push(Box::new(like));
        }
    }

    sql.push_str(" ORDER BY id DESC LIMIT ?");
    params_vec.push(Box::new(limit.clamp(1, 2000) as i64));

    let rusqlite_params: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

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

/// 自动清理超过指定保留周期的历史数据。
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
    conn.execute(
        "DELETE FROM activity_history WHERE timestamp < ?1",
        params![cutoff],
    )
    .unwrap_or(0)
}

/// 用户手动清空所有或指定文件夹的传输历史
pub fn clear_all_history(folder_id: Option<&str>) -> usize {
    let conn_mutex = match get_conn() {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let conn = conn_mutex.lock().unwrap();
    if let Some(fid) = folder_id {
        if !fid.is_empty() && fid != "all" {
            return conn
                .execute("DELETE FROM activity_history WHERE folder_id = ?1", params![fid])
                .unwrap_or(0);
        }
    }
    conn.execute("DELETE FROM activity_history", []).unwrap_or(0)
}
