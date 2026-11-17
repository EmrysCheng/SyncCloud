//! 应用配置：全局设置 + 同步文件夹列表，持久化到 `%APPDATA%/SyncCloud/config.json`。
//! 敏感字段（密码/密钥）不落盘，存入系统凭据管理器（见 credential.rs）。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::sync::types::{ConflictPolicy, Direction, SyncMode, TransferPriority};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub device_name: String,
    pub device_id: String,
    #[serde(default)]
    pub folders: Vec<FolderConfig>,
    #[serde(default)]
    pub settings: GlobalSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalSettings {
    /// 并发传输数（每文件夹）
    pub concurrency: u32,
    /// 失败重试次数
    pub max_retries: u32,
    /// 兜底全量扫描间隔（分钟）
    pub full_scan_interval_min: u64,
    /// 大文件确认阈值（字节），0 = 不限制
    pub large_file_threshold: u64,
    pub language: String,
    /// 关闭主窗口时最小化到系统托盘而不是退出（退出请用托盘菜单）
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    /// 云盘开放平台自建应用凭据（设置页授权与添加向导共用）
    #[serde(default)]
    pub baidu_app_key: String,
    #[serde(default)]
    pub baidu_secret_key: String,
    #[serde(default)]
    pub aliyun_app_id: String,
    #[serde(default)]
    pub aliyun_app_secret: String,
    /// 日志等级：trace / debug / info / warn / error
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// 是否输出文件日志（数据目录 logs/，轮转保留）
    #[serde(default = "default_true")]
    pub log_to_file: bool,
    /// 大规模误删安全熔断保护（对标 GoodSync / Syncthing，单次删除过多时阻断）
    #[serde(default = "default_true")]
    pub deletion_guard: bool,
    /// 误删保护触发数量阈值（默认 50 项）
    #[serde(default = "default_deletion_guard_threshold")]
    pub deletion_guard_threshold: u32,
    /// 自定义日志文件输出目录（None 或空则默认存放于程序所在目录下的 logs/）
    #[serde(default)]
    pub custom_log_dir: Option<String>,
    /// 传输历史保留周期（天数，默认 7 天，0 表示永久保留）
    #[serde(default = "default_activity_retention_days")]
    pub activity_retention_days: u32,
    /// 上传限速（KB/s，0 为不限速，对标 GoodSync / Syncthing / Resilio Sync）
    #[serde(default)]
    pub max_upload_kbps: u32,
    /// 下载限速（KB/s，0 为不限速）
    #[serde(default)]
    pub max_download_kbps: u32,
}

fn default_activity_retention_days() -> u32 {
    7
}

fn default_deletion_guard_threshold() -> u32 {
    50
}

fn default_log_level() -> String {
    "info".into()
}

impl GlobalSettings {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!((1..=32).contains(&self.concurrency), "并发数应为 1 到 32");
        anyhow::ensure!(self.max_retries <= 20, "重试次数不能超过 20");
        anyhow::ensure!(
            (1..=10080).contains(&self.full_scan_interval_min),
            "扫描间隔应为 1 到 10080 分钟"
        );
        Ok(())
    }
}
impl FolderConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.backend.validate()?;
        anyhow::ensure!(
            !self.name.trim().is_empty() && Path::new(&self.local_path).is_dir(),
            "文件夹名称或本地路径无效"
        );
        anyhow::ensure!(
            matches!(self.encryption, EncryptionConfig::Off),
            "加密功能尚未集成，请保持关闭"
        );
        let remote = match &self.backend {
            BackendConfig::S3 { root_prefix, .. } => root_prefix,
            BackendConfig::WebDav { root_path, .. } => root_path,
            BackendConfig::AliyunDrive { remote_dir, .. }
            | BackendConfig::BaiduPan { remote_dir, .. } => remote_dir,
            BackendConfig::Git {
                remote_url, branch, ..
            } => {
                if let Ok(url) = url::Url::parse(remote_url) {
                    anyhow::ensure!(
                        url.username().is_empty() && url.password().is_none(),
                        "请将 Git 凭据填写到独立的用户名和密码字段"
                    );
                }
                anyhow::ensure!(
                    !branch.starts_with('-')
                        && !branch.contains(['\\', ' ', ':', '~', '^', '?', '*', '['])
                        && !branch.contains(".."),
                    "Git 分支名称无效"
                );
                return Ok(());
            }
        };
        anyhow::ensure!(
            !remote.contains(['\\', '\0']) && remote.split('/').all(|p| p != ".." && p != "."),
            "远端根目录无效"
        );
        Ok(())
    }
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            concurrency: 3,
            max_retries: 5,
            full_scan_interval_min: 15,
            large_file_threshold: 512 * 1024 * 1024,
            language: "zh-CN".into(),
            close_to_tray: true,
            baidu_app_key: String::new(),
            baidu_secret_key: String::new(),
            aliyun_app_id: String::new(),
            aliyun_app_secret: String::new(),
            log_level: default_log_level(),
            log_to_file: true,
            deletion_guard: true,
            deletion_guard_threshold: 50,
            custom_log_dir: None,
            activity_retention_days: 7,
            max_upload_kbps: 0,
            max_download_kbps: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderConfig {
    pub id: Uuid,
    pub name: String,
    pub local_path: String,
    pub backend: BackendConfig,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default)]
    pub conflict_policy: ConflictPolicy,
    /// 用户附加的排除规则（glob）
    #[serde(default)]
    pub ignore_rules: Vec<String>,
    #[serde(default)]
    pub sync_mode: SyncMode,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    #[serde(default)]
    pub encryption: EncryptionConfig,
    /// 传输调度优先级（对标 Syncthing 与 PanguFlow）
    #[serde(default)]
    pub transfer_priority: TransferPriority,
    /// 单文件大小上限限制（MB，0 或 None 表示不限制）
    #[serde(default)]
    pub max_file_size_mb: Option<u64>,
    /// 忽略 Windows 隐藏与系统文件（对标 FreeFileSync 与 GoodSync）
    #[serde(default)]
    pub ignore_hidden: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_interval() -> u64 {
    300
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BackendConfig {
    // 注意：变体内的字段名必须用 camelCase（对齐前端 types.ts 与 Tauri IPC 传参），
    // 枚举级 rename_all 只作用于变体名，不改字段名。
    #[serde(rename = "s3", rename_all = "camelCase")]
    S3 {
        endpoint: String,
        region: String,
        bucket: String,
        access_key_id: String,
        /// 存于系统凭据管理器；此处仅存 keyring 引用是否存在的标志
        #[serde(default)]
        secret_access_key: String,
        #[serde(default)]
        path_style: bool,
        /// 仓库在 bucket 内的前缀路径，例如 "synccloud/docs"
        #[serde(default)]
        root_prefix: String,
    },
    #[serde(rename = "webdav", rename_all = "camelCase")]
    WebDav {
        url: String,
        username: String,
        #[serde(default)]
        password: String,
        /// 远端根目录（相对 WebDAV 服务器根），仓库将位于 <root>/synccloud
        #[serde(default)]
        root_path: String,
    },
    #[serde(rename = "git", rename_all = "camelCase")]
    Git {
        remote_url: String,
        #[serde(default = "default_branch")]
        branch: String,
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: String,
    },
    #[serde(rename = "baidu", rename_all = "camelCase")]
    BaiduPan {
        #[serde(default)]
        auth_id: String,
        #[serde(default)]
        account_name: String,
        app_key: String,
        #[serde(default)]
        secret_key: String,
        /// 同步根目录（默认 /apps/synccloud）
        #[serde(default = "default_remote_dir")]
        remote_dir: String,
    },
    #[serde(rename = "aliyun", rename_all = "camelCase")]
    AliyunDrive {
        #[serde(default)]
        auth_id: String,
        #[serde(default)]
        account_name: String,
        #[serde(default)]
        app_id: String,
        #[serde(default)]
        app_secret: String,
        #[serde(default = "default_remote_dir")]
        remote_dir: String,
    },
}

fn default_branch() -> String {
    "synccloud".into()
}

fn default_remote_dir() -> String {
    "/apps/synccloud".into()
}

impl BackendConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::WebDav { url, root_path, .. } => {
                let endpoint = url::Url::parse(url)?;
                anyhow::ensure!(
                    matches!(endpoint.scheme(), "https" | "http") && endpoint.host_str().is_some(),
                    "WebDAV 地址必须为 HTTP(S) URL"
                );
                anyhow::ensure!(
                    !root_path.contains('\\')
                        && root_path.split('/').all(|p| p != ".." && p != "."),
                    "WebDAV 根路径无效"
                );
            }
            Self::S3 {
                endpoint,
                root_prefix,
                ..
            } => {
                let endpoint = url::Url::parse(endpoint)?;
                anyhow::ensure!(
                    matches!(endpoint.scheme(), "https" | "http"),
                    "S3 地址必须为 HTTP(S) URL"
                );
                anyhow::ensure!(
                    !root_prefix.contains('\\')
                        && root_prefix.split('/').all(|p| p != ".." && p != "."),
                    "S3 前缀无效"
                );
            }
            Self::Git {
                remote_url, branch, ..
            } => {
                anyhow::ensure!(
                    !remote_url.starts_with('-')
                        && !branch.is_empty()
                        && !branch.starts_with('-')
                        && !branch.contains("..")
                        && !branch.contains([' ', ':', '~', '^', '?', '*', '[', '\\']),
                    "Git 地址或分支无效"
                );
                if let Ok(url) = url::Url::parse(remote_url) {
                    if matches!(url.scheme(), "http" | "https") {
                        anyhow::ensure!(
                            url.username().is_empty() && url.password().is_none(),
                            "请将 Git 凭据填写到独立用户名和密码字段"
                        );
                    }
                }
            }
            Self::BaiduPan { remote_dir, .. } | Self::AliyunDrive { remote_dir, .. } => {
                anyhow::ensure!(
                    !remote_dir.contains(['\\', '\0'])
                        && remote_dir.split('/').all(|p| p != ".." && p != "."),
                    "云盘根路径无效"
                );
            }
        }
        Ok(())
    }
    pub fn secret_mut(&mut self) -> &mut String {
        match self {
            Self::S3 {
                secret_access_key, ..
            } => secret_access_key,
            Self::WebDav { password, .. } | Self::Git { password, .. } => password,
            Self::BaiduPan { secret_key, .. } => secret_key,
            Self::AliyunDrive { app_secret, .. } => app_secret,
        }
    }
    pub fn resolve_credentials(&mut self) -> anyhow::Result<()> {
        let secret = self.secret_mut();
        *secret = crate::credential::resolve(secret)?;
        Ok(())
    }
    pub fn kind(&self) -> &'static str {
        match self {
            BackendConfig::S3 { .. } => "s3",
            BackendConfig::WebDav { .. } => "webdav",
            BackendConfig::Git { .. } => "git",
            BackendConfig::BaiduPan { .. } => "baidu",
            BackendConfig::AliyunDrive { .. } => "aliyun",
        }
    }

    /// 序列化给前端展示（脱敏）。
    pub fn display(&self) -> String {
        match self {
            BackendConfig::S3 {
                endpoint,
                bucket,
                root_prefix,
                ..
            } => {
                if root_prefix.is_empty() {
                    format!("S3://{bucket}@{endpoint}")
                } else {
                    format!("S3://{bucket}/{root_prefix}@{endpoint}")
                }
            }
            BackendConfig::WebDav { url, root_path, .. } => {
                if root_path.is_empty() {
                    format!("WebDAV://{url}")
                } else {
                    format!("WebDAV://{url}/{root_path}")
                }
            }
            BackendConfig::Git {
                remote_url, branch, ..
            } => format!("Git://{remote_url}#{branch}"),
            BackendConfig::BaiduPan { remote_dir, .. } => format!("百度网盘://{remote_dir}"),
            BackendConfig::AliyunDrive { remote_dir, .. } => format!("阿里云盘://{remote_dir}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum EncryptionConfig {
    #[default]
    Off,
    /// 密码派生密钥；密码本身存系统凭据管理器
    Passphrase,
}

impl AppConfig {
    /// 加载配置，不存在则创建带随机设备标识的默认配置。
    pub fn load_or_create(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join("config.json");
        if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            let mut cfg: AppConfig = serde_json::from_str(&raw)?;
            if cfg.device_id.is_empty() {
                cfg.device_id = Uuid::new_v4().to_string();
            }
            // 完成凭据写入及回读后才替换原明文配置。
            cfg.protect_credentials()?;
            if serde_json::to_value(&cfg)? != serde_json::from_str::<serde_json::Value>(&raw)? {
                cfg.save(dir)?;
            }
            return Ok(cfg);
        }
        let hostname = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "Device".into());
        let cfg = AppConfig {
            device_name: hostname,
            device_id: Uuid::new_v4().to_string(),
            folders: Vec::new(),
            settings: GlobalSettings::default(),
        };
        cfg.save(dir)?;
        Ok(cfg)
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("config.json");
        let mut protected = self.clone();
        protected.protect_credentials()?;
        let raw = serde_json::to_string_pretty(&protected)?;
        // 先写临时文件再原子替换，避免崩溃损坏配置
        let tmp = dir.join("config.json.tmp");
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn protect_credentials(&mut self) -> anyhow::Result<()> {
        for folder in &mut self.folders {
            crate::credential::protect(
                folder.backend.secret_mut(),
                &format!("folder:{}:backend", folder.id),
            )?;
        }
        crate::credential::protect(
            &mut self.settings.baidu_secret_key,
            "settings:baidu_secret_key",
        )?;
        crate::credential::protect(
            &mut self.settings.aliyun_app_secret,
            "settings:aliyun_app_secret",
        )?;
        Ok(())
    }
}

static CURRENT_DATA_DIR: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// 系统默认数据目录：`%APPDATA%/SyncCloud`（Windows）。
pub fn default_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("SyncCloud")
}

/// 应用当前生效的数据目录（支持自定义配置）。
pub fn app_data_dir() -> PathBuf {
    if let Ok(guard) = CURRENT_DATA_DIR.read() {
        if let Some(dir) = guard.as_ref() {
            return dir.clone();
        }
    }

    let default_dir = default_data_dir();
    let location_file = default_dir.join("storage_location.txt");
    let resolved = if location_file.exists() {
        if let Ok(txt) = std::fs::read_to_string(&location_file) {
            let path_str = txt.trim();
            if !path_str.is_empty() {
                PathBuf::from(path_str)
            } else {
                default_dir
            }
        } else {
            default_dir
        }
    } else {
        default_dir
    };

    if let Ok(mut guard) = CURRENT_DATA_DIR.write() {
        *guard = Some(resolved.clone());
    }
    resolved
}

/// 测试专用：仅设置进程内数据目录（不持久化 storage_location.txt，
/// 避免测试污染真实应用的数据目录重定向）。
#[doc(hidden)]
pub fn set_app_data_dir_in_memory(dir: PathBuf) {
    if let Ok(mut guard) = CURRENT_DATA_DIR.write() {
        *guard = Some(dir);
    }
}

/// 设置并迁移应用数据目录。
pub fn set_app_data_dir(new_dir: PathBuf, copy_existing: bool) -> anyhow::Result<()> {
    let current = app_data_dir().canonicalize()?;
    anyhow::ensure!(new_dir.is_absolute(), "数据目录必须为绝对路径");
    std::fs::create_dir_all(
        new_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("目标目录无父目录"))?,
    )?;
    let parent = new_dir.parent().unwrap().canonicalize()?;
    let destination = parent.join(
        new_dir
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("不能使用磁盘根目录"))?,
    );
    let resolved = if destination.exists() {
        destination.canonicalize()?
    } else {
        destination.clone()
    };
    if resolved == current {
        return Ok(());
    }
    anyhow::ensure!(
        !resolved.starts_with(&current) && !current.starts_with(&resolved),
        "新旧数据目录不能互相包含"
    );
    anyhow::ensure!(copy_existing, "为保留账户和基线，请勾选迁移现有数据");
    if destination.exists() {
        anyhow::ensure!(
            std::fs::read_dir(&destination)?.next().is_none(),
            "目标数据目录必须为空，以免覆盖已有数据"
        );
    }
    let staging = parent.join(format!(".synccloud-migrate-{}", Uuid::new_v4()));
    copy_dir_recursive(&current, &staging)?;
    let config: AppConfig = serde_json::from_slice(&std::fs::read(staging.join("config.json"))?)?;
    let original: AppConfig = serde_json::from_slice(&std::fs::read(current.join("config.json"))?)?;
    anyhow::ensure!(
        serde_json::to_value(config)? == serde_json::to_value(original)?,
        "迁移配置校验失败"
    );
    if destination.exists() {
        std::fs::remove_dir(&destination)?;
    }
    std::fs::rename(&staging, &destination)?;
    let default = default_data_dir();
    std::fs::create_dir_all(&default)?;
    let pointer = default.join("storage_location.txt");
    let temp = default.join("storage_location.txt.tmp");
    std::fs::write(&temp, destination.to_string_lossy().as_bytes())?;
    std::fs::rename(temp, pointer)?;
    // Existing database handles keep the original directory until app restart.
    // The manager remains stopped in maintenance mode; source is retained for rollback.
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    copy_data_tree(src, src, dst)
}
fn is_state_database(root: &Path, path: &Path) -> bool {
    path == root.join("history.db")
        || (path.file_name().is_some_and(|n| n == "state.db")
            && path.parent().and_then(|p| p.parent()) == Some(root.join("data").as_path()))
}
fn copy_data_tree(root: &Path, src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if src == root && (text == "temp" || text == "storage_location.txt") {
            continue;
        }
        if let Some(db) = text
            .strip_suffix("-wal")
            .or_else(|| text.strip_suffix("-shm"))
        {
            if is_state_database(root, &src.join(db)) {
                continue;
            }
        }
        anyhow::ensure!(
            !kind.is_symlink(),
            "数据目录包含链接，停止迁移: {}",
            entry.path().display()
        );
        let target = dst.join(&name);
        if kind.is_dir() {
            anyhow::ensure!(
                entry
                    .path()
                    .canonicalize()?
                    .starts_with(src.canonicalize()?),
                "数据目录包含越界链接"
            );
            copy_data_tree(root, &entry.path(), &target)?;
        } else if is_state_database(root, &entry.path()) {
            let db = rusqlite::Connection::open_with_flags(
                entry.path(),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            )?;
            db.busy_timeout(std::time::Duration::from_secs(15))?;
            db.execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()])?;
            let backup = rusqlite::Connection::open(&target)?;
            let check: String = backup.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            anyhow::ensure!(check == "ok", "迁移数据库校验失败: {text}");
        } else {
            std::fs::copy(entry.path(), &target)?;
            anyhow::ensure!(
                crate::sync::scanner::hash_file(&entry.path())?
                    == crate::sync::scanner::hash_file(&target)?,
                "迁移文件校验失败: {text}"
            );
        }
    }
    Ok(())
}

/// 某同步文件夹的状态数据库目录。
pub fn folder_data_dir(folder_id: &Uuid) -> PathBuf {
    app_data_dir().join("data").join(folder_id.to_string())
}

/// 云盘授权账户记录（支持多百度网盘、多阿里云盘账户）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudAccount {
    pub id: String,
    pub provider: String,
    pub name: String,
    #[serde(default)]
    pub app_key: Option<String>,
    #[serde(default)]
    pub secret_key: Option<String>,
    pub created_at: String,
}

static ACCOUNTS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub fn load_cloud_accounts(base_dir: &Path) -> anyhow::Result<Vec<CloudAccount>> {
    let _guard = ACCOUNTS_LOCK.lock().unwrap();
    load_cloud_accounts_inner(base_dir)
}
pub fn save_cloud_accounts(base_dir: &Path, accounts: &[CloudAccount]) -> anyhow::Result<()> {
    let _guard = ACCOUNTS_LOCK.lock().unwrap();
    save_cloud_accounts_inner(base_dir, accounts)
}
pub fn upsert_cloud_account(
    base_dir: &Path,
    mut account: CloudAccount,
) -> anyhow::Result<CloudAccount> {
    let _guard = ACCOUNTS_LOCK.lock().unwrap();
    if let Some(secret) = account.secret_key.as_mut() {
        crate::credential::protect(secret, &format!("account:{}:app_secret", account.id))?;
    }
    let mut list = load_cloud_accounts_inner(base_dir)?;
    list.retain(|a| a.id != account.id);
    list.push(account.clone());
    save_cloud_accounts_inner(base_dir, &list)?;
    Ok(account)
}
pub fn remove_cloud_account(base_dir: &Path, id: &str) -> anyhow::Result<Option<CloudAccount>> {
    let _guard = ACCOUNTS_LOCK.lock().unwrap();
    let mut list = load_cloud_accounts_inner(base_dir)?;
    let account = list.iter().find(|a| a.id == id).cloned();
    list.retain(|a| a.id != id);
    save_cloud_accounts_inner(base_dir, &list)?;
    Ok(account)
}

fn load_cloud_accounts_inner(base_dir: &Path) -> anyhow::Result<Vec<CloudAccount>> {
    let p = base_dir.join("accounts.json");
    let bytes = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut list: Vec<CloudAccount> = serde_json::from_slice(&bytes)?;
    let mut migrated = false;
    for account in &mut list {
        if let Some(secret) = account.secret_key.as_mut() {
            if !secret.is_empty() && !secret.starts_with("keyring:") {
                crate::credential::protect(secret, &format!("account:{}:app_secret", account.id))?;
                migrated = true;
            }
        }
    }
    if migrated {
        save_cloud_accounts_inner(base_dir, &list)?;
    }
    Ok(list)
}

fn save_cloud_accounts_inner(base_dir: &Path, accounts: &[CloudAccount]) -> anyhow::Result<()> {
    let p = base_dir.join("accounts.json");
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut protected = accounts.to_vec();
    for account in &mut protected {
        if let Some(secret) = account.secret_key.as_mut() {
            crate::credential::protect(secret, &format!("account:{}:app_secret", account.id))?;
        }
    }
    let data = serde_json::to_vec_pretty(&protected)?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_keeps_nested_business_temp_names() {
        let root = std::env::temp_dir().join(format!("synccloud-copy-names-{}", Uuid::new_v4()));
        let src = root.join("source");
        let dst = root.join("copy");
        let staged = src.join("data/task/git-stage");
        std::fs::create_dir_all(staged.join("temp")).unwrap();
        std::fs::write(staged.join("temp/report.txt"), b"pending upload").unwrap();
        std::fs::write(staged.join("storage_location.txt"), b"business file").unwrap();
        std::fs::create_dir_all(src.join("temp")).unwrap();
        std::fs::write(src.join("temp/discard"), b"application temp").unwrap();
        copy_dir_recursive(&src, &dst).unwrap();
        assert_eq!(
            std::fs::read(dst.join("data/task/git-stage/temp/report.txt")).unwrap(),
            b"pending upload"
        );
        assert_eq!(
            std::fs::read(dst.join("data/task/git-stage/storage_location.txt")).unwrap(),
            b"business file"
        );
        assert!(!dst.join("temp").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migration_copies_live_sqlite_with_wal_and_propagates_copy_errors() {
        let root = std::env::temp_dir().join(format!("synccloud-migration-{}", Uuid::new_v4()));
        let src = root.join("source");
        let dst = root.join("copy");
        std::fs::create_dir_all(&src).unwrap();
        let db = rusqlite::Connection::open(src.join("history.db")).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE proof(value TEXT);INSERT INTO proof VALUES('committed WAL');").unwrap();
        copy_dir_recursive(&src, &dst).unwrap();
        let copy = rusqlite::Connection::open(dst.join("history.db")).unwrap();
        let value: String = copy
            .query_row("SELECT value FROM proof", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, "committed WAL");
        let blocked_source = root.join("blocked-source");
        let blocked_target = root.join("blocked-target");
        std::fs::create_dir_all(&blocked_source).unwrap();
        std::fs::write(blocked_source.join("blocked"), b"original").unwrap();
        std::fs::create_dir_all(blocked_target.join("blocked")).unwrap();
        assert!(copy_dir_recursive(&blocked_source, &blocked_target).is_err());
        assert_eq!(
            std::fs::read(blocked_source.join("blocked")).unwrap(),
            b"original"
        );
        drop(copy);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plaintext_credentials_migrate_only_after_verified_vault_write() {
        let root = std::env::temp_dir().join(format!("synccloud-credentials-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut cfg = AppConfig {
            device_name: "fixture".into(),
            device_id: Uuid::new_v4().to_string(),
            folders: Vec::new(),
            settings: GlobalSettings::default(),
        };
        cfg.settings.baidu_secret_key = "fixture-secret-unique".into();
        std::fs::write(root.join("config.json"), serde_json::to_vec(&cfg).unwrap()).unwrap();
        let protected = AppConfig::load_or_create(&root).unwrap();
        let reference = &protected.settings.baidu_secret_key;
        assert!(reference.starts_with("keyring:"));
        assert_eq!(
            crate::credential::resolve(reference).unwrap(),
            "fixture-secret-unique"
        );
        assert!(!std::fs::read_to_string(root.join("config.json"))
            .unwrap()
            .contains("fixture-secret-unique"));
        let blocked = root.join("blocked");
        std::fs::create_dir_all(blocked.join("config.json")).unwrap();
        assert!(protected.save(&blocked).is_err());
        assert_eq!(
            crate::credential::resolve(reference).unwrap(),
            "fixture-secret-unique"
        );
        crate::credential::delete_secret(reference.strip_prefix("keyring:").unwrap()).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 前端 IPC 发送的是 camelCase 字段（types.ts），此处固定该契约，
    /// 防止再次出现 `missing field 'remote_url'` 一类的反序列化错位。
    #[test]
    fn backend_config_deserializes_frontend_camel_case() {
        let s3: BackendConfig =
            serde_json::from_str(r#"{"type":"s3","endpoint":"http://e","region":"r","bucket":"b","accessKeyId":"ak","secretAccessKey":"sk","pathStyle":true,"rootPrefix":"p"}"#).unwrap();
        assert!(
            matches!(&s3, BackendConfig::S3 { access_key_id, path_style, root_prefix, .. } if access_key_id=="ak" && *path_style && root_prefix=="p")
        );

        let dav: BackendConfig = serde_json::from_str(
            r#"{"type":"webdav","url":"https://d","username":"u","password":"p","rootPath":"/x"}"#,
        )
        .unwrap();
        assert!(matches!(&dav, BackendConfig::WebDav { root_path, .. } if root_path == "/x"));

        let git: BackendConfig = serde_json::from_str(
            r#"{"type":"git","remoteUrl":"https://gitee.com/a/b.git","username":"u","password":"t"}"#,
        )
        .unwrap();
        assert!(
            matches!(&git, BackendConfig::Git { remote_url, branch, .. } if remote_url=="https://gitee.com/a/b.git" && branch=="synccloud")
        );

        let baidu: BackendConfig = serde_json::from_str(
            r#"{"type":"baidu","appKey":"k","secretKey":"s","remoteDir":"/apps/synccloud"}"#,
        )
        .unwrap();
        assert!(
            matches!(&baidu, BackendConfig::BaiduPan { app_key, remote_dir, .. } if app_key=="k" && remote_dir=="/apps/synccloud")
        );

        let aliyun: BackendConfig =
            serde_json::from_str(r#"{"type":"aliyun","appId":"i","appSecret":"s"}"#).unwrap();
        assert!(matches!(&aliyun, BackendConfig::AliyunDrive { app_id, .. } if app_id == "i"));
    }

    /// 序列化往返保持同一 camelCase 契约（config.json 落盘格式）。
    #[test]
    fn backend_config_round_trip() {
        let cfg = BackendConfig::Git {
            remote_url: "https://example.com/r.git".into(),
            branch: "main".into(),
            username: "u".into(),
            password: "p".into(),
        };
        let raw = serde_json::to_string(&cfg).unwrap();
        assert!(
            raw.contains(r#""remoteUrl":"https://example.com/r.git""#),
            "raw = {raw}"
        );
        let back: BackendConfig = serde_json::from_str(&raw).unwrap();
        assert_eq!(cfg.display(), back.display());
    }

    /// 旧版 config.json 的 settings 没有云盘凭据字段，加载时取默认空串。
    #[test]
    fn global_settings_defaults_for_old_config() {
        let s: GlobalSettings = serde_json::from_str(
            r#"{"concurrency":5,"maxRetries":9,"fullScanIntervalMin":30,"largeFileThreshold":1024,"language":"zh-CN"}"#,
        )
        .unwrap();
        assert_eq!(s.concurrency, 5);
        assert!(s.baidu_app_key.is_empty() && s.aliyun_app_id.is_empty());
    }
}
