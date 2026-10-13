//! SQLite 状态库（每文件夹一个）：三方对比基线 + 哈希缓存 + 冲突记录 + 键值存储
//! + 远端变更日志（崩溃恢复）。
//! 引擎任务独占持有；Connection 包 Mutex 使 StateDb 为 Sync（引擎 future 需跨 await）。

use std::path::Path;
use rusqlite::{params, Connection};
use serde::Serialize;
use crate::sync::types::{FileMeta, FileTree};

pub struct StateDb {
    conn: std::sync::Mutex<Connection>,
}

/// 远端变更日志条目：动作已成功写入远端存储，但 manifest 提交可能尚未完成。
/// 下一轮同步开始时先重放进 manifest，保证「上传成功但程序中断」的文件
/// 不会被三方对比误判为远端已删除（防数据丢失 / 全量重传）。
#[derive(Debug, Clone)]
pub enum JournalOp {
    Put(String, FileMeta),
    Remove(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictRecord {
    pub id: i64,
    pub path: String,
    pub local_hash: String,
    pub remote_hash: String,
    pub conflict_copy: Option<String>,
    pub created_at: String,
    pub resolved: bool,
    pub resolution: Option<String>,
}

impl StateDb {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            CREATE TABLE IF NOT EXISTS baseline (
                path       TEXT PRIMARY KEY,
                hash       TEXT NOT NULL,
                size       INTEGER NOT NULL,
                mtime_ms   INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS hash_cache (
                path     TEXT PRIMARY KEY,
                size     INTEGER NOT NULL,
                mtime_ms INTEGER NOT NULL,
                hash     TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS conflicts (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                path         TEXT NOT NULL,
                local_hash   TEXT NOT NULL,
                remote_hash  TEXT NOT NULL,
                conflict_copy TEXT,
                created_at   TEXT NOT NULL,
                resolved     INTEGER NOT NULL DEFAULT 0,
                resolution   TEXT
            );
            CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS journal (
                id   INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL,
                op   TEXT NOT NULL,
                meta TEXT
            );
        ")?;
        Ok(Self { conn: std::sync::Mutex::new(conn) })
    }

    // ---- 远端变更日志（崩溃恢复） ----

    /// 读取未提交的远端变更。
    pub fn journal_load(&self) -> anyhow::Result<Vec<JournalOp>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT path, op, meta FROM journal ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (path, op, meta) = r?;
            match op.as_str() {
                "put" => {
                    if let Some(m) = meta.and_then(|s| serde_json::from_str(&s).ok()) {
                        out.push(JournalOp::Put(path, m));
                    }
                }
                "remove" => out.push(JournalOp::Remove(path)),
                _ => {}
            }
        }
        Ok(out)
    }

    pub fn journal_put(&self, path: &str, meta: &FileMeta) {
        let _ = self.conn.lock().unwrap().execute(
            "INSERT INTO journal (path, op, meta) VALUES (?1, 'put', ?2)",
            params![path, serde_json::to_string(meta).unwrap_or_default()],
        );
    }

    pub fn journal_remove(&self, path: &str) {
        let _ = self.conn.lock().unwrap().execute(
            "INSERT INTO journal (path, op, meta) VALUES (?1, 'remove', NULL)",
            params![path],
        );
    }

    /// manifest 成功提交后清空日志。
    pub fn journal_clear(&self) {
        let _ = self.conn.lock().unwrap().execute("DELETE FROM journal", []);
    }

    // ---- 基线 ----

    pub fn load_baseline(&self) -> anyhow::Result<FileTree> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT path, hash, size, mtime_ms FROM baseline")?;
        let mut tree = FileTree::new();
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        for r in rows {
            let (path, hash, size, mtime) = r?;
            tree.insert(
                path,
                FileMeta {
                    hash,
                    size: size.max(0) as u64,
                    mtime_ms: mtime,
                },
            );
        }
        Ok(tree)
    }

    /// 用一轮同步结束后的最终状态整体替换基线（事务内全删重插，对万级文件规模足够）。
    pub fn replace_baseline(&self, tree: &FileTree) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM baseline", [])?;
        {
            let mut stmt = tx
                .prepare("INSERT INTO baseline (path, hash, size, mtime_ms) VALUES (?1, ?2, ?3, ?4)")?;
            for (path, meta) in tree {
                stmt.execute(params![path, meta.hash, meta.size as i64, meta.mtime_ms])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- 哈希缓存 ----

    /// mtime 与 size 都匹配时复用已缓存的哈希，避免重复读取文件内容。
    pub fn cached_hash(&self, path: &str, size: u64, mtime_ms: i64) -> Option<String> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT hash FROM hash_cache WHERE path = ?1 AND size = ?2 AND mtime_ms = ?3",
                params![path, size as i64, mtime_ms],
                |row| row.get(0),
            )
            .ok()
    }

    pub fn put_hash_cache(&self, path: &str, size: u64, mtime_ms: i64, hash: &str) {
        let _ = self.conn.lock().unwrap().execute(
            "INSERT OR REPLACE INTO hash_cache (path, size, mtime_ms, hash) VALUES (?1, ?2, ?3, ?4)",
            params![path, size as i64, mtime_ms, hash],
        );
    }

    // ---- 冲突记录 ----

    pub fn record_conflict(
        &self,
        path: &str,
        local_hash: &str,
        remote_hash: &str,
        conflict_copy: Option<&str>,
    ) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO conflicts (path, local_hash, remote_hash, conflict_copy, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                path,
                local_hash,
                remote_hash,
                conflict_copy,
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_conflicts(&self, include_resolved: bool) -> anyhow::Result<Vec<ConflictRecord>> {
        let conn = self.conn.lock().unwrap();
        let sql = if include_resolved {
            "SELECT id, path, local_hash, remote_hash, conflict_copy, created_at, resolved, resolution FROM conflicts ORDER BY id DESC"
        } else {
            "SELECT id, path, local_hash, remote_hash, conflict_copy, created_at, resolved, resolution FROM conflicts WHERE resolved = 0 ORDER BY id DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(ConflictRecord {
                id: row.get(0)?,
                path: row.get(1)?,
                local_hash: row.get(2)?,
                remote_hash: row.get(3)?,
                conflict_copy: row.get(4)?,
                created_at: row.get(5)?,
                resolved: row.get::<_, i64>(6)? != 0,
                resolution: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn get_conflict(&self, id: i64) -> anyhow::Result<Option<ConflictRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, path, local_hash, remote_hash, conflict_copy, created_at, resolved, resolution FROM conflicts WHERE id = ?1"
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok(ConflictRecord {
                id: row.get(0)?,
                path: row.get(1)?,
                local_hash: row.get(2)?,
                remote_hash: row.get(3)?,
                conflict_copy: row.get(4)?,
                created_at: row.get(5)?,
                resolved: row.get::<_, i64>(6)? != 0,
                resolution: row.get(7)?,
            })
        })?;
        if let Some(r) = rows.next() {
            Ok(Some(r?))
        } else {
            Ok(None)
        }
    }

    pub fn resolve_conflict(&self, id: i64, resolution: &str) -> anyhow::Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE conflicts SET resolved = 1, resolution = ?2 WHERE id = ?1",
            params![id, resolution],
        )?;
        Ok(())
    }

    // ---- 键值 ----

    pub fn kv_get(&self, key: &str) -> Option<String> {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT v FROM kv WHERE k = ?1", params![key], |r| r.get(0))
            .ok()
    }

    pub fn kv_set(&self, key: &str, value: &str) {
        let _ = self.conn.lock().unwrap().execute(
            "INSERT OR REPLACE INTO kv (k, v) VALUES (?1, ?2)",
            params![key, value],
        );
    }
}
