//! WebDAV 后端：坚果云、Nextcloud、ownCloud、群晖 DSM、InfiniCLOUD 等。
//! 基于 reqwest + quick-xml 手写 PROPFIND/GET/PUT/DELETE/MKCOL。

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::{Client, Method, StatusCode};
use url::Url;

use super::{RemoteEntry, RemoteStore};
use crate::sync::types::Manifest;

pub struct WebDavStore {
    http: Client,
    /// 仓库根 URL，例如 `https://dav.jianguoyun.com/dav/synccloud/docs/`
    base: Url,
    /// 元数据区 URL：仓库根的兄弟目录 `<base>/../.synccloud/synccloud/`
    meta: Url,
    auth: Option<(String, String)>,
}

#[derive(Debug)]
struct DavItem {
    href: String,
    is_dir: bool,
    size: u64,
    /// HTTP ETag（部分服务器可能不返回）
    etag: Option<String>,
    /// Last-Modified 原始字符串
    modified: Option<String>,
}

impl WebDavStore {
    pub fn new(url: String, username: String, password: String, root_path: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(20))
            .danger_accept_invalid_certs(false)
            .build()
            .expect("reqwest client");
        // 规范化根地址：<url>/<root_path>/synccloud/（尾斜杠保证 join 相对路径正确）
        let mut base = Url::parse(&url).expect("WebDAV URL 非法");
        {
            let mut path = base.path().trim_end_matches('/').to_string();
            let root = root_path.trim_matches('/');
            if !root.is_empty() {
                path.push('/');
                path.push_str(root);
            }
            path.push_str("/synccloud/");
            base.set_path(&path);
        }
        // 元数据区与仓库根同级，用户在同步目录内看不到任何引擎文件
        let meta = base.join("../.synccloud/synccloud/").expect("join meta path");
        let auth = (!username.is_empty() || !password.is_empty())
            .then(|| (username, password));
        Self { http, base, meta, auth }
    }

    fn url_of(&self, rel: &str) -> Url {
        let rel = rel.trim_start_matches('/');
        if rel.is_empty() {
            return self.base.clone();
        }
        self.base.join(rel).expect("join path")
    }

    fn meta_url_of(&self, rel: &str) -> Url {
        let rel = rel.trim_start_matches('/');
        if rel.is_empty() {
            return self.meta.clone();
        }
        self.meta.join(rel).expect("join path")
    }

    fn request(&self, method: Method, rel: &str) -> reqwest::RequestBuilder {
        self.request_url(method, self.url_of(rel))
    }

    fn request_url(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        let mut rb = self.http.request(method, url);
        if let Some((user, pass)) = &self.auth {
            rb = rb.basic_auth(user, Some(pass));
        }
        rb
    }

    /// MKCOL 元数据目录（逐级，已存在 405/409 容忍）。
    async fn ensure_meta_dir(&self) -> anyhow::Result<()> {
        let parent = {
            let s = self.base.as_str().trim_end_matches('/');
            s.strip_suffix("/synccloud").unwrap_or(s).to_string()
        };
        for seg in ["/.synccloud", "/.synccloud/synccloud"] {
            let u = Url::parse(&format!("{parent}{seg}"))
                .map_err(|e| anyhow::anyhow!("元数据 URL 非法: {e}"))?;
            let status = self
                .request_url(Method::from_bytes(b"MKCOL")?, u)
                .send()
                .await?
                .status();
            let ok = status.is_success()
                || status == StatusCode::METHOD_NOT_ALLOWED
                || status == StatusCode::CONFLICT;
            if !ok {
                anyhow::bail!("MKCOL 元数据目录失败: HTTP {status}");
            }
        }
        Ok(())
    }

    /// PROPFIND Depth:1，解析直系子项。
    async fn propfind1(&self, rel: &str) -> anyhow::Result<Vec<DavItem>> {
        self.propfind_url(&self.url_of(rel), &self.base).await
    }

    /// PROPFIND 任意 URL，结果相对于 parse_base 解析。
    async fn propfind_url(&self, url: &Url, parse_base: &Url) -> anyhow::Result<Vec<DavItem>> {
        let resp = self
            .request_url(Method::from_bytes(b"PROPFIND")?, url.clone())
            .header("Depth", "1")
            .header("Content-Type", "application/xml")
            .body(PROPFIND_BODY)
            .send()
            .await?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        if !status.is_success() && status.as_u16() != 207 {
            anyhow::bail!("PROPFIND {} 失败: HTTP {status}", url.path());
        }
        let text = resp.text().await?;
        parse_propfind(&text, parse_base)
    }

    async fn mkcol_all(&self, rel_dir: &str) -> anyhow::Result<()> {
        // 逐级 MKCOL（已存在返回 405，视为成功）
        let mut cur = String::new();
        for seg in rel_dir.trim_matches('/').split('/') {
            if seg.is_empty() {
                continue;
            }
            if !cur.is_empty() {
                cur.push('/');
            }
            cur.push_str(seg);
            let status = self
                .request(Method::from_bytes(b"MKCOL")?, &cur)
                .send()
                .await?
                .status();
            let ok = status.is_success() || status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::CONFLICT;
            if !ok {
                anyhow::bail!("MKCOL {cur} 失败: HTTP {status}");
            }
        }
        Ok(())
    }
}

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:resourcetype/>
    <D:getcontentlength/>
    <D:getetag/>
    <D:getlastmodified/>
  </D:prop>
</D:propfind>"#;

/// 解析 multistatus。兼容 D:/d:/无前缀等命名空间写法（按 local-name 匹配）。
fn parse_propfind(xml: &str, base: &Url) -> anyhow::Result<Vec<DavItem>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut items = Vec::new();
    let mut cur_href: Option<String> = None;
    let mut cur_is_dir = false;
    let mut cur_size: u64 = 0;
    let mut cur_etag: Option<String> = None;
    let mut cur_modified: Option<String> = None;
    let mut text_into: Option<&'static str> = None;

    let base_path = base.path().trim_end_matches('/').to_string();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                match name {
                    b"response" => {
                        cur_href = None;
                        cur_is_dir = false;
                        cur_size = 0;
                        cur_etag = None;
                        cur_modified = None;
                    }
                    b"href" if cur_href.is_none() => {
                        // 仅 response 直下第一个 href
                        if let Ok(Event::Text(t)) = reader.read_event() {
                            let raw = t.unescape()?.to_string();
                            cur_href = Some(raw);
                        }
                    }
                    b"collection" => cur_is_dir = true,
                    b"getcontentlength" => text_into = Some("len"),
                    b"getetag" => text_into = Some("etag"),
                    b"getlastmodified" => text_into = Some("lm"),
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                match text_into {
                    Some("len") => {
                        let s = t.unescape()?;
                        cur_size = s.parse().unwrap_or(0);
                    }
                    Some("etag") => {
                        cur_etag = Some(t.unescape()?.to_string());
                    }
                    Some("lm") => {
                        cur_modified = Some(t.unescape()?.to_string());
                    }
                    _ => {}
                }
                text_into = None;
            }
            Ok(Event::End(e)) => {
                let name = e.local_name();
                let n = name.as_ref();
                if matches!(n, b"getcontentlength" | b"getetag" | b"getlastmodified") {
                    text_into = None;
                }
                if n == b"response" {
                    if let Some(href) = cur_href.take() {
                        // href URL 解码 + 剥离 base 前缀 → 相对路径
                        let decoded = percent_encoding::percent_decode_str(&href)
                            .decode_utf8_lossy()
                            .to_string();
                        let decoded = decoded.trim_end_matches('/');
                        let base_decoded = percent_encoding::percent_decode_str(&base_path)
                            .decode_utf8_lossy()
                            .to_string();
                        let base_decoded = base_decoded.trim_end_matches('/');
                        let rel = decoded
                            .strip_prefix(&base_decoded)
                            .map(|r| r.trim_start_matches('/').to_string())
                            .unwrap_or_else(|| decoded.to_string());
                        if rel.is_empty() {
                            continue; // 仓库根自身
                        }
                        items.push(DavItem {
                            href: rel,
                            is_dir: cur_is_dir,
                            size: cur_size,
                            etag: cur_etag.take(),
                            modified: cur_modified.take(),
                        });
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => anyhow::bail!("解析 PROPFIND 响应失败: {e}"),
            _ => {}
        }
    }
    Ok(items)
}

#[async_trait]
impl RemoteStore for WebDavStore {
    fn kind(&self) -> &'static str {
        "webdav"
    }

    fn raw_io(&self) -> &dyn super::RawIo {
        self
    }

    async fn test(&self) -> anyhow::Result<()> {
        // 读验证：PROPFIND 仓库根（Depth:0 探测可达性与认证）
        let resp = self
            .request(Method::from_bytes(b"PROPFIND")?, "")
            .header("Depth", "0")
            .header("Content-Type", "application/xml")
            .body(PROPFIND_BODY)
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() != 207 && !status.is_success() {
            anyhow::bail!("WebDAV 连接失败: HTTP {status}（检查地址、账号与应用密码）");
        }
        // 写验证：幂等创建元数据目录（同步目录外）
        self.ensure_meta_dir().await?;
        Ok(())
    }

    async fn read_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        let resp = self.request_url(Method::GET, self.meta_url_of("manifest.json")).send().await?;
        match resp.status() {
            StatusCode::OK => {
                let raw = resp.text().await?;
                Ok(Some(serde_json::from_str(&raw)?))
            }
            StatusCode::NOT_FOUND => Ok(None),
            s => anyhow::bail!("读取 manifest 失败: HTTP {s}"),
        }
    }

    async fn write_manifest(&self, manifest: &Manifest) -> anyhow::Result<()> {
        self.ensure_meta_dir().await?;
        let raw = serde_json::to_vec_pretty(manifest)?;
        let status = self
            .request_url(Method::PUT, self.meta_url_of("manifest.json"))
            .header("Content-Type", "application/json")
            .body(raw)
            .send()
            .await?
            .status();
        if !status.is_success() {
            anyhow::bail!("写入 manifest 失败: HTTP {status}");
        }
        Ok(())
    }

    /// PROPFIND Depth:1 元数据目录，取 manifest.json 的 etag/修改时间/大小
    /// 作为增量快路径的廉价变更戳（一次请求代替整个 manifest 下载）。
    async fn manifest_stamp(&self) -> anyhow::Result<Option<String>> {
        let items = self
            .propfind_url(&self.meta_url_of(""), &self.meta)
            .await?;
        for it in items {
            if !it.is_dir && it.href == "manifest.json" {
                return Ok(Some(format!(
                    "etag{}|lm{}|len{}",
                    it.etag.as_deref().unwrap_or(""),
                    it.modified.as_deref().unwrap_or(""),
                    it.size
                )));
            }
        }
        Ok(None)
    }

    async fn read_legacy_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        let resp = self.request(Method::GET, ".synccloud/manifest.json").send().await?;
        match resp.status() {
            StatusCode::OK => {
                let raw = resp.text().await?;
                Ok(Some(serde_json::from_str(&raw)?))
            }
            StatusCode::NOT_FOUND => Ok(None),
            s => anyhow::bail!("读取旧版 manifest 失败: HTTP {s}"),
        }
    }

    async fn cleanup_legacy_meta(&self) -> anyhow::Result<()> {
        if self.meta.path() == format!("{}/.synccloud/", self.base.path()) {
            return Ok(()); // 结构上不可能同位，防御性保留
        }
        let _ = self.delete_file(".synccloud/manifest.json").await;
        for item in self.propfind1(".synccloud/locks").await? {
            if !item.is_dir {
                let _ = self.delete_file(&format!(".synccloud/locks/{}", item.href)).await;
            }
        }
        // 空目录清理（尽力而为）
        let _ = self.request(Method::DELETE, ".synccloud/locks").send().await;
        let _ = self.request(Method::DELETE, ".synccloud").send().await;
        Ok(())
    }

    async fn put_file(&self, path: &str, src: &Path) -> anyhow::Result<()> {
        if let Some(parent) = Path::new(path).parent() {
            let parent_str = parent.to_string_lossy().replace('\\', "/");
            let parent_str = parent_str.trim_start_matches('/').to_string();
            if !parent_str.is_empty() {
                self.mkcol_all(&parent_str).await?;
            }
        }
        let file = tokio::fs::File::open(src).await
            .map_err(|e| anyhow::anyhow!("打开本地文件失败 {}: {e}", src.display()))?;
        let len = file.metadata().await?.len();
        let stream = tokio_util::io::ReaderStream::new(file);
        let body = reqwest::Body::wrap_stream(stream);
        let status = self
            .request(Method::PUT, path)
            .header("Content-Length", len)
            .body(body)
            .send()
            .await?
            .status();
        if !status.is_success() {
            anyhow::bail!("上传 WebDAV:{path} 失败: HTTP {status}");
        }
        Ok(())
    }

    async fn get_file(&self, path: &str, dest: &Path) -> anyhow::Result<()> {
        let resp = self.request(Method::GET, path).send().await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("下载 WebDAV:{path} 失败: HTTP {status}");
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::File::create(dest).await?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &bytes).await?;
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        Ok(())
    }

    async fn delete_file(&self, path: &str) -> anyhow::Result<()> {
        let status = self.request(Method::DELETE, path).send().await?.status();
        if status.is_success() || status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            anyhow::bail!("删除 WebDAV:{path} 失败: HTTP {status}");
        }
    }

    async fn list_objects(&self) -> anyhow::Result<Vec<RemoteEntry>> {
        // BFS 逐层 PROPFIND（Depth:infinity 多数服务器禁用）
        let mut out = Vec::new();
        let mut queue: Vec<String> = vec![String::new()];
        while let Some(dir) = queue.pop() {
            let items = self.propfind1(&dir).await?;
            for item in items {
                let name = item.href.trim_matches('/');
                if name.starts_with(".synccloud") || name.ends_with(".synccloud-part") {
                    continue;
                }
                let full = if dir.is_empty() {
                    item.href
                } else {
                    format!("{dir}/{}", item.href)
                };
                if item.is_dir {
                    queue.push(full);
                } else {
                    out.push(RemoteEntry {
                        key: full,
                        size: item.size,
                    });
                }
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl super::RawIo for WebDavStore {
    async fn put_text(&self, key: &str, content: &str) -> anyhow::Result<()> {
        self.ensure_meta_dir().await?;
        if let Some(parent) = Path::new(key).parent() {
            let p = parent.to_string_lossy().replace('\\', "/");
            let p = p.trim_start_matches('/').to_string();
            if !p.is_empty() {
                let status = self
                    .request_url(Method::from_bytes(b"MKCOL")?, self.meta_url_of(&p))
                    .send()
                    .await?
                    .status();
                let ok = status.is_success()
                    || status == StatusCode::METHOD_NOT_ALLOWED
                    || status == StatusCode::CONFLICT;
                if !ok {
                    anyhow::bail!("MKCOL 元数据 {p} 失败: HTTP {status}");
                }
            }
        }
        let status = self
            .request_url(Method::PUT, self.meta_url_of(key))
            .body(content.to_string())
            .send()
            .await?
            .status();
        if !status.is_success() {
            anyhow::bail!("PUT 元数据 {key} 失败: HTTP {status}");
        }
        Ok(())
    }

    async fn read_text(&self, key: &str) -> anyhow::Result<Option<String>> {
        let resp = self.request_url(Method::GET, self.meta_url_of(key)).send().await?;
        match resp.status() {
            StatusCode::OK => Ok(Some(resp.text().await?)),
            StatusCode::NOT_FOUND => Ok(None),
            s => anyhow::bail!("GET 元数据 {key} 失败: HTTP {s}"),
        }
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let status = self
            .request_url(Method::DELETE, self.meta_url_of(key))
            .send()
            .await?
            .status();
        if status.is_success() || status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            anyhow::bail!("DELETE 元数据 {key} 失败: HTTP {status}");
        }
    }

    async fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let items = self
            .propfind_url(&self.meta_url_of(prefix.trim_end_matches('/')), &self.meta)
            .await?;
        Ok(items.into_iter().filter(|i| !i.is_dir).map(|i| i.href).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_jianguoyun_style_propfind() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
 <D:response>
  <D:href>/dav/synccloud/docs/</D:href>
  <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat>
 </D:response>
 <D:response>
  <D:href>/dav/synccloud/docs/a.txt</D:href>
  <D:propstat><D:prop>
    <D:resourcetype/>
    <D:getcontentlength>128</D:getcontentlength>
  </D:prop></D:propstat>
 </D:response>
 <D:response>
  <D:href>/dav/synccloud/docs/%E4%B8%AD%E6%96%87%20%E6%96%87%E4%BB%B6.md</D:href>
  <D:propstat><D:prop>
    <D:resourcetype/>
    <D:getcontentlength>256</D:getcontentlength>
  </D:prop></D:propstat>
 </D:response>
</D:multistatus>"#;
        let base = Url::parse("https://dav.example.com/dav/synccloud/docs").unwrap();
        let items = parse_propfind(xml, &base).unwrap();
        assert_eq!(items.len(), 2); // 根自身被剥掉
        assert!(!items[0].is_dir);
        assert_eq!(items[0].href, "a.txt");
        assert_eq!(items[0].size, 128);
        assert_eq!(items[1].href, "中文 文件.md");
        assert_eq!(items[1].size, 256);
    }

    #[test]
    fn parse_nextcloud_style_propfind() {
        // Nextcloud 常用 d: 小写前缀
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
 <d:response>
  <d:href>/remote.php/dav/files/user/synccloud/sub/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
 </d:response>
</d:multistatus>"#;
        let base = Url::parse("https://cloud.example.com/remote.php/dav/files/user/synccloud").unwrap();
        let items = parse_propfind(xml, &base).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].is_dir);
        assert_eq!(items[0].href, "sub");
    }
}
