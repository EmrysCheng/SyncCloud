//! 应用配置：全局设置 + 同步文件夹列表，持久化到 `%APPDATA%/SyncCloud/config.json`。
//! 敏感字段（密码/密钥）不落盘，存入系统凭据管理器（见 credential.rs）。

use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::sync::types::{ConflictPolicy, Direction, SyncMode};

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
            BackendConfig::S3 { endpoint, bucket, root_prefix, .. } => {
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
            BackendConfig::Git { remote_url, branch, .. } => format!("Git://{remote_url}#{branch}"),
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
        let raw = serde_json::to_string_pretty(self)?;
        // 先写临时文件再原子替换，避免崩溃损坏配置
        let tmp = dir.join("config.json.tmp");
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, &path)?;
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

/// 设置并迁移应用数据目录。
pub fn set_app_data_dir(new_dir: PathBuf, copy_existing: bool) -> anyhow::Result<()> {
    let current_dir = app_data_dir();
    let default_dir = default_data_dir();

    if current_dir == new_dir {
        return Ok(());
    }

    std::fs::create_dir_all(&new_dir)?;

    if copy_existing && current_dir.exists() {
        copy_dir_recursive(&current_dir, &new_dir)?;
    }

    std::fs::create_dir_all(&default_dir)?;
    let location_file = default_dir.join("storage_location.txt");

    if new_dir == default_dir {
        if location_file.exists() {
            let _ = std::fs::remove_file(&location_file);
        }
    } else {
        std::fs::write(&location_file, new_dir.to_string_lossy().as_bytes())?;
    }

    if let Ok(mut guard) = CURRENT_DATA_DIR.write() {
        *guard = Some(new_dir);
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();
        if name_str == "temp" || name_str == "storage_location.txt" {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(&file_name);
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            let _ = std::fs::copy(&src_path, &dst_path);
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

pub fn load_cloud_accounts(base_dir: &Path) -> Vec<CloudAccount> {
    let p = base_dir.join("accounts.json");
    if let Ok(bytes) = std::fs::read(&p) {
        if let Ok(list) = serde_json::from_slice(&bytes) {
            return list;
        }
    }
    Vec::new()
}

pub fn save_cloud_accounts(base_dir: &Path, accounts: &[CloudAccount]) -> anyhow::Result<()> {
    let p = base_dir.join("accounts.json");
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(accounts)?;
    std::fs::write(&p, data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 前端 IPC 发送的是 camelCase 字段（types.ts），此处固定该契约，
    /// 防止再次出现 `missing field 'remote_url'` 一类的反序列化错位。
    #[test]
    fn backend_config_deserializes_frontend_camel_case() {
        let s3: BackendConfig =
            serde_json::from_str(r#"{"type":"s3","endpoint":"http://e","region":"r","bucket":"b","accessKeyId":"ak","secretAccessKey":"sk","pathStyle":true,"rootPrefix":"p"}"#).unwrap();
        assert!(matches!(&s3, BackendConfig::S3 { access_key_id, path_style, root_prefix, .. } if access_key_id=="ak" && *path_style && root_prefix=="p"));

        let dav: BackendConfig = serde_json::from_str(
            r#"{"type":"webdav","url":"https://d","username":"u","password":"p","rootPath":"/x"}"#,
        )
        .unwrap();
        assert!(matches!(&dav, BackendConfig::WebDav { root_path, .. } if root_path == "/x"));

        let git: BackendConfig = serde_json::from_str(
            r#"{"type":"git","remoteUrl":"https://gitee.com/a/b.git","username":"u","password":"t"}"#,
        )
        .unwrap();
        assert!(matches!(&git, BackendConfig::Git { remote_url, branch, .. } if remote_url=="https://gitee.com/a/b.git" && branch=="synccloud"));

        let baidu: BackendConfig =
            serde_json::from_str(r#"{"type":"baidu","appKey":"k","secretKey":"s","remoteDir":"/apps/synccloud"}"#).unwrap();
        assert!(matches!(&baidu, BackendConfig::BaiduPan { app_key, remote_dir, .. } if app_key=="k" && remote_dir=="/apps/synccloud"));

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
        assert!(raw.contains(r#""remoteUrl":"https://example.com/r.git""#), "raw = {raw}");
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
