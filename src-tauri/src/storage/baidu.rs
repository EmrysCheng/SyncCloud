//! 百度网盘开放平台后端（实验性）。
//! 需要用户在 https://pan.baidu.com/union/main 开放平台自建应用，填入 AppKey/SecretKey。
//! 授权：设备码流程（用户在 baidu.com/device 输入码）；令牌存系统凭据管理器。
//! 注意：个人开发者的测试应用有配额限制，正式使用需企业认证。

use std::path::Path;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use md5::{Digest, Md5};

use super::{RemoteEntry, RemoteStore, sibling_meta};
use crate::sync::types::Manifest;

const OPENAPI: &str = "https://openapi.baidu.com";
const PAN: &str = "https://pan.baidu.com/rest/2.0";
const PCS: &str = "https://d.pcs.baidu.com/rest/2.0/pcs/superfile2";

pub struct BaiduStore {
    auth_id: String,
    app_key: String,
    secret_key: String,
    remote_dir: String,
    /// 元数据区（manifest/锁），位于同步目录之外的兄弟目录
    meta_dir: String,
    http: reqwest::Client,
    token: tokio::sync::Mutex<Option<TokenState>>,
    /// 目录列表缓存（dir → 子项）。百度同样没有按路径直达的 API，
    /// 逐级 list 是每轮同步的最大开销；任何写操作后整体清空保证一致。
    list_cache: tokio::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<Vec<BaiduFile>>>>,
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
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|_| anyhow::anyhow!("百度令牌接口响应异常(HTTP {status}): {}", truncate(&body, 200)))?;
    if !status.is_success() && v.get("error").is_none() {
        anyhow::bail!("百度令牌接口返回 HTTP {status}: {}", truncate(&body, 200));
    }
    Ok(TokenResp {
        access_token: v.get("access_token").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        refresh_token: v.get("refresh_token").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        expires_in: v.get("expires_in").and_then(|x| x.as_i64()).unwrap_or(0),
        error: v.get("error").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        error_description: v.get("error_description").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    })
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ListResp {
    #[serde(default)]
    list: Vec<BaiduFile>,
    #[serde(default)]
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
    #[serde(default)]
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
    #[serde(default)]
    errno: i64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CreateResp {
    #[serde(default)]
    fs_id: u64,
    #[serde(default)]
    errno: i64,
}

impl BaiduStore {
    pub fn new(auth_id: String, app_key: String, secret_key: String, remote_dir: String) -> Self {
        let remote_dir = remote_dir.trim_end_matches('/').to_string();
        // "/Sync" → "/.synccloud/Sync"；根目录同步时退化为 "/.synccloud"
        let meta_dir = format!("/{}", sibling_meta(&remote_dir));
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
            token: tokio::sync::Mutex::new(None),
            list_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 是否已完成设备码授权（keyring 中存在令牌）。
    pub fn authorized(auth_id: &str, app_key: &str) -> bool {
        crate::credential::load_secret(&keyring_account(auth_id, app_key)).ok().flatten().is_some()
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
        format!("{}/{}", self.meta_dir.trim_end_matches('/'), rel.trim_start_matches('/'))
    }

    async fn load_token(&self) -> Option<TokenState> {
        if let Some(t) = self.token.lock().await.as_ref() {
            return Some(t.clone());
        }
        let raw = crate::credential::load_secret(&keyring_account(&self.auth_id, &self.app_key)).ok().flatten()?;
        let t: TokenState = serde_json::from_str(&raw).ok()?;
        *self.token.lock().await = Some(t.clone());
        Some(t)
    }

    async fn save_token(&self, t: &TokenState) {
        *self.token.lock().await = Some(t.clone());
        let raw = serde_json::to_string(t).unwrap_or_default();
        let _ = crate::credential::save_secret(&keyring_account(&self.auth_id, &self.app_key), &raw);
    }

    /// 启动设备码授权，返回给 UI 展示的验证信息。
    /// 百度在 client_id 无效时也返回 200/401 + error JSON，这里透出官方错误描述。
    pub async fn start_device_login(&self) -> anyhow::Result<DeviceCodeResp> {
        let resp = self
            .http
            .get(format!("{OPENAPI}/oauth/2.0/device/code"))
            .query(&[
                ("response_type", "device_code"),
                ("client_id", self.app_key.as_str()),
                ("scope", "basic,netdisk"),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|_| anyhow::anyhow!("百度授权接口响应异常(HTTP {status}): {}", truncate(&body, 200)))?;
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            let desc = v.get("error_description").and_then(|d| d.as_str()).unwrap_or("");
            anyhow::bail!("百度授权发起失败: {desc} ({err})，请检查 AppKey/SecretKey");
        }
        if !status.is_success() {
            anyhow::bail!("百度授权接口返回 HTTP {status}: {}", truncate(&body, 200));
        }
        Ok(serde_json::from_value(v)?)
    }

    /// 轮询设备码授权结果（用户在 verification_url 输入 user_code 后完成）。
    /// null 容错：等待授权时响应只含 error 字段，其余字段可能缺失或为 null。
    pub async fn poll_device_login(&self, device_code: &str) -> anyhow::Result<bool> {
        let resp = self
            .http
            .get(format!("{OPENAPI}/oauth/2.0/token"))
            .query(&[
                ("grant_type", "device_token"),
                ("code", device_code),
                ("client_id", self.app_key.as_str()),
                ("client_secret", self.secret_key.as_str()),
            ])
            .send()
            .await?;
        let t = parse_token_resp(resp).await?;
        if t.error.is_empty() && !t.access_token.is_empty() {
            let nt = TokenState {
                access_token: t.access_token,
                refresh_token: t.refresh_token,
                expires_at: now_ts() + t.expires_in - 60,
            };
            self.save_token(&nt).await;
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
        let t = self
            .load_token()
            .await
            .ok_or_else(|| anyhow::anyhow!("百度网盘未授权：请先在设置中完成设备登录"))?;
        if now_ts() < t.expires_at {
            return Ok(t.access_token);
        }
        // 刷新（null 容错解析）
        let resp = self
            .http
            .get(format!("{OPENAPI}/oauth/2.0/token"))
            .query(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", t.refresh_token.as_str()),
                ("client_id", self.app_key.as_str()),
                ("client_secret", self.secret_key.as_str()),
            ])
            .send()
            .await?;
        let resp = parse_token_resp(resp).await?;
        if resp.error.is_empty() && !resp.access_token.is_empty() {
            let nt = TokenState {
                access_token: resp.access_token,
                refresh_token: resp.refresh_token,
                expires_at: now_ts() + resp.expires_in - 60,
            };
            self.save_token(&nt).await;
            Ok(nt.access_token)
        } else {
            anyhow::bail!("百度令牌刷新失败，请重新授权: {}", resp.error_description)
        }
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
                .http
                .get(format!("{PAN}/xpan/file"))
                .query(&[
                    ("method", "list"),
                    ("access_token", token.as_str()),
                    ("dir", dir),
                    ("start", &start.to_string()),
                    ("limit", "1000"),
                    ("web", "1"),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if resp.errno != 0 {
                anyhow::bail!("百度网盘列目录失败 errno={}（目录不存在或令牌无效）", resp.errno);
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
                let full = format!("{}/{}", parent_of(&cur).trim_end_matches('/'), f.server_filename);
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

    /// 按绝对云端路径逐级创建目录（可越出同步目录，供元数据区使用）。
    async fn mkdir_p_abs(&self, abs: &str) -> anyhow::Result<()> {
        let mut cur = String::new();
        let mut created = false;
        for seg in abs.trim_start_matches('/').split('/') {
            if seg.is_empty() {
                continue;
            }
            cur = format!("{cur}/{seg}");
            if self.lookup_abs(&cur).await?.is_some() {
                continue;
            }
            created = true;
            let token = self.access_token().await?;
            let resp: CreateResp = self
                .http
                .post(format!("{PAN}/xpan/file"))
                .query(&[
                    ("method", "create"),
                    ("access_token", token.as_str()),
                ])
                .form(&[
                    ("path", cur.as_str()),
                    ("isdir", "1"),
                    ("size", "0"),
                    ("rtype", "0"),
                ])
                .send()
                .await?
                .json()
                .await?;
            if resp.errno != 0 && resp.errno != -8 {
                // -8 = 已存在
                anyhow::bail!("创建目录 {cur} 失败 errno={}", resp.errno);
            }
        }
        if created {
            self.invalidate_list_cache().await;
        }
        Ok(())
    }

    /// 读取指定绝对路径的 manifest（不存在返回 None）。
    async fn manifest_at(&self, abs: &str) -> anyhow::Result<Option<Manifest>> {
        match self.lookup_abs(abs).await? {
            None => Ok(None),
            Some(fs_id) => {
                let token = self.access_token().await?;
                let metas: FileMetasResp = self
                    .http
                    .get(format!("{PAN}/xpan/multimedia"))
                    .query(&[
                        ("method", "filemetas"),
                        ("access_token", token.as_str()),
                        ("fsids", format!("[{fs_id}]").as_str()),
                        ("dlink", "1"),
                    ])
                    .send()
                    .await?
                    .json()
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
                    .await?
                    .error_for_status()?
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
    fn kind(&self) -> &'static str {
        "baidu"
    }

    fn raw_io(&self) -> &dyn super::RawIo {
        self
    }

    async fn test(&self) -> anyhow::Result<()> {
        self.access_token().await?;
        self.mkdir_p("").await?;
        Ok(())
    }

    async fn read_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        self.manifest_at(&self.meta_path("manifest.json")).await
    }

    async fn write_manifest(&self, manifest: &Manifest) -> anyhow::Result<()> {
        let tmp = std::env::temp_dir().join(format!("synccloud-baidu-manifest-{}.json", uuid::Uuid::new_v4()));
        tokio::fs::write(&tmp, serde_json::to_vec_pretty(manifest)?).await?;
        self.mkdir_p_abs(&self.meta_dir).await?;
        let r = self.put_file_abs(&self.meta_path("manifest.json"), &tmp).await;
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
        let body = self
            .http
            .get(format!("{PAN}/xpan/multimedia"))
            .query(&[
                ("method", "filemetas"),
                ("access_token", token.as_str()),
                ("fsids", format!("[{fs_id}]").as_str()),
                ("dlink", "0"),
            ])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        // null 容错解析
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        let item = v.get("list").and_then(|l| l.get(0));
        let lm = item.and_then(|i| i.get("server_mtime")).and_then(|x| x.as_i64()).unwrap_or(0);
        let len = item.and_then(|i| i.get("size")).and_then(|x| x.as_u64()).unwrap_or(0);
        Ok(Some(format!("fs{fs_id}|lm{lm}|len{len}")))
    }

    async fn read_legacy_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        self.manifest_at(&self.abs_path(".synccloud/manifest.json")).await
    }

    async fn cleanup_legacy_meta(&self) -> anyhow::Result<()> {
        let legacy = self.abs_path(".synccloud");
        if legacy == self.meta_dir {
            return Ok(()); // 根级退化：元数据区与旧位置同位
        }
        // 逐个删除已知元数据文件，目录删除尽力而为（filemanager 支持目录递归删）
        let _ = self.delete_abs(&format!("{legacy}/manifest.json")).await;
        if let Ok(items) = self.list_dir(&format!("{legacy}/locks")).await {
            for f in items {
                if f.isdir == 0 {
                    let _ = self.delete_abs(&format!("{legacy}/locks/{}", f.server_filename)).await;
                }
            }
        }
        let _ = self.delete_abs(&format!("{legacy}/locks")).await;
        let _ = self.delete_abs(&legacy).await;
        Ok(())
    }

    async fn put_file(&self, path: &str, src: &Path) -> anyhow::Result<()> {
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
        self.get_file_abs(&self.abs_path(path), dest).await
    }

    async fn delete_file(&self, path: &str) -> anyhow::Result<()> {
        self.delete_abs(&self.abs_path(path)).await
    }

    async fn list_objects(&self) -> anyhow::Result<Vec<RemoteEntry>> {
        // BFS 递归列出 remote_dir 下所有文件（跳过 .synccloud）
        let mut out = Vec::new();
        let mut queue = vec![self.remote_dir.clone()];
        while let Some(dir) = queue.pop() {
            for item in self.list_dir(&dir).await? {
                if item.server_filename.starts_with(".synccloud") || item.server_filename.ends_with(".synccloud-part") {
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
        // 共享模式读取：文件被占用时也能上传
        let data = crate::sync::fslock::read_shared(src).await?;
        let size = data.len();
        // 分块 md5（4MB 一块）
        const BLOCK: usize = 4 * 1024 * 1024;
        let mut block_list = Vec::new();
        if size == 0 {
            block_list.push(md5_hex(b""));
        } else {
            for chunk in data.chunks(BLOCK) {
                block_list.push(md5_hex(chunk));
            }
        }
        let block_json = serde_json::to_string(&block_list)?;
        let token = self.access_token().await?;

        // 1. precreate
        let rid = chrono::Utc::now().timestamp_millis().to_string();
        let pre: PrecreateResp = self
            .http
            .post(format!("{PAN}/xpan/file"))
            .query(&[("method", "precreate"), ("access_token", token.as_str())])
            .form(&[
                ("path", abs),
                ("size", &size.to_string()),
                ("isdir", "0"),
                ("autoinit", "1"),
                ("rtype", "3"), // 覆盖同名
                ("block_list", block_json.as_str()),
                ("rid", rid.as_str()),
            ])
            .send()
            .await?
            .json()
            .await?;
        if pre.errno != 0 {
            anyhow::bail!("百度预创建失败 errno={}（检查路径与配额）", pre.errno);
        }
        // 秒传：远端已有同内容文件，precreate 已直接完成，跳过分片与合并
        if pre.return_type == 2 {
            return Ok(());
        }

        // 2. superfile2 分块上传（partseq 从 0 开始计数）
        let total_parts = block_list.len().max(1);
        for (i, chunk) in (if size == 0 { vec![&data[..]] } else { data.chunks(BLOCK).collect::<Vec<_>>() })
            .into_iter()
            .enumerate()
        {
            let resp = self
                .http
                .put(PCS)
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
                .await?;
            if !resp.status().is_success() {
                let code = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("百度分片 {} 上传失败: HTTP {code} {}", i, truncate(&body, 120));
            }
        }
        let _ = total_parts;

        // 3. create 合并
        let done: CreateResp = self
            .http
            .post(format!("{PAN}/xpan/file"))
            .query(&[("method", "create"), ("access_token", token.as_str())])
            .form(&[
                ("path", abs),
                ("size", &size.to_string()),
                ("isdir", "0"),
                ("rtype", "3"),
                ("block_list", block_json.as_str()),
                ("uploadid", pre.uploadid.as_str()),
            ])
            .send()
            .await?
            .json()
            .await?;
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
            .http
            .get(format!("{PAN}/xpan/multimedia"))
            .query(&[
                ("method", "filemetas"),
                ("access_token", token.as_str()),
                ("fsids", format!("[{fs_id}]").as_str()),
                ("dlink", "1"),
            ])
            .send()
            .await?
            .json()
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
                    last_err = Some(e.into());
                    continue;
                }
            };
            let resp = match resp.error_for_status() {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(e.into());
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
                        got += c.len() as u64;
                        if let Err(e) = file.write_all(&c).await {
                            stream_err = Some(e.into());
                            break;
                        }
                    }
                    Err(e) => {
                        stream_err = Some(e.into());
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
            .post(format!("{PAN}/xpan/file"))
            .query(&[
                ("method", "filemanager"),
                ("opera", "delete"),
                ("access_token", token.as_str()),
            ])
            .form(&[("filelist", filelist.as_str())])
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("百度删除文件失败: HTTP {status}: {}", truncate(&body, 120));
        }
        let v: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            // 2xx 但非 JSON（空体等）视为成功
            Err(_) => return Ok(()),
        };
        let errno = v.get("errno").and_then(|x| x.as_i64()).unwrap_or(0);
        if errno != 0 {
            // 文件不存在视为成功（幂等）
            if errno == 12 || errno == -6 || errno == -9 {
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
        let tmp = std::env::temp_dir().join(format!("synccloud-baidu-lock-{}.txt", uuid::Uuid::new_v4()));
        tokio::fs::write(&tmp, content).await?;
        let p = self.meta_path(key);
        self.mkdir_p_abs(&parent_of(&p)).await?;
        let r = self.put_file_abs(&p, &tmp).await;
        let _ = tokio::fs::remove_file(&tmp).await;
        r
    }

    async fn read_text(&self, key: &str) -> anyhow::Result<Option<String>> {
        let tmp = std::env::temp_dir().join(format!("synccloud-baidu-read-{}.txt", uuid::Uuid::new_v4()));
        match self.get_file_abs(&self.meta_path(key), &tmp).await {
            Ok(()) => {
                let s = tokio::fs::read_to_string(&tmp).await.ok();
                let _ = tokio::fs::remove_file(&tmp).await;
                Ok(s)
            }
            Err(_) => Ok(None),
        }
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.delete_abs(&self.meta_path(key)).await
    }

    async fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let dir = self.meta_path(prefix.trim_end_matches('/'));
        let mut out = Vec::new();
        for item in self.list_dir(&dir).await? {
            if item.isdir == 0 {
                out.push(format!("{}/{}", prefix.trim_end_matches('/'), item.server_filename));
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
