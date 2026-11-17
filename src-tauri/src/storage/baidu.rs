//! 百度网盘开放平台后端（实验性）。
//! 需要用户在 https://pan.baidu.com/union/main 开放平台自建应用，填入 AppKey/SecretKey。
//! 授权：设备码流程（用户在 baidu.com/device 输入码）；令牌存系统凭据管理器。
//! 注意：个人开发者的测试应用有配额限制，正式使用需企业认证。

use std::path::Path;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

use super::{sibling_meta, RemoteEntry, RemoteStore};
use crate::sync::types::Manifest;

const OPENAPI: &str = "https://openapi.baidu.com";
const PAN: &str = "https://pan.baidu.com/rest/2.0";
const PCS: &str = "https://d.pcs.baidu.com/rest/2.0/pcs/superfile2";

pub struct BaiduStore {
    auth_id: String,
    app_key: String,
    secret_key: String,
    remote_dir: String,
    /// 元数据区（manifest/锁），严格位于用户指定的同步目录内部（{remote_dir}/.synccloud）
    meta_dir: String,
    http: reqwest::Client,
    api_base: String,
    oauth_base: String,
    upload_base: String,
    token: tokio::sync::Mutex<Option<TokenState>>,
    /// 目录列表缓存（dir → 子项）。百度同样没有按路径直达的 API，
    /// 逐级 list 是每轮同步的最大开销；任何写操作后整体清空保证一致。
    list_cache:
        tokio::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<Vec<BaiduFile>>>>,
    created_dirs: tokio::sync::Mutex<std::collections::HashSet<String>>,
    mkdir_lock: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenState {
    access_token: String,
    refresh_token: String,
    /// Unix 秒
    expires_at: i64,
}

fn keyring_account(auth_id: &str, app_key: &str) -> String {
    if !auth_id.is_empty() {
        format!("baidu:account:{auth_id}")
    } else {
        format!("baidu:{app_key}")
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct DeviceCodeResp {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    #[serde(default)]
    pub qrcode_url: String,
    #[serde(default = "default_interval")]
    pub interval: u64,
    #[serde(default = "default_expires")]
    pub expires_in: i64,
}

fn default_interval() -> u64 {
    5
}
fn default_expires() -> i64 {
    300
}

/// 百度 token 接口在等待授权/出错时只返回 error 字段，
/// 所有字段必须可选，先解析再按语义判断。
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TokenResp {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

/// 解析 token 接口响应（null 容错：字段可能缺失或为 JSON null，
/// 直接 `.json()` 反序列化会因 `invalid type: null` 整体失败）。
async fn parse_token_resp(resp: reqwest::Response) -> anyhow::Result<TokenResp> {
    let status = resp.status();
    let body = resp.text().await?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
        anyhow::anyhow!("百度令牌接口响应异常(HTTP {status}): {}", "响应字段不完整")
    })?;
    if !status.is_success() && v.get("error").is_none() {
        anyhow::bail!("百度令牌接口返回 HTTP {status}: {}", "响应字段不完整");
    }
    Ok(TokenResp {
        access_token: v
            .get("access_token")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        refresh_token: v
            .get("refresh_token")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        expires_in: v.get("expires_in").and_then(|x| x.as_i64()).unwrap_or(0),
        error: v
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        error_description: v
            .get("error_description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ListResp {
    #[serde(default)]
    list: Vec<BaiduFile>,
    errno: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct BaiduFile {
    fs_id: u64,
    #[serde(default)]
    isdir: i64,
    #[serde(default)]
    server_filename: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct FileMetasResp {
    #[serde(default)]
    list: Vec<DlinkItem>,
    errno: i64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct DlinkItem {
    dlink: String,
    #[serde(default)]
    size: u64,
}

/// precreate 响应：`return_type` 是整数（1=需上传 2=秒传），不能声明为 String。
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PrecreateResp {
    #[serde(default)]
    uploadid: String,
    #[serde(default)]
    return_type: i64,
    errno: i64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CreateResp {
    #[serde(default)]
    fs_id: u64,
    errno: i64,
}

impl BaiduStore {
    pub fn new(auth_id: String, app_key: String, secret_key: String, remote_dir: String) -> Self {
        let remote_dir = remote_dir.trim_end_matches('/').to_string();
        // 严格位于用户指定的同步目录内部（{remote_dir}/.synccloud），绝不向指定目录外上传任何文件
        let meta_dir = format!("{remote_dir}/.synccloud");
        Self {
            auth_id,
            app_key,
            secret_key,
            meta_dir,
            remote_dir,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .user_agent("pan.baidu.com")
                .build()
                .expect("reqwest client"),
            api_base: PAN.into(),
            oauth_base: OPENAPI.into(),
            upload_base: PCS.into(),
            token: tokio::sync::Mutex::new(None),
            list_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            created_dirs: tokio::sync::Mutex::new(std::collections::HashSet::new()),
            mkdir_lock: Default::default(),
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
        self.delete_abs(&self.remote_dir).await
    }

    /// 是否已完成设备码授权（keyring 中存在令牌）。
    pub fn authorized(auth_id: &str, app_key: &str) -> bool {
        crate::credential::load_secret(&keyring_account(auth_id, app_key))
            .ok()
            .flatten()
            .is_some()
    }

    /// 写操作后清空目录列表缓存。
    async fn invalidate_list_cache(&self) {
        self.list_cache.lock().await.clear();
    }

    fn abs_path(&self, rel: &str) -> String {
        let rel = rel.trim_start_matches('/');
        if rel.is_empty() {
            return self.remote_dir.clone();
        }
        format!("{}/{}", self.remote_dir, rel)
    }

    /// 元数据区内文件的绝对云端路径。
    fn meta_path(&self, rel: &str) -> String {
        format!(
            "{}/{}",
            self.meta_dir.trim_end_matches('/'),
            rel.trim_start_matches('/')
        )
    }

    async fn load_token(&self) -> Option<TokenState> {
        if let Some(t) = self.token.lock().await.as_ref() {
            return Some(t.clone());
        }
        let raw = crate::credential::load_secret(&keyring_account(&self.auth_id, &self.app_key))
            .ok()
            .flatten()?;
        let t: TokenState = serde_json::from_str(&raw).ok()?;
        *self.token.lock().await = Some(t.clone());
        Some(t)
    }

    async fn save_token(&self, t: &TokenState) -> anyhow::Result<()> {
        let raw = serde_json::to_string(t)?;
        crate::credential::save_secret(&keyring_account(&self.auth_id, &self.app_key), &raw)?;
        *self.token.lock().await = Some(t.clone());
        Ok(())
    }

    /// 启动设备码授权，返回给 UI 展示的验证信息。
    /// 百度在 client_id 无效时也返回 200/401 + error JSON，这里透出官方错误描述。
    pub async fn start_device_login(&self) -> anyhow::Result<DeviceCodeResp> {
        let resp = self
            .http
            .get(format!("{}/oauth/2.0/device/code", self.oauth_base))
            .timeout(Duration::from_secs(15))
            .query(&[
                ("response_type", "device_code"),
                ("client_id", self.app_key.as_str()),
                ("scope", "basic,netdisk"),
            ])
            .send()
            .await
            .map_err(|e| e.without_url())?;
        let status = resp.status();
        let body = resp.text().await?;
        let v: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
            anyhow::anyhow!("百度授权接口响应异常(HTTP {status}): {}", "响应字段不完整")
        })?;
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            let desc = v
                .get("error_description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            anyhow::bail!("百度授权发起失败: {desc} ({err})，请检查 AppKey/SecretKey");
        }
        if !status.is_success() {
            anyhow::bail!("百度授权接口返回 HTTP {status}: {}", "响应字段不完整");
        }
        Ok(serde_json::from_value(v)?)
    }

    /// 轮询设备码授权结果（用户在 verification_url 输入 user_code 后完成）。
    /// null 容错：等待授权时响应只含 error 字段，其余字段可能缺失或为 null。
    pub async fn poll_device_login(&self, device_code: &str) -> anyhow::Result<bool> {
        let lock = crate::credential::account_lock(&keyring_account(&self.auth_id, &self.app_key));
        let _account = lock.lock().await;
        self.poll_device_login_unlocked(device_code).await
    }
    pub(crate) async fn poll_device_login_unlocked(
        &self,
        device_code: &str,
    ) -> anyhow::Result<bool> {
        let resp = self
            .http
            .get(format!("{}/oauth/2.0/token", self.oauth_base))
            .timeout(Duration::from_secs(15))
            .query(&[
                ("grant_type", "device_token"),
                ("code", device_code),
                ("client_id", self.app_key.as_str()),
                ("client_secret", self.secret_key.as_str()),
            ])
            .send()
            .await
            .map_err(|e| e.without_url())?;
        let t = parse_token_resp(resp).await?;
        if t.error.is_empty() && !t.access_token.is_empty() {
            let nt = TokenState {
                access_token: t.access_token,
                refresh_token: t.refresh_token,
                expires_at: now_ts() + t.expires_in - 60,
            };
            self.save_token(&nt).await?;
            Ok(true)
        } else if t.error == "authorization_pending" || t.error == "slow_down" {
            Ok(false)
        } else if t.error == "expired_token" {
            anyhow::bail!("用户码已过期，请重新发起授权")
        } else {
            anyhow::bail!("百度授权失败: {} ({})", t.error_description, t.error)
        }
    }

    async fn access_token(&self) -> anyhow::Result<String> {
        self.refresh_access_token(None).await
    }
    async fn refresh_access_token(&self, rejected: Option<&str>) -> anyhow::Result<String> {
        let lock = crate::credential::account_lock(&keyring_account(&self.auth_id, &self.app_key));
        let _guard = lock.lock().await;
        *self.token.lock().await = None;
        let t = self
            .load_token()
            .await
            .ok_or_else(|| anyhow::anyhow!("百度网盘未授权：请先在设置中完成设备登录"))?;
        if now_ts() < t.expires_at && rejected != Some(t.access_token.as_str()) {
            return Ok(t.access_token);
        }
        // 刷新（null 容错解析）
        let resp = self
            .http
            .get(format!("{}/oauth/2.0/token", self.oauth_base))
            .timeout(Duration::from_secs(15))
            .query(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", t.refresh_token.as_str()),
                ("client_id", self.app_key.as_str()),
                ("client_secret", self.secret_key.as_str()),
            ])
            .send()
            .await
            .map_err(|e| e.without_url())?;
        let resp = parse_token_resp(resp).await?;
        if resp.error.is_empty() && !resp.access_token.is_empty() {
            let nt = TokenState {
                access_token: resp.access_token,
                refresh_token: resp.refresh_token,
                expires_at: now_ts() + resp.expires_in - 60,
            };
            self.save_token(&nt).await?;
            Ok(nt.access_token)
        } else {
            anyhow::bail!("百度令牌刷新失败，请重新授权: {}", resp.error_description)
        }
    }

    async fn control_json<T: serde::de::DeserializeOwned>(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> anyhow::Result<T> {
        let mut request = builder.timeout(Duration::from_secs(30)).build()?;
        for attempt in 0..2 {
            let retry = request
                .try_clone()
                .ok_or_else(|| anyhow::anyhow!("控制请求不可重试"))?;
            let response = self
                .http
                .execute(retry)
                .await
                .map_err(|e| e.without_url())?;
            let status = response.status();
            let value: serde_json::Value = response.json().await.map_err(|e| e.without_url())?;
            let code = value
                .get("errno")
                .or_else(|| value.get("error_code"))
                .and_then(|v| v.as_i64());
            if attempt == 0
                && (status == reqwest::StatusCode::UNAUTHORIZED
                    || matches!(code, Some(110 | 111 | -6)))
            {
                let previous = request
                    .url()
                    .query_pairs()
                    .find(|(k, _)| k == "access_token")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                let token = self.refresh_access_token(Some(&previous)).await?;
                let pairs: Vec<(String, String)> = request
                    .url()
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                request
                    .url_mut()
                    .query_pairs_mut()
                    .clear()
                    .extend_pairs(pairs.iter().map(|(k, v)| {
                        (
                            k.as_str(),
                            if k == "access_token" {
                                token.as_str()
                            } else {
                                v.as_str()
                            },
                        )
                    }));
                continue;
            }
            anyhow::ensure!(status.is_success(), "百度控制请求失败 HTTP {status}");
            return Ok(serde_json::from_value(value)?);
        }
        anyhow::bail!("百度认证失效，请重新授权")
    }

    /// 列目录（分页聚合，带缓存）。
    async fn list_dir(&self, dir: &str) -> anyhow::Result<Vec<BaiduFile>> {
        if let Some(hit) = self.list_cache.lock().await.get(dir) {
            return Ok(hit.as_ref().clone());
        }
        let token = self.access_token().await?;
        let mut out = Vec::new();
        let mut start = 0u32;
        loop {
            let resp: ListResp = self
                .control_json(
                    self.http
                        .get(format!("{}/xpan/file", self.api_base))
                        .query(&[
                            ("method", "list"),
                            ("access_token", token.as_str()),
                            ("dir", dir),
                            ("start", &start.to_string()),
                            ("limit", "1000"),
                            ("web", "1"),
                        ]),
                )
                .await?;
            if resp.errno == -9 {
                return Ok(Vec::new());
            }
            if resp.errno != 0 {
                anyhow::bail!(
                    "百度网盘列目录失败 errno={}（目录不存在或令牌无效）",
                    resp.errno
                );
            }
            let n = resp.list.len() as u32;
            out.extend(resp.list);
            if n < 1000 {
                break;
            }
            start += n;
        }
        self.list_cache
            .lock()
            .await
            .insert(dir.to_string(), std::sync::Arc::new(out.clone()));
        Ok(out)
    }

    /// 逐级查找文件，返回 fs_id。不存在返回 None。
    #[allow(dead_code)]
    async fn lookup(&self, rel: &str) -> anyhow::Result<Option<u64>> {
        self.lookup_abs(&self.abs_path(rel)).await
    }

    /// 按绝对云端路径逐级查找文件（可越出同步目录，供元数据区使用）。
    async fn lookup_abs(&self, abs: &str) -> anyhow::Result<Option<u64>> {
        let mut cur = String::new();
        let mut fs_id: Option<u64> = None;
        for seg in abs.trim_start_matches('/').split('/') {
            if cur.is_empty() {
                cur.push('/');
            } else {
                cur.push('/');
            }
            cur.push_str(seg);
            let items = self.list_dir(&parent_of(&cur)).await?;
            match items.into_iter().find(|f| {
                let full = format!(
                    "{}/{}",
                    parent_of(&cur).trim_end_matches('/'),
                    f.server_filename
                );
                full.trim_start_matches('/') == cur.trim_start_matches('/')
            }) {
                Some(f) => fs_id = Some(f.fs_id),
                None => return Ok(None),
            }
        }
        Ok(fs_id)
    }

    /// 逐级创建目录（已存在返回成功）。
    async fn mkdir_p(&self, rel_dir: &str) -> anyhow::Result<()> {
        self.mkdir_p_abs(&self.abs_path(rel_dir)).await
    }

    /// 按绝对云端路径创建目录（带内存缓存加速，避免重复请求触发网盘限流）。
    async fn mkdir_p_abs(&self, abs: &str) -> anyhow::Result<()> {
        let _mkdir = self.mkdir_lock.lock().await;
        let abs = abs.trim_end_matches('/');
        if abs.is_empty() || abs == "/" {
            return Ok(());
        }
        {
            let guard = self.created_dirs.lock().await;
            if guard.contains(abs) {
                return Ok(());
            }
        }
        let token = self.access_token().await?;
        let resp: CreateResp = self
            .control_json(
                self.http
                    .post(format!("{}/xpan/file", self.api_base))
                    .query(&[("method", "create"), ("access_token", token.as_str())])
                    .form(&[("path", abs), ("isdir", "1"), ("size", "0"), ("rtype", "0")]),
            )
            .await?;
        if resp.errno == 0 || resp.errno == -8 {
            let mut guard = self.created_dirs.lock().await;
            guard.insert(abs.to_string());
            self.invalidate_list_cache().await;
            return Ok(());
        }
        anyhow::bail!("创建目录 {abs} 失败 errno={}", resp.errno);
    }

    /// 读取指定绝对路径的 manifest（不存在返回 None）。
    async fn manifest_at(&self, abs: &str) -> anyhow::Result<Option<Manifest>> {
        match self.lookup_abs(abs).await? {
            None => Ok(None),
            Some(fs_id) => {
                let token = self.access_token().await?;
                let metas: FileMetasResp = self
                    .control_json(
                        self.http
                            .get(format!("{}/xpan/multimedia", self.api_base))
                            .query(&[
                                ("method", "filemetas"),
                                ("access_token", token.as_str()),
                                ("fsids", format!("[{fs_id}]").as_str()),
                                ("dlink", "1"),
                            ]),
                    )
                    .await?;
                let dlink = metas
                    .list
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("获取下载链接失败"))?
                    .dlink
                    .clone();
                let bytes = self
                    .http
                    .get(format!("{dlink}&access_token={token}"))
                    .header("User-Agent", "pan.baidu.com")
                    .send()
                    .await
                    .map_err(|e| e.without_url())?
                    .error_for_status()
                    .map_err(|e| e.without_url())?
                    .bytes()
                    .await?;
                Ok(Some(serde_json::from_slice(&bytes)?))
            }
        }
    }
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

fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].to_string(),
    }
}

#[async_trait]
impl RemoteStore for BaiduStore {
    async fn begin_cycle(&self) -> anyhow::Result<()> {
        self.invalidate_list_cache().await;
        self.created_dirs.lock().await.clear();
        Ok(())
    }
    fn kind(&self) -> &'static str {
        "baidu"
    }

    fn raw_io(&self) -> &dyn super::RawIo {
        self
    }

    async fn test(&self) -> anyhow::Result<()> {
        self.access_token().await?;
        self.mkdir_p("").await?;
        super::probe_text_io(self).await
    }

    async fn read_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        self.manifest_at(&self.meta_path("manifest.json")).await
    }

    async fn write_manifest(&self, manifest: &Manifest) -> anyhow::Result<()> {
        let tmp = std::env::temp_dir().join(format!(
            "synccloud-baidu-manifest-{}.json",
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

    /// 廉价变更戳：manifest 的 fs_id + filemetas 里的修改时间/大小
    /// （rtype=3 覆盖上传会产生新文件版本，server_mtime 随之变化）。
    async fn manifest_stamp(&self) -> anyhow::Result<Option<String>> {
        let fs_id = match self.lookup_abs(&self.meta_path("manifest.json")).await? {
            None => return Ok(None),
            Some(id) => id,
        };
        let token = self.access_token().await?;
        let v: serde_json::Value = self
            .control_json(
                self.http
                    .get(format!("{}/xpan/multimedia", self.api_base))
                    .query(&[
                        ("method", "filemetas"),
                        ("access_token", token.as_str()),
                        ("fsids", format!("[{fs_id}]").as_str()),
                        ("dlink", "0"),
                    ]),
            )
            .await?;
        anyhow::ensure!(
            v.get("errno").and_then(|v| v.as_i64()) == Some(0),
            "百度清单元数据请求失败"
        );
        let item = v
            .get("list")
            .and_then(|l| l.get(0))
            .ok_or_else(|| anyhow::anyhow!("百度清单元数据缺失"))?;
        if let Some(md5) = item
            .get("md5")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
        {
            return Ok(Some(format!("fs{fs_id}|md5{md5}")));
        }
        Ok(self
            .read_manifest()
            .await?
            .as_ref()
            .map(super::manifest_stamp_of))
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
        if let Some(parent) = Path::new(path).parent() {
            let p = parent.to_string_lossy().replace('\\', "/");
            let p = p.trim_start_matches('/').to_string();
            if !p.is_empty() {
                self.mkdir_p(&p).await?;
            }
        }
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
        // BFS 递归列出 remote_dir 下所有文件（跳过 .synccloud）
        let mut out = Vec::new();
        let mut queue = vec![self.remote_dir.clone()];
        while let Some(dir) = queue.pop() {
            for item in self.list_dir(&dir).await? {
                if item.server_filename.starts_with(".synccloud")
                    || item.server_filename.ends_with(".synccloud-part")
                {
                    continue;
                }
                let full = format!("{}/{}", dir.trim_end_matches('/'), item.server_filename);
                if item.isdir != 0 {
                    queue.push(full);
                } else {
                    out.push(RemoteEntry {
                        key: full
                            .strip_prefix(&format!("{}/", self.remote_dir.trim_end_matches('/')))
                            .unwrap_or(&full)
                            .to_string(),
                        size: item.size,
                    });
                }
            }
        }
        Ok(out)
    }
}

impl BaiduStore {
    /// 上传到绝对云端路径（precreate → 分片 → 合并；可越出同步目录）。
    async fn put_file_abs(&self, abs: &str, src: &Path) -> anyhow::Result<()> {
        self.invalidate_list_cache().await;
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut file = crate::sync::fslock::open_read_shared_async(src).await?;
        let size = file.metadata().await?.len() as usize;
        const BLOCK: usize = 4 * 1024 * 1024;
        let mut block_list = Vec::new();
        let mut buffer = vec![0; BLOCK];
        let parts = ((size + BLOCK - 1) / BLOCK).max(1);
        for i in 0..parts {
            let len = size.saturating_sub(i * BLOCK).min(BLOCK);
            file.read_exact(&mut buffer[..len]).await?;
            block_list.push(md5_hex(&buffer[..len]));
        }
        file.seek(std::io::SeekFrom::Start(0)).await?;
        let block_json = serde_json::to_string(&block_list)?;
        let token = self.access_token().await?;

        // 1. precreate
        let rid = chrono::Utc::now().timestamp_millis().to_string();
        let pre: PrecreateResp = self
            .control_json(
                self.http
                    .post(format!("{}/xpan/file", self.api_base))
                    .query(&[("method", "precreate"), ("access_token", token.as_str())])
                    .form(&[
                        ("path", abs),
                        ("size", &size.to_string()),
                        ("isdir", "0"),
                        ("autoinit", "1"),
                        ("rtype", "3"), // 覆盖同名
                        ("block_list", block_json.as_str()),
                        ("rid", rid.as_str()),
                    ]),
            )
            .await?;
        if pre.errno != 0 {
            anyhow::bail!("百度预创建失败 errno={}（检查路径与配额）", pre.errno);
        }
        // 秒传：远端已有同内容文件，precreate 已直接完成，跳过分片与合并
        if pre.return_type == 2 {
            return Ok(());
        }

        anyhow::ensure!(!pre.uploadid.is_empty(), "百度预创建响应缺少 uploadid");
        // 2. superfile2 分块上传（partseq 从 0 开始计数）
        let total_parts = block_list.len().max(1);
        for i in 0..total_parts {
            let len = size.saturating_sub(i * BLOCK).min(BLOCK);
            file.read_exact(&mut buffer[..len]).await?;
            let chunk = &buffer[..len];
            crate::sync::transfer::upload_limiter()
                .consume(chunk.len())
                .await;
            let resp = self
                .http
                .put(&self.upload_base)
                .query(&[
                    ("method", "upload"),
                    ("access_token", token.as_str()),
                    ("type", "tmpfile"),
                    ("path", abs),
                    ("uploadid", pre.uploadid.as_str()),
                    ("partseq", &i.to_string()),
                ])
                .header("Content-Type", "application/octet-stream")
                .body(chunk.to_vec())
                .send()
                .await
                .map_err(|e| e.without_url())?;
            if !resp.status().is_success() {
                let code = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!(
                    "百度分片 {} 上传失败: HTTP {code} {}",
                    i,
                    truncate(&body, 120)
                );
            }
            let part: serde_json::Value = resp.json().await.map_err(|e| e.without_url())?;
            anyhow::ensure!(
                part.get("error_code").and_then(|v| v.as_i64()).unwrap_or(0) == 0
                    && part.get("errno").and_then(|v| v.as_i64()).unwrap_or(0) == 0,
                "百度分片 {i} 上传被拒绝"
            );
            anyhow::ensure!(
                part.get("md5")
                    .and_then(|v| v.as_str())
                    .is_some_and(|v| v.eq_ignore_ascii_case(&block_list[i])),
                "百度分片 {i} 响应缺少或不匹配内容 MD5"
            );
        }
        let _ = total_parts;

        // 3. create 合并
        let done: CreateResp = self
            .control_json(
                self.http
                    .post(format!("{}/xpan/file", self.api_base))
                    .query(&[("method", "create"), ("access_token", token.as_str())])
                    .form(&[
                        ("path", abs),
                        ("size", &size.to_string()),
                        ("isdir", "0"),
                        ("rtype", "3"),
                        ("block_list", block_json.as_str()),
                        ("uploadid", pre.uploadid.as_str()),
                    ]),
            )
            .await?;
        anyhow::ensure!(
            done.fs_id > 0 || done.errno != 0,
            "百度创建响应缺少文件编号"
        );
        if done.errno != 0 {
            anyhow::bail!("百度创建文件失败 errno={}", done.errno);
        }
        Ok(())
    }

    /// 按绝对云端路径下载（可越出同步目录，供元数据区使用）。
    async fn get_file_abs(&self, abs: &str, dest: &Path) -> anyhow::Result<()> {
        let fs_id = self
            .lookup_abs(abs)
            .await?
            .ok_or_else(|| anyhow::anyhow!("百度网盘文件不存在: {abs}"))?;
        let token = self.access_token().await?;
        let metas: FileMetasResp = self
            .control_json(
                self.http
                    .get(format!("{}/xpan/multimedia", self.api_base))
                    .query(&[
                        ("method", "filemetas"),
                        ("access_token", token.as_str()),
                        ("fsids", format!("[{fs_id}]").as_str()),
                        ("dlink", "1"),
                    ]),
            )
            .await?;
        let item = metas
            .list
            .first()
            .ok_or_else(|| anyhow::anyhow!("获取下载链接失败"))?;
        let dlink = item.dlink.clone();
        if dlink.is_empty() {
            anyhow::bail!("获取下载链接失败（dlink 为空，稍后重试）");
        }
        let expected = item.size;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // 百度 dlink 走 CDN：刚上传的文件偶发返回空/过期响应，
        // 按 filemetas 给出的 size 校验并重试。
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            let resp = match self
                .http
                .get(format!("{dlink}&access_token={token}"))
                .header("User-Agent", "pan.baidu.com")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(e.without_url().into());
                    continue;
                }
            };
            let resp = match resp.error_for_status() {
                Ok(r) => r,
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
            let mut stream_err: Option<anyhow::Error> = None;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(c) => {
                        crate::sync::transfer::download_limiter()
                            .consume(c.len())
                            .await;
                        got += c.len() as u64;
                        if let Err(e) = file.write_all(&c).await {
                            stream_err = Some(e.into());
                            break;
                        }
                    }
                    Err(e) => {
                        stream_err = Some(e.without_url().into());
                        break;
                    }
                }
            }
            match stream_err {
                Some(e) => {
                    last_err = Some(e);
                    continue;
                }
                None => {
                    if expected == 0 || got == expected {
                        return Ok(());
                    }
                    last_err = Some(anyhow::anyhow!(
                        "下载大小不一致：期望 {expected} 字节，实得 {got}（CDN 未就绪）"
                    ));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("下载失败")))
    }

    /// 按绝对云端路径删除（可越出同步目录，供元数据区使用；目录亦可递归删）。
    async fn delete_abs(&self, abs: &str) -> anyhow::Result<()> {
        self.invalidate_list_cache().await;
        let filelist = serde_json::to_string(&[abs])?;
        let token = self.access_token().await?;
        // 注意：新版 file_manager?method=delete 对个人测试应用恒返回 31023 param error，
        // 旧版 filemanager 接口实测可用（2026-10）。
        let resp = self
            .http
            .post(format!("{}/xpan/file", self.api_base))
            .query(&[
                ("method", "filemanager"),
                ("opera", "delete"),
                ("access_token", token.as_str()),
            ])
            .form(&[("async", "0"), ("filelist", filelist.as_str())])
            .send()
            .await
            .map_err(|e| e.without_url())?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("百度删除文件失败: HTTP {status}: {}", truncate(&body, 120));
        }
        let v: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => return Err(anyhow::anyhow!("百度删除响应不是有效 JSON: {e}")),
        };
        let errno = v
            .get("errno")
            .and_then(|x| x.as_i64())
            .ok_or_else(|| anyhow::anyhow!("百度删除响应缺少 errno"))?;
        if errno != 0 {
            // 文件不存在视为成功（幂等）
            if errno == -9 {
                return Ok(());
            }
            anyhow::bail!("百度删除文件失败 errno={errno}");
        }
        Ok(())
    }
}

/// 锁原语：键相对元数据区根（同步目录之外），映射为远端小文件操作。
#[async_trait]
impl super::RawIo for BaiduStore {
    async fn put_text(&self, key: &str, content: &str) -> anyhow::Result<()> {
        super::validate_key(key)?;
        let tmp =
            std::env::temp_dir().join(format!("synccloud-baidu-lock-{}.txt", uuid::Uuid::new_v4()));
        tokio::fs::write(&tmp, content).await?;
        let p = self.meta_path(key);
        self.mkdir_p_abs(&parent_of(&p)).await?;
        let r = self.put_file_abs(&p, &tmp).await;
        let _ = tokio::fs::remove_file(&tmp).await;
        r
    }

    async fn read_text(&self, key: &str) -> anyhow::Result<Option<String>> {
        super::validate_key(key)?;
        let abs = self.meta_path(key);
        if self.lookup_abs(&abs).await?.is_none() {
            return Ok(None);
        }
        let tmp =
            std::env::temp_dir().join(format!("synccloud-baidu-read-{}", uuid::Uuid::new_v4()));
        self.get_file_abs(&abs, &tmp).await?;
        let text = tokio::fs::read_to_string(&tmp).await;
        let _ = tokio::fs::remove_file(tmp).await;
        Ok(Some(text?))
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        super::validate_key(key)?;
        self.delete_abs(&self.meta_path(key)).await
    }

    async fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        super::validate_key(prefix.trim_end_matches('/'))?;
        let dir = self.meta_path(prefix.trim_end_matches('/'));
        let mut out = Vec::new();
        for item in self.list_dir(&dir).await? {
            if item.isdir == 0 {
                out.push(format!(
                    "{}/{}",
                    prefix.trim_end_matches('/'),
                    item.server_filename
                ));
            }
        }
        Ok(out)
    }
}

fn md5_hex(data: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(data);
    hex::encode(h.finalize())
}

#[cfg(test)]
mod reliability_tests {
    use super::*;
    #[tokio::test]
    async fn failed_part_response_never_merges_file() {
        use crate::storage::test_http::{serve, Response};
        let merged = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = merged.clone();
        let server = serve(move |request| {
            if request.path.starts_with("/upload") {
                return Response::json(
                    serde_json::json!({"error_code":31066,"error_msg":"denied"}),
                );
            }
            if request.path.contains("method=precreate") {
                return Response::json(
                    serde_json::json!({"errno":0,"uploadid":"upload","return_type":1}),
                );
            }
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Response::json(serde_json::json!({"errno":0,"fs_id":1}))
        })
        .await;
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let mut store = BaiduStore::new(
            id.clone(),
            "fixture".into(),
            "fixture".into(),
            "/sandbox".into(),
        );
        store.api_base = server.url.clone();
        store.upload_base = format!("{}/upload", server.url);
        store.created_dirs.lock().await.insert("/sandbox".into());
        store
            .save_token(&TokenState {
                access_token: "fixture".into(),
                refresh_token: "unused".into(),
                expires_at: now_ts() + 3600,
            })
            .await
            .unwrap();
        let tmp = std::env::temp_dir().join(format!("synccloud-part-{}", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, b"new").unwrap();
        let result = store.put_file_abs("/sandbox/file", &tmp).await;
        std::fs::remove_file(tmp).unwrap();
        crate::credential::delete_secret(&keyring_account(&id, "fixture")).unwrap();
        assert!(
            result.is_err(),
            "HTTP 200 business error must fail the upload"
        );
        assert!(
            !merged.load(std::sync::atomic::Ordering::SeqCst),
            "must not merge failed parts"
        );
    }
    #[tokio::test]
    async fn concurrent_auth_rejections_share_one_refresh() {
        use crate::storage::test_http::{serve, Response};
        let refreshes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = refreshes.clone();
        let server=serve(move|request|{
            if request.path.starts_with("/oauth/") {count.fetch_add(1,std::sync::atomic::Ordering::SeqCst);return Response::json(serde_json::json!({"access_token":"new","refresh_token":"rotated","expires_in":3600}));}
            if request.path.contains("access_token=new"){Response::json(serde_json::json!({"errno":0,"list":[]}))}
            else{Response::json(serde_json::json!({"errno":111}))}
        }).await;
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let mut store = BaiduStore::new(
            id.clone(),
            "fixture".into(),
            "fixture".into(),
            "/sandbox".into(),
        );
        store.oauth_base = server.url.clone();
        store
            .save_token(&TokenState {
                access_token: "old".into(),
                refresh_token: "fixture".into(),
                expires_at: now_ts() + 3600,
            })
            .await
            .unwrap();
        let request = || {
            store
                .http
                .get(format!("{}/xpan/file?access_token=old", server.url))
        };
        let (a, b) = tokio::join!(
            store.control_json::<ListResp>(request()),
            store.control_json::<ListResp>(request())
        );
        assert_eq!(a.unwrap().errno, 0);
        assert_eq!(b.unwrap().errno, 0);
        assert_eq!(refreshes.load(std::sync::atomic::Ordering::SeqCst), 1);
        crate::credential::delete_secret(&keyring_account(&id, "fixture")).unwrap();
    }
    #[tokio::test]
    async fn missing_directory_is_empty_but_permission_error_is_not() {
        use crate::storage::test_http::{serve, Response};
        let server = serve(move |request| {
            Response::json(
                serde_json::json!({"errno":if request.path.contains("denied"){31066}else{-9}}),
            )
        })
        .await;
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let mut store = BaiduStore::new(
            id.clone(),
            "fixture".into(),
            "fixture".into(),
            "/sandbox".into(),
        );
        store.api_base = server.url.clone();
        store
            .save_token(&TokenState {
                access_token: "fixture".into(),
                refresh_token: "fixture".into(),
                expires_at: now_ts() + 3600,
            })
            .await
            .unwrap();
        assert!(store.list_dir("/missing").await.unwrap().is_empty());
        assert!(store.list_dir("/denied").await.is_err());
        crate::credential::delete_secret(&keyring_account(&id, "fixture")).unwrap();
    }
}
