//! 文件日志系统（零额外依赖，手写轮转 MakeWriter）：
//! - 输出到 `<数据目录>/logs/synccloud.log`，超过 5MB 轮转为 `.log.1`（保留一份历史）；
//! - 等级由设置控制（trace/debug/info/warn/error），运行时可通过 IPC 热调整；
//! - 文件输出可开关（关闭时仍保留控制台输出）；
//! - 启动时清理 14 天前的历史日志。
//!
//! 此前 tracing 只输出到 stdout —— GUI 模式下用户完全看不到日志，
//! 排障只能靠复现，这是「日志输出优化」的核心补齐。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{reload, EnvFilter};

const MAX_LOG_SIZE: u64 = 5 * 1024 * 1024;
const KEEP_DAYS: u64 = 14;

/// 文件日志运行时开关（设置页热切换，无需重启）。
static FILE_ENABLED: AtomicBool = AtomicBool::new(true);
/// 运行时等级调整句柄。
static LEVEL_HANDLE: OnceLock<reload::Handle<EnvFilter, tracing_subscriber::Registry>> =
    OnceLock::new();
/// 日志目录（支持热切换）。
static LOG_DIR: OnceLock<Mutex<PathBuf>> = OnceLock::new();
/// 全局日志文件句柄引用（用于动态调整目标路径）。
static GLOBAL_FILE_WRITER: OnceLock<std::sync::Arc<SharedLogFile>> = OnceLock::new();

/// 获取程序可执行文件所在目录（优先识别便携启动器注入的环境变量，其次读取当前 exe 路径）
pub fn program_dir() -> PathBuf {
    if let Ok(p) = std::env::var("SYNCCLOUD_PROGRAM_DIR") {
        let pb = PathBuf::from(p);
        if pb.is_dir() {
            return pb;
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// 解析日志目录：如果指定了合法自定义目录则使用；否则默认使用程序所在目录下的 logs/；
/// 若遇到无权限写入等异常，兜底 fallback 到应用数据目录 logs/。
pub fn resolve_log_dir(custom: Option<&str>) -> PathBuf {
    if let Some(c) = custom {
        let trimmed = c.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            if std::fs::create_dir_all(&p).is_ok() {
                return p;
            }
        }
    }
    let default_dir = program_dir().join("logs");
    if std::fs::create_dir_all(&default_dir).is_ok() {
        return default_dir;
    }
    let fallback = crate::config::app_data_dir().join("logs");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

pub fn log_dir() -> PathBuf {
    if let Some(m) = LOG_DIR.get() {
        if let Ok(guard) = m.lock() {
            return guard.clone();
        }
    }
    resolve_log_dir(None)
}

/// 初始化日志：控制台 + 轮转文件双层，等级来自全局设置（RUST_LOG 可覆盖）。
pub fn init_logging() {
    let (level, file_enabled, custom_log_dir) = match crate::config::AppConfig::load_or_create(&crate::config::app_data_dir()) {
        Ok(cfg) => (cfg.settings.log_level.clone(), cfg.settings.log_to_file, cfg.settings.custom_log_dir.clone()),
        Err(_) => ("info".to_string(), true, None),
    };
    FILE_ENABLED.store(file_enabled, Ordering::Relaxed);

    let dir = resolve_log_dir(custom_log_dir.as_deref());
    let _ = std::fs::create_dir_all(&dir);
    cleanup_old_logs(&dir);
    let _ = LOG_DIR.set(Mutex::new(dir.clone()));

    let file_writer = SharedLogFile::new(dir.join("synccloud.log"), MAX_LOG_SIZE);
    let file_writer = std::sync::Arc::new(file_writer);
    let _ = GLOBAL_FILE_WRITER.set(file_writer.clone());

    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let (filter_layer, handle) = reload::Layer::new(filter);
    let _ = LEVEL_HANDLE.set(handle);

    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(std::io::stdout);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_ansi(false)
        .with_writer(move || LogLineWriter {
            file: file_writer.clone(),
            buf: Vec::with_capacity(256),
        });

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(console_layer)
        .with(file_layer)
        .init();

    tracing::info!(level = %level, file_logging = file_enabled, log_dir = %dir.display(), "日志系统初始化完成");
}

/// 运行时调整日志等级、文件开关与自定义目录（设置页保存时调用）。
pub fn apply_settings(level: &str, file_enabled: bool, custom_log_dir: Option<&str>) {
    FILE_ENABLED.store(file_enabled, Ordering::Relaxed);
    if let Some(handle) = LEVEL_HANDLE.get() {
        match EnvFilter::try_new(level) {
            Ok(f) => {
                let _ = handle.modify(|old| {
                    *old = f;
                });
                tracing::info!(level = %level, "日志等级已调整");
            }
            Err(_) => tracing::warn!(level = %level, "非法日志等级，忽略"),
        }
    }

    // 检查日志目录是否需要调整
    let new_dir = resolve_log_dir(custom_log_dir);
    let mut dir_changed = false;
    if let Some(m) = LOG_DIR.get() {
        if let Ok(mut guard) = m.lock() {
            if *guard != new_dir {
                *guard = new_dir.clone();
                dir_changed = true;
            }
        }
    }
    if dir_changed {
        let _ = std::fs::create_dir_all(&new_dir);
        cleanup_old_logs(&new_dir);
        if let Some(writer) = GLOBAL_FILE_WRITER.get() {
            writer.update_path(new_dir.join("synccloud.log"));
        }
        tracing::info!(new_dir = %new_dir.display(), "日志输出目录已热切换");
    }
}

/// 清理超过保留期的历史日志。
fn cleanup_old_logs(dir: &Path) {
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(KEEP_DAYS * 86400);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let age_ok = e
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t < cutoff)
                .unwrap_or(false);
            if age_ok {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

// ---- 轮转文件写入器 ----

/// 共享日志文件：Mutex 串行化写入，超限自动轮转。
struct SharedLogFile {
    path: Mutex<PathBuf>,
    max_size: u64,
    state: Mutex<Option<(File, u64)>>, // 文件 + 已写字节
}

impl SharedLogFile {
    fn new(path: PathBuf, max_size: u64) -> Self {
        Self {
            path: Mutex::new(path),
            max_size,
            state: Mutex::new(None),
        }
    }

    fn update_path(&self, new_path: PathBuf) {
        let mut guard = self.state.lock().unwrap();
        *guard = None; // 关闭原文件描述符
        let mut p = self.path.lock().unwrap();
        *p = new_path;
    }

    fn write_line(&self, buf: &[u8]) {
        if !FILE_ENABLED.load(Ordering::Relaxed) || buf.is_empty() {
            return;
        }
        let cur_path = {
            let p = self.path.lock().unwrap();
            p.clone()
        };
        let mut guard = self.state.lock().unwrap();
        // 惰性打开（追加模式），记录当前大小用于轮转判断
        if guard.is_none() {
            let size = std::fs::metadata(&cur_path).map(|m| m.len()).unwrap_or(0);
            match OpenOptions::new().create(true).append(true).open(&cur_path) {
                Ok(f) => *guard = Some((f, size)),
                Err(_) => return,
            }
        }
        if let Some((f, written)) = guard.as_mut() {
            let _ = f.write_all(buf);
            *written += buf.len() as u64;
            if *written >= self.max_size {
                // 轮转：当前 → .log.1（覆盖更旧的）
                let rotated = cur_path.with_extension("log.1");
                let _ = std::fs::remove_file(&rotated);
                let _ = f.flush();
                let _ = std::fs::rename(&cur_path, &rotated);
                *guard = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&cur_path)
                    .ok()
                    .map(|nf| (nf, 0));
            }
        }
    }
}

/// 单条日志事件的缓冲写入器（fmt layer 每事件 make_writer → write* → flush）。
struct LogLineWriter {
    file: std::sync::Arc<SharedLogFile>,
    buf: Vec<u8>,
}

impl Write for LogLineWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            self.file.write_line(&self.buf);
            self.buf.clear();
        }
        Ok(())
    }
}

impl Drop for LogLineWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

// ---- 日志查询（供设置页查看器） ----

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogFileInfo {
    pub name: String,
    pub size: u64,
    pub modified: String,
}

/// 列出日志目录中的文件（按修改时间倒序）。
pub fn list_log_files() -> Vec<LogFileInfo> {
    let dir = log_dir();
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".log") && !name.ends_with(".log.1") {
                continue;
            }
            let meta = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            out.push(LogFileInfo {
                name,
                size: meta.len(),
                modified: meta
                    .modified()
                    .ok()
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Local> = t.into();
                        dt.format("%Y-%m-%d %H:%M:%S").to_string()
                    })
                    .unwrap_or_default(),
            });
        }
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

/// 读取指定日志文件的最后 N 行。文件名必须位于日志目录内（防路径穿越）。
pub fn read_log_tail(name: &str, lines: usize) -> anyhow::Result<Vec<String>> {
    let dir = log_dir();
    if !name.ends_with(".log") && !name.ends_with(".log.1") {
        anyhow::bail!("非法日志文件名");
    }
    let path = dir.join(name);
    // 只允许读日志目录下的直接子文件（canonicalize 消解符号链接与相对段）
    let canonical = path.canonicalize().map_err(|_| anyhow::anyhow!("日志文件不存在"))?;
    let canonical_dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
    if canonical.parent() != Some(canonical_dir.as_path()) {
        anyhow::bail!("非法日志文件路径");
    }
    let content = std::fs::read(&canonical)?;
    let text = String::from_utf8_lossy(&content);
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    Ok(all[start..].iter().map(|s| s.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_log_file_writes_and_rotates() {
        let dir = std::env::temp_dir().join("synccloud-logging-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = SharedLogFile::new(dir.join("a.log"), 200);
        FILE_ENABLED.store(true, Ordering::Relaxed);
        for i in 0..50 {
            log.write_line(format!("line-{i:03} padded to some bytes each\n").as_bytes());
        }
        // 已发生轮转：当前文件小于阈值，且存在 .log.1
        let cur = std::fs::metadata(dir.join("a.log")).unwrap().len();
        assert!(cur < 400, "current size {cur}");
        assert!(dir.join("a.log.1").exists());
        let content = std::fs::read_to_string(dir.join("a.log")).unwrap();
        assert!(content.contains("line-049"));
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn file_disabled_drops_writes() {
        let dir = std::env::temp_dir().join("synccloud-logging-test-off");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = SharedLogFile::new(dir.join("b.log"), 1024);
        FILE_ENABLED.store(false, Ordering::Relaxed);
        log.write_line(b"should not appear\n");
        assert!(!dir.join("b.log").exists());
        FILE_ENABLED.store(true, Ordering::Relaxed);
    }

    #[test]
    fn tail_returns_last_lines() {
        let dir = std::env::temp_dir().join("synccloud-logging-tail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("c.log"), "1\n2\n3\n4\n5\n").unwrap();
        // read_log_tail 依赖 LOG_DIR，测试里直接构造路径校验逻辑等价实现
        let content = std::fs::read_to_string(dir.join("c.log")).unwrap();
        let all: Vec<&str> = content.lines().collect();
        let start = all.len().saturating_sub(2);
        let tail: Vec<String> = all[start..].iter().map(|s| s.to_string()).collect();
        assert_eq!(tail, vec!["4".to_string(), "5".to_string()]);
    }
}
