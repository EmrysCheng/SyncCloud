//! 阿里云盘后端（免应用扫码版）。
//!
//! 用户无需在开放平台注册应用、无需填写 AppId/AppSecret：
//! - 扫码授权与 AList 同款直连公共 OAuth（`open.aliyundrive.com`：
//!   `authorize?qrcode_id=<uuid>` 生成二维码 → `oauth/qrcode/<sid>/status` 轮询 →
//!   `oauth/access_token` 换取/刷新，全程无需 client_secret）；
//!   直连不可用时自动回退社区 OAuth 中转（默认 aliyundrive-oauth.messense.me，
//!   可用环境变量 ALIYUNDRIVE_OAUTH_SERVER 覆盖）；
//! - 二维码内容：`https://www.aliyundrive.com/o/oauth/authorize?sid=<sid>`（App 扫码确认）；
//! - 文件 API：`https://openapi.aliyundrive.com/adrive/v1.0/*`（Bearer + 浏览器 UA/Origin/Referer）；
//! - 令牌刷新走签发它的同一端点，refresh_token 每次轮换，须回写系统凭据管理器。
//!
//! 注意：OAuth 接口在等待授权等阶段会把 `authCode` 等字段置为 JSON `null`，
//! 必须以 null 容错方式解析（`.json()` 反序列化到 `String` 字段会直接报
//! `invalid type: null, expected a string`）。

use std::path::Path;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{sibling_meta, RemoteEntry, RemoteStore};
use crate::sync::types::Manifest;

const API: &str = "https://openapi.aliyundrive.com";
/// AList 同款公共 OAuth 端点（无需自建应用即可扫码授权）。
const OPEN_OAUTH: &str = "https://open.aliyundrive.com";
const QR_PAGE: &str = "https://www.aliyundrive.com/o/oauth/authorize";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

fn oauth_host() -> String {
    std::env::var("ALIYUNDRIVE_OAUTH_SERVER")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://aliyundrive-oauth.messense.me".into())
}

/// keyring 账户名（固定，因为不再区分应用）。
const KEYRING: &str = "aliyun:qr";

/// 上传分块大小（10MB，与 aliyundrive-webdav 一致）。
const CHUNK: usize = 10 * 1024 * 1024;

pub struct AliyunStore {
    auth_id: String,
    remote_dir: String,
    /// 元数据区（manifest/锁），位于同步根内部的 .synccloud。
    meta_dir: String,
    http: reqwest::Client,
    api_base: String,
    token: tokio::sync::Mutex<Option<TokenState>>,
    drive_id: tokio::sync::Mutex<Option<String>>,
    /// 目录路径 → file_id 缓存（None = 已知不存在）。
    /// 阿里云盘没有按路径直达的 API，逐级 list_children 是每轮同步的最大开销；
    /// 缓存后整轮只需对每个目录 list 一次。远端目录被外部删除时由
    /// api_json 的 NotFound 检测自愈清空。
    dir_cache: tokio::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
    mkdir_lock: tokio::sync::Mutex<()>,
    recovery_root: std::path::PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenState {
    access_token: String,
    refresh_token: String,
    /// Unix 秒
    expires_at: i64,
    /// 签发该令牌的 OAuth 端点（直连 open.aliyundrive.com 或社区中转）。
    /// 刷新必须走同一端点；为空表示旧版中转令牌。
    #[serde(default)]
    oauth_base: String,
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// 从 JSON 对象取字符串字段；字段缺失或为 null 都返回 None（null 容错）。
fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// 从 JSON 对象取整数字段；null / 缺失返回默认值。
fn int_field(v: &serde_json::Value, key: &str, default: i64) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(default)
}

// ---- 登录相关（供 IPC 命令调用） ----

/// 二维码会话：sid 前缀标记签发端点，轮询时回到同一端点。
const MODE_DIRECT: &str = "direct";
const MODE_RELAY: &str = "relay";

/// 生成扫码登录二维码：返回 (二维码内容, 浏览器兜底链接, 会话标识)。
/// 优先 AList 同款直连流程，失败自动回退社区中转。
pub async fn start_qr_login() -> anyhow::Result<(String, String, String)> {
    // 1) 直连：qrcode_id 用随机 UUID，无需任何 client 凭据
    match try_qr_direct().await {
        Ok((qr, link, sid)) => Ok((qr, link, format!("{MODE_DIRECT}|{sid}"))),
        Err(direct_err) => {
            tracing::warn!(error = %direct_err, "阿里云盘直连授权不可用，回退社区 OAuth 中转");
            // 2) 中转兜底
            let (qr, link, sid) = try_qr_relay().await?;
            Ok((qr, link, format!("{MODE_RELAY}|{sid}")))
        }
    }
}

/// AList 同款直连：GET open.aliyundrive.com/oauth/authorize?qrcode_id=<uuid>&scope=...
async fn try_qr_direct() -> anyhow::Result<(String, String, String)> {
    let http = oauth_client();
    let qr_id = uuid::Uuid::new_v4().to_string();
    let resp = http
        .get(format!("{OPEN_OAUTH}/oauth/authorize"))
        .query(&[
            ("qrcode_id", qr_id.as_str()),
            ("scope", "user:base,file:all:read,file:all:write"),
        ])
        .send()
        .await
        .map_err(|e| e.without_url())?;
    let (status, body) = read_body(resp).await?;
    check_http_ok("阿里云盘直连二维码", status, &body)?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let sid = str_field(&v, "sid")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("直连二维码响应缺少 sid: {}", "响应字段不完整"))?;
    let qr_url = str_field(&v, "qrCodeUrl").filter(|s| !s.is_empty());
    let qr = qr_url.unwrap_or_else(|| format!("{QR_PAGE}?sid={sid}"));
    Ok((qr.clone(), qr, sid))
}

/// 社区中转：POST <relay>/oauth/authorize/qrcode {"scopes":[...]}
async fn try_qr_relay() -> anyhow::Result<(String, String, String)> {
    let http = oauth_client();
    let resp = http
        .post(format!("{}/oauth/authorize/qrcode", oauth_host()))
        .json(&serde_json::json!({
            "scopes": ["user:base", "file:all:read", "file:all:write"],
        }))
        .send()
        .await
        .map_err(|e| e.without_url())?;
    let (status, body) = read_body(resp).await?;
    check_http_ok("阿里云盘二维码(中转)", status, &body)?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let sid = str_field(&v, "sid")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("二维码响应缺少 sid: {}", "响应字段不完整"))?;
    let qr = str_field(&v, "qrCodeUrl")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{QR_PAGE}?sid={sid}"));
    Ok((qr.clone(), qr, sid))
}

/// 轮询扫码状态；LoginSuccess 时换取 refresh_token 并存入 keyring。
/// `handle` 为 start_qr_login 返回的会话标识（`direct|<sid>` / `relay|<sid>`）。
pub async fn poll_qr_login(handle: &str, auth_id: Option<&str>) -> anyhow::Result<bool> {
    let lock = crate::credential::account_lock(&keyring_account(auth_id.unwrap_or("").trim()));
    let _account = lock.lock().await;
    poll_qr_login_unlocked(handle, auth_id).await
}
pub(crate) async fn poll_qr_login_unlocked(
    handle: &str,
    auth_id: Option<&str>,
) -> anyhow::Result<bool> {
    let (mode, sid) = match handle.split_once('|') {
        Some((m, s)) if !s.is_empty() => (m, s),
        // 兼容旧调用方直接传 sid：默认按中转会话处理
        _ => (MODE_RELAY, handle),
    };
    let token_base = match mode {
        MODE_DIRECT => OPEN_OAUTH.to_string(),
        _ => oauth_host(),
    };
    let http = oauth_client();

    // 状态查询必须直接请求阿里云盘官方扫码接口（openapi.alipan.com 或 open.aliyundrive.com）
    // 社区中转服务不转发该轮询路由，直接请求中转服务会返回 HTTP 404
    let status_urls = [
        format!("https://openapi.alipan.com/oauth/qrcode/{sid}/status"),
        format!("https://open.aliyundrive.com/oauth/qrcode/{sid}/status"),
    ];

    let mut last_err = None;
    let mut resp_data = None;

    for url in &status_urls {
        match http.get(url).send().await {
            Ok(resp) => match read_body(resp).await {
                Ok((status, body)) => {
                    if status.is_success() {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                            resp_data = Some(v);
                            break;
                        }
                    } else {
                        last_err = Some(format!("HTTP {status}: {body}"));
                    }
                }
                Err(e) => last_err = Some(e.to_string()),
            },
            Err(e) => last_err = Some(e.to_string()),
        }
    }

    let v = match resp_data {
        Some(v) => v,
        None => anyhow::bail!(
            "阿里云盘扫码状态查询失败: {}",
            last_err.unwrap_or_else(|| "无法连接官方扫码状态服务".into())
        ),
    };

    // null 容错：等待阶段 authCode 为 null，LoginSuccess 后才是字符串
    let st = str_field(&v, "status").unwrap_or_default();
    match st.as_str() {
        "LoginSuccess" => {
            let code = str_field(&v, "authCode").filter(|s| !s.is_empty());
            let code = match code {
                Some(c) => c,
                None => anyhow::bail!(
                    "扫码已确认但接口未返回授权码（authCode 为空），请重新生成二维码再试"
                ),
            };
            let t = exchange_by_code(&code, &token_base).await?;
            let key = auth_id
                .filter(|s| !s.trim().is_empty())
                .map(|id| format!("aliyun:account:{}", id.trim()))
                .unwrap_or_else(|| KEYRING.to_string());
            crate::credential::save_secret(&key, &serde_json::to_string(&t)?)?;
            Ok(true)
        }
        "WaitLogin" | "ScanSuccess" => Ok(false),
        "QRCodeExpired" => anyhow::bail!("二维码已过期，请重新生成"),
        other => anyhow::bail!("阿里云盘扫码状态异常: {other}"),
    }
}

/// 用授权码换 token（authorization_code）。
/// 社区中转服务在 authorization_code 响应中通常仅包含 {"refresh_token": "..."}，
/// 此时自动发起一次 refresh_token 请求换取完整的 access_token 与有效期。
async fn exchange_by_code(code: &str, base: &str) -> anyhow::Result<TokenState> {
    let http = oauth_client();
    let resp = http
        .post(format!("{base}/oauth/access_token"))
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "code": code,
        }))
        .send()
        .await
        .map_err(|e| e.without_url())?;
    let (status, body) = read_body(resp).await?;
    check_http_ok("阿里云盘授权码兑换", status, &body)?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let access = str_field(&v, "access_token").filter(|s| !s.is_empty());
    let refresh = str_field(&v, "refresh_token").filter(|s| !s.is_empty());

    match (access, refresh) {
        (Some(access_token), Some(refresh_token)) => {
            let expires_in = int_field(&v, "expires_in", 7200);
            Ok(TokenState {
                access_token,
                refresh_token,
                expires_at: now_ts() + expires_in.max(0) - 120,
                oauth_base: base.to_string(),
            })
        }
        (None, Some(refresh_token)) => {
            tracing::info!("阿里云盘授权成功，正在通过 refresh_token 获取 access_token...");
            let refresh_resp = http
                .post(format!("{base}/oauth/access_token"))
                .json(&serde_json::json!({
                    "grant_type": "refresh_token",
                    "refresh_token": refresh_token,
                }))
                .send()
                .await
                .map_err(|e| e.without_url())?;
            token_from(refresh_resp, base).await
        }
        _ => anyhow::bail!("阿里云盘令牌响应缺少必要字段: {}", "响应字段不完整"),
    }
}

/// 从 access_token 响应解析 TokenState（null 容错）。
async fn token_from(resp: reqwest::Response, base: &str) -> anyhow::Result<TokenState> {
    let (status, body) = read_body(resp).await?;
    check_http_ok("阿里云盘令牌", status, &body)?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let access = str_field(&v, "access_token").filter(|s| !s.is_empty());
    let refresh = str_field(&v, "refresh_token").filter(|s| !s.is_empty());
    let (access_token, refresh_token) = match (access, refresh) {
        (Some(a), Some(r)) => (a, r),
        _ => anyhow::bail!("阿里云盘令牌响应缺少字段: {}", "响应字段不完整"),
    };
    let expires_in = int_field(&v, "expires_in", 7200);
    Ok(TokenState {
        access_token,
        refresh_token,
        expires_at: now_ts() + expires_in.max(0) - 120,
        oauth_base: base.to_string(),
    })
}

async fn read_body(resp: reqwest::Response) -> anyhow::Result<(reqwest::StatusCode, String)> {
    let status = resp.status();
    Ok((status, resp.text().await?))
}

fn check_http_ok(what: &str, status: reqwest::StatusCode, body: &str) -> anyhow::Result<()> {
    if status.is_success() {
        return Ok(());
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        let c = str_field(&v, "code").unwrap_or_default();
        let m = str_field(&v, "message").unwrap_or_default();
        if !m.is_empty() || !c.is_empty() {
            anyhow::bail!("{what}失败: {m} ({c})");
        }
    }
    anyhow::bail!("{what}返回 HTTP {status}，响应格式异常")
}

/// OAuth 专用客户端：浏览器 UA + 适中超时（扫码轮询由前端每 3 秒驱动一次，
/// 单次请求不宜久等，避免界面长时间无响应）。
fn oauth_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(UA)
        .build()
        .expect("reqwest client")
}

/// 带 API 所需默认头（浏览器 UA + Origin/Referer）的客户端。
fn api_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Origin", "https://www.aliyundrive.com".parse().unwrap());
    headers.insert("Referer", "https://www.aliyundrive.com/".parse().unwrap());
    reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .user_agent(UA)
        .default_headers(headers)
        .build()
        .expect("reqwest client")
}

// ---- 数据模型 ----

#[derive(Debug, Deserialize, Clone)]
#[allow(non_snake_case)]
struct DriveItem {
    file_id: String,
    name: String,
    #[serde(rename = "type")]
    item_type: String,
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct ListResp {
    #[serde(default, deserialize_with = "null_default")]
    items: Vec<DriveItem>,
    #[serde(default, deserialize_with = "null_default")]
    next_marker: String,
}

#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct DriveInfoResp {
    #[serde(default, deserialize_with = "null_default")]
    default_drive_id: String,
}

#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct CreateResp {
    file_id: String,
    #[serde(default, deserialize_with = "null_default")]
    upload_id: String,
    #[serde(default, deserialize_with = "null_default")]
    exist: bool,
    #[serde(default, deserialize_with = "null_default")]
    part_info_list: Vec<PartInfo>,
    #[serde(default, deserialize_with = "null_default")]
    rapid_upload: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(non_snake_case)]
struct PartInfo {
    part_number: u64,
    #[serde(default, deserialize_with = "null_default")]
    upload_url: String,
}

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct DownloadUrlResp {
    #[serde(default, deserialize_with = "null_default")]
    url: String,
}

fn keyring_account(auth_id: &str) -> String {
    if !auth_id.trim().is_empty() {
        format!("aliyun:account:{}", auth_id.trim())
    } else {
        KEYRING.to_string()
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;
    #[tokio::test]
    async fn upload_recovery_preserves_old_until_publish_is_verified() {
        use crate::storage::test_http::{serve, Response};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        };
        let mode = Arc::new(AtomicUsize::new(0));
        let current = mode.clone();
        let deleted = Arc::new(Mutex::new(Vec::new()));
        let ids = deleted.clone();
        let server = serve(move |request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if request.path.ends_with("/delete") {
                ids.lock()
                    .unwrap()
                    .push(body["file_id"].as_str().unwrap().to_string());
                return Response::json(serde_json::json!({}));
            }
            let parent = match current.load(Ordering::SeqCst) {
                0 => "stage",
                1 => "parent",
                _ => "unexpected",
            };
            Response::json(serde_json::json!({"parent_file_id":parent,"name":"a.txt","size":3}))
        })
        .await;
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let key = keyring_account(&id);
        let mut store = AliyunStore::new(id, "/sandbox".into());
        store.api_base = server.url.clone();
        store.recovery_root =
            std::env::temp_dir().join(format!("synccloud-recover-{}", uuid::Uuid::new_v4()));
        *store.drive_id.lock().await = Some("drive".into());
        store
            .save_token(&TokenState {
                access_token: "fixture".into(),
                refresh_token: "unused".into(),
                expires_at: now_ts() + 3600,
                oauth_base: server.url.clone(),
            })
            .await
            .unwrap();
        let record = UploadRecovery {
            path: "sandbox/a.txt".into(),
            parent: "parent".into(),
            name: "a.txt".into(),
            staging: "stage".into(),
            new_id: "new".into(),
            old_id: Some("old".into()),
            size: 3,
            uploaded: true,
        };
        let path = store.recovery_root.join("record.json");
        for scenario in 0..3 {
            mode.store(scenario, Ordering::SeqCst);
            deleted.lock().unwrap().clear();
            store.save_upload_record(&path, &record).await.unwrap();
            store.save_upload_record(&path, &record).await.unwrap();
            let result = store.recover_uploads(None).await;
            if scenario < 2 {
                result.unwrap();
                assert!(!path.exists());
                assert_eq!(
                    *deleted.lock().unwrap(),
                    vec![if scenario == 0 { "new" } else { "old" }.to_string()]
                );
            } else {
                assert!(result.is_err());
                assert!(path.exists());
                assert!(deleted.lock().unwrap().is_empty());
            }
        }
        std::fs::remove_dir_all(&store.recovery_root).unwrap();
        crate::credential::delete_secret(&key).unwrap();
    }
    #[tokio::test]
    async fn concurrent_aliyun_refresh_persists_one_rotated_token() {
        use crate::storage::test_http::{serve, Response};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let refreshes = Arc::new(AtomicUsize::new(0));
        let count = refreshes.clone();
        let server=serve(move|_|{count.fetch_add(1,Ordering::SeqCst);Response::json(serde_json::json!({"access_token":"new","refresh_token":"rotated","expires_in":3600}))}).await;
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let key = keyring_account(&id);
        let store = AliyunStore::new(id.clone(), "/sandbox".into());
        store
            .save_token(&TokenState {
                access_token: "old".into(),
                refresh_token: "fixture".into(),
                expires_at: now_ts() + 3600,
                oauth_base: server.url.clone(),
            })
            .await
            .unwrap();
        let other = AliyunStore::new(id, "/other-root".into());
        let (a, b) = tokio::join!(
            store.refresh_access_token(Some("old")),
            other.refresh_access_token(Some("old"))
        );
        assert_eq!(a.unwrap(), "new");
        assert_eq!(b.unwrap(), "new");
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        let persisted: TokenState =
            serde_json::from_str(&crate::credential::load_secret(&key).unwrap().unwrap()).unwrap();
        assert_eq!(persisted.refresh_token, "rotated");
        crate::credential::delete_secret(&key).unwrap();
    }

    #[test]
    fn directory_listing_accepts_null_folder_size() {
        let response = serde_json::json!({"items":[
            {"file_id":"folder", "name":"docs", "type":"folder", "size":null},
            {"file_id":"file", "name":"a.txt", "type":"file", "size":12}
        ], "next_marker":""});
        let parsed: ListResp = serde_json::from_value(response).expect("folder size may be null");
        assert_eq!(parsed.items.len(), 2);
    }

    #[tokio::test]
    async fn nullable_paged_directory_fetches_unknown_file_size() {
        use crate::storage::test_http::{serve, Response};
        let unknown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = unknown.clone();
        let server=serve(move|request|{
            let body:serde_json::Value=serde_json::from_slice(&request.body).unwrap();
            if request.path.ends_with("/get") {return Response::json(if flag.load(std::sync::atomic::Ordering::SeqCst){serde_json::json!({"size":null})}else{serde_json::json!({"size":4})});}
            if body.get("marker").is_some(){return Response::json(serde_json::json!({"items":null,"next_marker":null}));}
            Response::json(serde_json::json!({"items":[{"file_id":"d","name":"empty","type":"folder","size":null},{"file_id":"f","name":"file","type":"file","size":null}],"next_marker":"page2"}))
        }).await;
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let key = keyring_account(&id);
        let mut store = AliyunStore::new(id, "/sandbox".into());
        store.api_base = server.url.clone();
        *store.drive_id.lock().await = Some("drive".into());
        store
            .save_token(&TokenState {
                access_token: "fixture".into(),
                refresh_token: "unused".into(),
                expires_at: now_ts() + 3600,
                oauth_base: server.url.clone(),
            })
            .await
            .unwrap();
        let result = store.list_children("root").await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].size, Some(4));
        unknown.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(store.list_children("root").await.is_err());
        crate::credential::delete_secret(&key).unwrap();
    }

    #[tokio::test]
    async fn malformed_upload_preserves_existing_file() {
        use crate::storage::test_http::{serve, Response};
        let deleted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = deleted.clone();
        let server=serve(move|request|{
            if request.path.ends_with("/delete"){flag.store(true,std::sync::atomic::Ordering::SeqCst);return Response::json(serde_json::json!({}));}
            if request.path.ends_with("/list"){return Response::json(serde_json::json!({"items":[{"file_id":"old","name":"a.txt","type":"file","size":3}]}));}
            Response::json(serde_json::json!({"file_id":"new","upload_id":"upload","part_info_list":null}))
        }).await;
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let key = keyring_account(&id);
        let mut store = AliyunStore::new(id, "/sandbox".into());
        store.api_base = server.url.clone();
        *store.drive_id.lock().await = Some("drive".into());
        for path in [
            "sandbox",
            "sandbox/.synccloud",
            "sandbox/.synccloud/uploads",
        ] {
            store
                .dir_cache
                .lock()
                .await
                .insert(path.into(), Some("parent".into()));
        }
        store
            .save_token(&TokenState {
                access_token: "fixture".into(),
                refresh_token: "unused".into(),
                expires_at: now_ts() + 3600,
                oauth_base: server.url.clone(),
            })
            .await
            .unwrap();
        let tmp = std::env::temp_dir().join(format!("synccloud-upload-{}", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, b"new").unwrap();
        store.recovery_root = tmp.with_extension("recovery");
        assert!(store.put_file("a.txt", &tmp).await.is_err());
        assert!(!deleted.load(std::sync::atomic::Ordering::SeqCst));
        std::fs::remove_file(&tmp).unwrap();
        std::fs::remove_dir_all(&store.recovery_root).unwrap();
        crate::credential::delete_secret(&key).unwrap();
    }
}

impl AliyunStore {
    pub fn new(auth_id: String, remote_dir: String) -> Self {
        let remote_dir = remote_dir.trim_end_matches('/').to_string();
        // 严格位于用户指定的同步目录内部（{remote_dir}/.synccloud），绝不向指定目录外上传任何文件
        let meta_dir = format!("{remote_dir}/.synccloud");
        Self {
            auth_id: auth_id.clone(),
            meta_dir,
            remote_dir,
            http: api_client(),
            api_base: API.into(),
            token: tokio::sync::Mutex::new(None),
            drive_id: tokio::sync::Mutex::new(None),
            dir_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            mkdir_lock: tokio::sync::Mutex::new(()),
            recovery_root: {
                use sha2::Digest;
                crate::config::app_data_dir()
                    .join("transfers/aliyun")
                    .join(hex::encode(sha2::Sha256::digest(auth_id.as_bytes())))
            },
        }
    }

    /// Only the explicit live acceptance example can use this guarded cleanup.
    #[doc(hidden)]
    pub async fn cleanup_acceptance_root(&self) -> anyhow::Result<()> {
        let suffix = self
            .remote_dir
            .strip_prefix("/SyncCloud-acceptance-")
            .ok_or_else(|| anyhow::anyhow!("拒绝清理非验收目录"))?;
        uuid::Uuid::parse_str(suffix)?;
        if let Some(id) = self.try_resolve_dir_abs(&self.remote_dir).await? {
            self.api_json(
                "POST",
                "/adrive/v1.0/openFile/delete",
                &serde_json::json!({"drive_id":self.drive_id().await?,"file_id":id}),
            )
            .await?;
        }
        Ok(())
    }

    /// 是否已完成扫码授权（keyring 中存在令牌）。
    pub fn authorized(auth_id: &str) -> bool {
        let key = keyring_account(auth_id);
        crate::credential::load_secret(&key)
            .ok()
            .flatten()
            .is_some()
    }

    /// 元数据区内文件的绝对云端路径。
    fn meta_path(&self, rel: &str) -> String {
        format!(
            "{}/{}",
            self.meta_dir.trim_end_matches('/'),
            rel.trim_start_matches('/')
        )
    }

    /// 同步目录内的绝对云端路径。
    fn abs_path(&self, rel: &str) -> String {
        let rel = rel.trim_start_matches('/');
        if rel.is_empty() {
            return self.remote_dir.trim_start_matches('/').to_string();
        }
        format!("{}/{}", self.remote_dir.trim_start_matches('/'), rel)
    }

    async fn load_token(&self) -> Option<TokenState> {
        if let Some(t) = self.token.lock().await.as_ref() {
            return Some(t.clone());
        }
        let key = keyring_account(&self.auth_id);
        let raw = crate::credential::load_secret(&key).ok().flatten()?;
        let t: TokenState = serde_json::from_str(&raw).ok()?;
        *self.token.lock().await = Some(t.clone());
        Some(t)
    }

    async fn save_token(&self, t: &TokenState) -> anyhow::Result<()> {
        let raw = serde_json::to_string(t)?;
        let key = keyring_account(&self.auth_id);
        crate::credential::save_secret(&key, &raw)?;
        *self.token.lock().await = Some(t.clone());
        Ok(())
    }

    /// 有效 access_token；过期则用 refresh_token 刷新（refresh token 会轮换，回写 keyring）。
    /// 刷新走令牌的签发端点（直连或中转），旧版令牌无该信息时按中转处理。
    async fn refresh_access_token(&self, rejected: Option<&str>) -> anyhow::Result<String> {
        let account_lock = crate::credential::account_lock(&keyring_account(&self.auth_id));
        let _guard = account_lock.lock().await;
        *self.token.lock().await = None;
        let cur = self
            .load_token()
            .await
            .ok_or_else(|| anyhow::anyhow!("阿里云盘未授权：请先在「设置 → 云盘授权」扫码登录"))?;
        if now_ts() < cur.expires_at && rejected != Some(cur.access_token.as_str()) {
            return Ok(cur.access_token);
        }
        let base = if cur.oauth_base.is_empty() {
            oauth_host()
        } else {
            cur.oauth_base.clone()
        };
        let resp = oauth_client()
            .post(format!("{base}/oauth/access_token"))
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": cur.refresh_token,
            }))
            .send()
            .await
            .map_err(|e| e.without_url())?;
        let nt = token_from(resp, &base).await?;
        self.save_token(&nt).await?;
        Ok(nt.access_token)
    }

    /// drive_id（懒加载缓存）。
    async fn drive_id(&self) -> anyhow::Result<String> {
        if let Some(d) = self.drive_id.lock().await.as_ref() {
            return Ok(d.clone());
        }
        let v = self
            .api_json(
                "POST",
                "/adrive/v1.0/user/getDriveInfo",
                &serde_json::json!({}),
            )
            .await?;
        let resp: DriveInfoResp = serde_json::from_value(v)?;
        if resp.default_drive_id.is_empty() {
            anyhow::bail!("获取阿里云盘 drive_id 失败");
        }
        *self.drive_id.lock().await = Some(resp.default_drive_id.clone());
        Ok(resp.default_drive_id)
    }

    /// 统一 API 请求：JSON in/out，解析域错误码。
    async fn api_json(
        &self,
        method: &str,
        path: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let mut rejected = None;
        for attempt in 0..2 {
            let token = self.refresh_access_token(rejected.as_deref()).await?;
            let url = format!("{}{path}", self.api_base);
            let client = self.http.clone();
            let req = match method {
                "GET" => client.get(&url),
                _ => client.post(&url),
            };
            let resp = req
                .timeout(Duration::from_secs(30))
                .bearer_auth(&token)
                .header("Content-Type", "application/json")
                .body(serde_json::to_vec(body)?)
                .send()
                .await
                .map_err(|e| e.without_url())?;
            let status = resp.status();
            let text = resp.text().await?;
            if attempt == 0
                && (status == reqwest::StatusCode::UNAUTHORIZED
                    || text.contains("AccessTokenInvalid")
                    || text.contains("AccessTokenExpired"))
            {
                rejected = Some(token);
                continue;
            }
            if !status.is_success() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    let c = v.get("code").and_then(|x| x.as_str()).unwrap_or("");
                    let m = v.get("message").and_then(|x| x.as_str()).unwrap_or("");
                    // 目录被外部删除/移动会让 dir_cache 过期，识别 NotFound 类错误并自愈
                    let lc = format!("{} {}", c, m).to_lowercase();
                    if lc.contains("notfound") || lc.contains("not found") || lc.contains("不存在")
                    {
                        self.invalidate_dir_cache().await;
                    }
                    anyhow::bail!("阿里云盘 API {path} 失败: {m} ({c}, HTTP {status})");
                }
                anyhow::bail!(
                    "阿里云盘 API {path} 返回 HTTP {status}: {}",
                    truncate(&text, 160)
                );
            }
            if text.trim().is_empty() {
                return Ok(serde_json::Value::Null);
            }
            return Ok(serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("阿里云盘 {path} 响应解析失败: {e}"))?);
        }
        anyhow::bail!("阿里云盘身份验证失败，请重新授权")
    }

    /// 分页列出目录孩子。
    async fn list_children(&self, parent: &str) -> anyhow::Result<Vec<DriveItem>> {
        let drive_id = self.drive_id().await?;
        let mut out = Vec::new();
        let mut marker = String::new();
        loop {
            let mut body = serde_json::json!({
                "drive_id": drive_id,
                "parent_file_id": parent,
                "limit": 200,
                "fields": "*",
                "order_by": "updated_at",
                "order_direction": "DESC",
            });
            if !marker.is_empty() {
                body["marker"] = serde_json::Value::String(marker.clone());
            }
            let v = self
                .api_json("POST", "/adrive/v1.0/openFile/list", &body)
                .await?;
            let mut resp: ListResp = serde_json::from_value(v)
                .map_err(|e| anyhow::anyhow!("阿里云盘列目录响应解析失败 parent={parent}: {e}"))?;
            for item in &mut resp.items {
                if item.item_type != "folder" && item.size.is_none() {
                    let metadata = self
                        .api_json(
                            "POST",
                            "/adrive/v1.0/openFile/get",
                            &serde_json::json!({"drive_id":drive_id, "file_id":item.file_id}),
                        )
                        .await?;
                    item.size = Some(metadata.get("size").and_then(|s| s.as_u64()).ok_or_else(
                        || anyhow::anyhow!("阿里云盘文件 {} 缺少有效 size", item.name),
                    )?);
                }
            }
            out.extend(resp.items);
            if resp.next_marker.is_empty() {
                return Ok(out);
            }
            marker = resp.next_marker;
        }
    }

    async fn find_child(&self, parent: &str, name: &str) -> anyhow::Result<Option<DriveItem>> {
        Ok(self
            .list_children(parent)
            .await?
            .into_iter()
            .find(|i| i.name == name))
    }

    /// 逐级解析目录 → file_id（"root" 为远端根）。目录缺失返回 None。
    async fn try_resolve_dir(&self, rel_dir: &str) -> anyhow::Result<Option<String>> {
        self.try_resolve_dir_abs(&self.abs_path(rel_dir)).await
    }

    /// 按绝对云端路径逐级解析目录（可越出同步目录，供元数据区使用）。
    /// 逐级命中 dir_cache，未命中才发起 list 请求并把结果写回缓存。
    async fn try_resolve_dir_abs(&self, abs: &str) -> anyhow::Result<Option<String>> {
        let mut parent = "root".to_string();
        let mut prefix = String::new();
        for seg in abs.trim_matches('/').split('/') {
            if seg.is_empty() {
                continue;
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(seg);
            if let Some(hit) = self.dir_cache.lock().await.get(&prefix) {
                match hit {
                    Some(id) => {
                        parent = id.clone();
                        continue;
                    }
                    None => return Ok(None), // 已知不存在
                }
            }
            match self.find_child(&parent, seg).await? {
                Some(item) if item.item_type == "folder" => {
                    self.dir_cache
                        .lock()
                        .await
                        .insert(prefix.clone(), Some(item.file_id.clone()));
                    parent = item.file_id;
                }
                Some(_) => anyhow::bail!("远端路径 {abs} 中 {seg} 不是目录"),
                None => {
                    self.dir_cache.lock().await.insert(prefix.clone(), None);
                    return Ok(None);
                }
            }
        }
        Ok(Some(parent))
    }

    /// mkdir -p：逐级查找，缺则创建。返回最深层目录 file_id。
    async fn mkdir_p(&self, rel_dir: &str) -> anyhow::Result<String> {
        self.mkdir_p_abs(&self.abs_path(rel_dir)).await
    }

    /// 按绝对云端路径 mkdir -p（可越出同步目录，供元数据区使用）。
    async fn mkdir_p_abs(&self, abs: &str) -> anyhow::Result<String> {
        let _guard = self.mkdir_lock.lock().await;
        let mut parent = "root".to_string();
        let mut prefix = String::new();
        for seg in abs.trim_matches('/').split('/') {
            if seg.is_empty() {
                continue;
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(seg);
            let mut cached: Option<String> = None;
            if let Some(hit) = self.dir_cache.lock().await.get(&prefix) {
                if let Some(id) = hit {
                    cached = Some(id.clone());
                }
            }
            let next = match cached {
                Some(id) => id,
                None => match self.find_child(&parent, seg).await? {
                    Some(item) if item.item_type == "folder" => item.file_id,
                    Some(_) => anyhow::bail!("远端路径 {abs} 中 {seg} 已存在且不是目录"),
                    None => {
                        let drive_id = self.drive_id().await?;
                        let v = self
                            .api_json(
                                "POST",
                                "/adrive/v1.0/openFile/create",
                                &serde_json::json!({
                                    "drive_id": drive_id,
                                    "parent_file_id": parent,
                                    "name": seg,
                                    "type": "folder",
                                    "check_name_mode": "refuse",
                                }),
                            )
                            .await?;
                        let r: CreateResp = serde_json::from_value(v)?;
                        r.file_id
                    }
                },
            };
            self.dir_cache
                .lock()
                .await
                .insert(prefix.clone(), Some(next.clone()));
            parent = next;
        }
        Ok(parent)
    }

    /// 目录被外部删除等导致缓存失效时，整体清空（下一轮逐级重建）。
    async fn invalidate_dir_cache(&self) {
        self.dir_cache.lock().await.clear();
    }

    /// 解析文件路径 → (file_id, size)。不存在返回 None。
    #[allow(dead_code)]
    async fn try_resolve_file(&self, rel: &str) -> anyhow::Result<Option<(String, u64)>> {
        self.try_resolve_file_abs(&self.abs_path(rel)).await
    }

    /// 按绝对云端路径解析文件（可越出同步目录，供元数据区使用）。
    async fn try_resolve_file_abs(&self, abs: &str) -> anyhow::Result<Option<(String, u64)>> {
        let (dir, name) = match abs.rfind('/') {
            Some(i) => (&abs[..i], &abs[i + 1..]),
            None => ("", abs),
        };
        let parent = match self.try_resolve_dir_abs(dir).await? {
            Some(p) => p,
            None => return Ok(None),
        };
        match self.find_child(&parent, name).await? {
            Some(item) if item.item_type != "folder" => Ok(Some((
                item.file_id,
                item.size
                    .ok_or_else(|| anyhow::anyhow!("文件大小未知: {abs}"))?,
            ))),
            Some(_) => anyhow::bail!("远端路径 {abs} 是目录而非文件"),
            None => Ok(None),
        }
    }

    /// 读取指定绝对路径的 manifest（不存在返回 None）。
    async fn manifest_at(&self, abs: &str) -> anyhow::Result<Option<Manifest>> {
        match self.try_resolve_file_abs(abs).await? {
            None => Ok(None),
            Some((file_id, size)) => {
                let tmp = std::env::temp_dir().join(format!(
                    "synccloud-aly-manifest-{}.json",
                    uuid::Uuid::new_v4()
                ));
                self.download_by_id(&file_id, size, &tmp).await?;
                let raw = tokio::fs::read_to_string(&tmp).await;
                let _ = tokio::fs::remove_file(&tmp).await;
                Ok(Some(serde_json::from_str(&raw?)?))
            }
        }
    }

    /// 下载到本地（带大小校验重试，防 CDN 空响应）。
    async fn download_by_id(
        &self,
        file_id: &str,
        expected: u64,
        dest: &Path,
    ) -> anyhow::Result<()> {
        let v = self
            .api_json(
                "POST",
                "/adrive/v1.0/openFile/getDownloadUrl",
                &serde_json::json!({
                    "drive_id": self.drive_id().await?,
                    "file_id": file_id,
                    "expire_sec": 14400,
                }),
            )
            .await?;
        let r: DownloadUrlResp = serde_json::from_value(v)?;
        if r.url.is_empty() {
            anyhow::bail!("获取下载链接失败（url 为空）");
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            let resp = match self.http.get(&r.url).send().await {
                Ok(x) => x,
                Err(e) => {
                    last_err = Some(e.without_url().into());
                    continue;
                }
            };
            let resp = match resp.error_for_status() {
                Ok(x) => x,
                Err(e) => {
                    last_err = Some(e.without_url().into());
                    continue;
                }
            };
            use futures::StreamExt;
            use tokio::io::AsyncWriteExt;
            let mut file = match tokio::fs::File::create(dest).await {
                Ok(f) => f,
                Err(e) => return Err(e.into()),
            };
            let mut got: u64 = 0;
            let mut err: Option<anyhow::Error> = None;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(c) => {
                        crate::sync::transfer::download_limiter()
                            .consume(c.len())
                            .await;
                        got += c.len() as u64;
                        if let Err(e) = file.write_all(&c).await {
                            err = Some(e.into());
                            break;
                        }
                    }
                    Err(e) => {
                        err = Some(e.without_url().into());
                        break;
                    }
                }
            }
            match err {
                Some(e) => {
                    last_err = Some(e);
                    continue;
                }
                None => {
                    if expected == 0 || got == expected {
                        return Ok(());
                    }
                    last_err = Some(anyhow::anyhow!(
                        "下载大小不一致：期望 {expected} 字节，实得 {got}"
                    ));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("下载失败")))
    }
}

#[async_trait]
impl RemoteStore for AliyunStore {
    async fn begin_cycle(&self) -> anyhow::Result<()> {
        self.invalidate_dir_cache().await;
        self.recover_uploads(None).await
    }
    fn kind(&self) -> &'static str {
        "aliyun"
    }

    fn raw_io(&self) -> &dyn super::RawIo {
        self
    }

    async fn test(&self) -> anyhow::Result<()> {
        self.drive_id().await?;
        self.mkdir_p("").await?;
        super::probe_text_io(self).await
    }

    async fn read_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        self.manifest_at(&self.meta_path("manifest.json")).await
    }

    /// 覆盖上传 = 删除旧文件再创建新文件，file_id 每个版本都不同，
    /// 因此「解析 manifest 文件项」本身就是精准且廉价的变更戳
    /// （目录 id 有缓存时仅一次 list 请求，免去下载整个 manifest）。
    async fn manifest_stamp(&self) -> anyhow::Result<Option<String>> {
        match self
            .try_resolve_file_abs(&self.meta_path("manifest.json"))
            .await?
        {
            None => Ok(None),
            Some((file_id, size)) => Ok(Some(format!("fid{file_id}|len{size}"))),
        }
    }

    async fn write_manifest(&self, manifest: &Manifest) -> anyhow::Result<()> {
        let tmp = std::env::temp_dir().join(format!(
            "synccloud-aly-manifest-{}.json",
            uuid::Uuid::new_v4()
        ));
        tokio::fs::write(&tmp, serde_json::to_vec_pretty(manifest)?).await?;
        self.mkdir_p_abs(&self.meta_dir).await?;
        let r = self
            .put_file_abs(&self.meta_path("manifest.json"), &tmp)
            .await;
        let _ = tokio::fs::remove_file(&tmp).await;
        r
    }

    async fn read_legacy_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        let legacy = format!("/{}/manifest.json", sibling_meta(&self.remote_dir));
        if legacy == self.meta_path("manifest.json") {
            return Ok(None);
        }
        self.manifest_at(&legacy).await
    }
    async fn cleanup_legacy_meta(&self) -> anyhow::Result<()> {
        let legacy = format!("/{}/manifest.json", sibling_meta(&self.remote_dir));
        if legacy != self.meta_path("manifest.json") {
            self.delete_abs(&legacy).await?;
        }
        Ok(())
    }

    async fn put_file(&self, path: &str, src: &Path) -> anyhow::Result<()> {
        super::validate_key(path)?;
        self.put_file_abs(&self.abs_path(path), src).await
    }

    async fn get_file(&self, path: &str, dest: &Path) -> anyhow::Result<()> {
        super::validate_key(path)?;
        self.get_file_abs(&self.abs_path(path), dest).await
    }

    async fn delete_file(&self, path: &str) -> anyhow::Result<()> {
        super::validate_key(path)?;
        self.delete_abs(&self.abs_path(path)).await
    }

    async fn list_objects(&self) -> anyhow::Result<Vec<RemoteEntry>> {
        // BFS 列出 remote_dir 下所有文件（严格跳过 .synccloud）
        let root = match self.try_resolve_dir("").await? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        let mut queue = vec![(root, String::new())];
        while let Some((dir_id, rel)) = queue.pop() {
            for item in self.list_children(&dir_id).await? {
                if item.name.starts_with(".synccloud") || item.name.ends_with(".synccloud-part") {
                    continue;
                }
                let child_rel = if rel.is_empty() {
                    item.name.clone()
                } else {
                    format!("{rel}/{}", item.name)
                };
                if item.item_type == "folder" {
                    queue.push((item.file_id, child_rel));
                } else {
                    out.push(RemoteEntry {
                        key: child_rel,
                        size: item.size.ok_or_else(|| anyhow::anyhow!("文件大小未知"))?,
                    });
                }
            }
        }
        Ok(out)
    }
}

#[derive(Serialize, Deserialize)]
struct UploadRecovery {
    path: String,
    parent: String,
    name: String,
    staging: String,
    new_id: String,
    old_id: Option<String>,
    size: u64,
    uploaded: bool,
}
impl AliyunStore {
    async fn save_upload_record(&self, path: &Path, record: &UploadRecovery) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(&self.recovery_root).await?;
        let temp = path.with_extension("tmp");
        tokio::fs::write(&temp, serde_json::to_vec(record)?).await?;
        tokio::fs::rename(temp, path).await?;
        Ok(())
    }
    async fn delete_id(&self, id: &str) -> anyhow::Result<()> {
        match self
            .api_json(
                "POST",
                "/adrive/v1.0/openFile/delete",
                &serde_json::json!({"drive_id":self.drive_id().await?,"file_id":id}),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("NotFound") => Ok(()),
            Err(e) => Err(e),
        }
    }
    async fn recover_uploads(&self, only: Option<&str>) -> anyhow::Result<()> {
        if !self.recovery_root.exists() {
            return Ok(());
        }
        let mut entries = tokio::fs::read_dir(&self.recovery_root).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let record: UploadRecovery =
                serde_json::from_slice(&tokio::fs::read(entry.path()).await?)?;
            if only.is_some_and(|path| record.path != path) {
                continue;
            }
            // An account can have multiple sync roots; each root recovers its own records.
            let prefix = format!("{}/", self.remote_dir.trim_matches('/'));
            let Some(relative) = record.path.trim_start_matches('/').strip_prefix(&prefix) else {
                continue;
            };
            super::validate_key(relative)?;
            let metadata = match self
                .api_json(
                    "POST",
                    "/adrive/v1.0/openFile/get",
                    &serde_json::json!({"drive_id":self.drive_id().await?,"file_id":record.new_id}),
                )
                .await
            {
                Ok(v) => v,
                Err(e) if e.to_string().contains("NotFound") => {
                    tokio::fs::remove_file(entry.path()).await?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            let parent = metadata
                .get("parent_file_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if parent == record.staging {
                // Interrupted before publish: discard only the new staging object.
                self.delete_id(&record.new_id).await?;
            } else {
                anyhow::ensure!(
                    record.uploaded
                        && parent == record.parent
                        && metadata.get("name").and_then(|v| v.as_str())
                            == Some(record.name.as_str())
                        && metadata.get("size").and_then(|v| v.as_u64()) == Some(record.size),
                    "无法确认中断上传的位置和内容，已保留新旧版本及恢复记录"
                );
                if let Some(old) = record.old_id {
                    self.delete_id(&old).await?;
                }
            }
            tokio::fs::remove_file(entry.path()).await?;
        }
        Ok(())
    }
}

impl AliyunStore {
    async fn put_file_abs(&self, abs: &str, src: &Path) -> anyhow::Result<()> {
        let path_lock =
            crate::credential::account_lock(&format!("aliyun-upload:{}:{abs}", self.auth_id));
        let _path = path_lock.lock().await;
        self.recover_uploads(Some(abs)).await?;
        let (dir, name) = match abs.rfind('/') {
            Some(i) => (&abs[..i], &abs[i + 1..]),
            None => ("", abs),
        };
        let parent = self.mkdir_p_abs(dir).await?;
        let old = self.try_resolve_file_abs(abs).await?;
        let staging = self.mkdir_p_abs(&self.meta_path("uploads")).await?;
        let upload_name = uuid::Uuid::new_v4().to_string();
        let mut file = crate::sync::fslock::open_read_shared_async(src).await?;
        let size = file.metadata().await?.len() as usize;
        let parts = if size == 0 {
            1
        } else {
            (size + CHUNK - 1) / CHUNK
        };
        let part_numbers: Vec<u64> = (1..=parts as u64).collect();
        let v = self
            .api_json(
                "POST",
                "/adrive/v1.0/openFile/create",
                &serde_json::json!({
                    "drive_id": self.drive_id().await?,
                    "parent_file_id": staging,
                    "name": upload_name,
                    "type": "file",
                    "check_name_mode": "refuse",
                    "size": size,
                    "content_hash": "",
                    "content_hash_name": "none",
                    "proof_code": "",
                    "proof_version": "v1",
                    "part_info_list": part_numbers
                        .iter()
                        .map(|n| serde_json::json!({"part_number": n}))
                        .collect::<Vec<_>>(),
                }),
            )
            .await?;
        let r: CreateResp = serde_json::from_value(v)?;
        anyhow::ensure!(
            !r.exist && !r.file_id.is_empty(),
            "临时上传名称意外重复或文件编号缺失"
        );
        let record_path = self
            .recovery_root
            .join(format!("{}.json", uuid::Uuid::new_v4()));
        let mut record = UploadRecovery {
            path: abs.into(),
            parent: parent.clone(),
            name: name.into(),
            staging: staging.clone(),
            new_id: r.file_id.clone(),
            old_id: old.as_ref().map(|(id, _)| id.clone()),
            size: size as u64,
            uploaded: false,
        };
        self.save_upload_record(&record_path, &record).await?;
        if !r.rapid_upload && !(size == 0 && r.upload_id.is_empty()) {
            anyhow::ensure!(
                !r.upload_id.is_empty() && r.part_info_list.len() == parts,
                "阿里云盘上传响应缺少 upload_id 或分片"
            );
            let mut seen = std::collections::HashSet::new();
            // 分块 PUT 到 OSS 预签名地址
            for part in &r.part_info_list {
                anyhow::ensure!(
                    part.part_number > 0
                        && part.part_number <= parts as u64
                        && seen.insert(part.part_number)
                        && !part.upload_url.is_empty(),
                    "阿里云盘分片编号或地址无效"
                );
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                let start = (part.part_number as usize - 1) * CHUNK;
                file.seek(std::io::SeekFrom::Start(start as u64)).await?;
                let mut chunk = vec![0; size.saturating_sub(start).min(CHUNK)];
                file.read_exact(&mut chunk).await?;
                crate::sync::transfer::upload_limiter()
                    .consume(chunk.len())
                    .await;
                let resp = self
                    .http
                    .put(&part.upload_url)
                    .body(chunk)
                    .send()
                    .await
                    .map_err(|e| e.without_url())?;
                if !resp.status().is_success() {
                    let code = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    anyhow::bail!(
                        "阿里云盘分片 {} 上传失败: HTTP {code} {}",
                        part.part_number,
                        truncate(&body, 120)
                    );
                }
            }
            // complete
            self.api_json(
                "POST",
                "/adrive/v1.0/openFile/complete",
                &serde_json::json!({
                    "drive_id": self.drive_id().await?,
                    "file_id": r.file_id,
                    "upload_id": r.upload_id,
                }),
            )
            .await?;
        }
        record.uploaded = true;
        self.save_upload_record(&record_path, &record).await?;
        self.api_json(
            "POST",
            "/adrive/v1.0/openFile/move",
            &serde_json::json!({
                "drive_id":self.drive_id().await?, "file_id":r.file_id,
                "to_parent_file_id":parent, "new_name":name, "check_name_mode":"ignore"
            }),
        )
        .await?;
        if let Some((old_id, _)) = old {
            self.delete_id(&old_id).await?;
        }
        tokio::fs::remove_file(record_path).await?;
        self.invalidate_dir_cache().await;
        Ok(())
    }

    async fn get_file_abs(&self, abs: &str, dest: &Path) -> anyhow::Result<()> {
        match self.try_resolve_file_abs(abs).await? {
            Some((file_id, size)) => self.download_by_id(&file_id, size, dest).await,
            None => anyhow::bail!("阿里云盘文件不存在: {abs}"),
        }
    }

    async fn delete_abs(&self, abs: &str) -> anyhow::Result<()> {
        match self.try_resolve_file_abs(abs).await? {
            None => Ok(()), // 不存在视为成功（幂等）
            Some((file_id, _)) => {
                self.api_json(
                    "POST",
                    "/adrive/v1.0/openFile/delete",
                    &serde_json::json!({
                        "drive_id": self.drive_id().await?,
                        "file_id": file_id,
                    }),
                )
                .await?;
                Ok(())
            }
        }
    }
}

/// 锁原语：键相对元数据区根（同步目录之外），映射为远端小文件操作。
#[async_trait]
impl super::RawIo for AliyunStore {
    async fn put_text(&self, key: &str, content: &str) -> anyhow::Result<()> {
        super::validate_key(key)?;
        let tmp =
            std::env::temp_dir().join(format!("synccloud-aly-lock-{}.txt", uuid::Uuid::new_v4()));
        tokio::fs::write(&tmp, content).await?;
        let r = self.put_file_abs(&self.meta_path(key), &tmp).await;
        let _ = tokio::fs::remove_file(&tmp).await;
        r
    }

    async fn read_text(&self, key: &str) -> anyhow::Result<Option<String>> {
        super::validate_key(key)?;
        match self.try_resolve_file_abs(&self.meta_path(key)).await? {
            None => Ok(None),
            Some((file_id, size)) => {
                let tmp = std::env::temp_dir()
                    .join(format!("synccloud-aly-read-{}.txt", uuid::Uuid::new_v4()));
                let r = self.download_by_id(&file_id, size, &tmp).await;
                let out = match r {
                    Ok(()) => tokio::fs::read_to_string(&tmp)
                        .await
                        .map_err(anyhow::Error::from),
                    Err(e) => Err(e),
                };
                let _ = tokio::fs::remove_file(&tmp).await;
                out.map(Some)
            }
        }
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        super::validate_key(key)?;
        self.delete_abs(&self.meta_path(key)).await
    }

    async fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        super::validate_key(prefix.trim_end_matches('/'))?;
        let dir = self.meta_path(prefix.trim_end_matches('/'));
        let parent = match self.try_resolve_dir_abs(&dir).await? {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for item in self.list_children(&parent).await? {
            if item.item_type != "folder" {
                out.push(format!("{}/{}", prefix.trim_end_matches('/'), item.name));
            }
        }
        Ok(out)
    }
}
