//! S3 兼容后端：AWS S3 / MinIO / 阿里云 OSS(S3 兼容) / 腾讯云 COS / Cloudflare R2 / Backblaze B2。

use std::path::Path;

use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::operation::get_object::GetObjectOutput;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use async_trait::async_trait;
#[allow(unused_imports)]
use futures::StreamExt;

use super::{RemoteEntry, RemoteStore, sibling_meta};
use crate::sync::types::Manifest;

pub const META_DIR: &str = ".synccloud";

pub struct S3Store {
    client: Client,
    bucket: String,
    prefix: String,
    /// 元数据键前缀：同步前缀的兄弟（`synccloud/docs` → `synccloud/.synccloud/docs`；
    /// 根前缀同步整桶时退化为桶根 `.synccloud`，靠列举过滤排除）
    meta_prefix: String,
}

impl S3Store {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        endpoint: String,
        region: String,
        bucket: String,
        access_key_id: String,
        secret_access_key: String,
        path_style: bool,
        root_prefix: String,
    ) -> Self {
        let sdk_cfg = aws_config::defaults(BehaviorVersion::latest())
            .credentials_provider(Credentials::new(
                access_key_id,
                secret_access_key,
                None,
                None,
                "SyncCloud",
            ))
            .region(Region::new(region))
            .endpoint_url(endpoint)
            .load()
            .await;
        let mut builder = aws_sdk_s3::config::Builder::from(&sdk_cfg);
        if path_style {
            builder = builder.force_path_style(true);
        }
        let prefix = root_prefix.trim_matches('/').to_string();
        // 严格位于用户指定的前缀目录内部（{prefix}/.synccloud），绝不越出指定目录创建任何对象
        let meta_prefix = if prefix.is_empty() {
            ".synccloud".to_string()
        } else {
            format!("{prefix}/.synccloud")
        };
        Self {
            client: Client::from_conf(builder.build()),
            bucket,
            prefix,
            meta_prefix,
        }
    }

    fn key(&self, rel: &str) -> String {
        let rel = rel.trim_start_matches('/');
        if self.prefix.is_empty() {
            rel.to_string()
        } else {
            format!("{}/{}", self.prefix, rel)
        }
    }

    /// 元数据区内对象的完整键（位于同步前缀之外）。
    fn meta_key(&self, rel: &str) -> String {
        format!("{}/{}", self.meta_prefix, rel.trim_start_matches('/'))
    }
}

async fn write_object_to_file(obj: GetObjectOutput, dest: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = obj.body;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &bytes[..]).await?;
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    Ok(())
}

fn is_not_found(e: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>) -> bool {
    let s = e.to_string();
    s.contains("NoSuchKey") || s.contains("404") || s.contains("NotFound")
}

#[async_trait]
impl RemoteStore for S3Store {
    fn kind(&self) -> &'static str {
        "s3"
    }

    fn raw_io(&self) -> &dyn super::RawIo {
        self
    }

    async fn test(&self) -> anyhow::Result<()> {
        // 读探针：HeadBucket
        self.client
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("S3 连接失败: {e}"))?;
        // 写探针：写删一个小对象（验证写权限，探针位于元数据区）
        let probe = self.meta_key(".probe");
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&probe)
            .body(ByteStream::from_static(b"probe"))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("S3 写入权限测试失败: {e}"))?;
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&probe)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("S3 删除权限测试失败: {e}"))?;
        Ok(())
    }

    async fn read_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        let key = self.meta_key("manifest.json");
        match self.client.get_object().bucket(&self.bucket).key(&key).send().await {
            Ok(obj) => {
                let tmp = std::env::temp_dir()
                    .join(format!("synccloud-manifest-{}.json", uuid::Uuid::new_v4()));
                write_object_to_file(obj, &tmp).await?;
                let raw = tokio::fs::read_to_string(&tmp).await?;
                let _ = tokio::fs::remove_file(&tmp).await;
                Ok(Some(serde_json::from_str(&raw)?))
            }
            Err(e) if is_not_found(&e) => {
                // 兼容迁移：若指定同步目录内尚无 manifest，但历史外部兄弟前缀存在，读取并迁入内部，同时清理外部旧对象
                let legacy_prefix = sibling_meta(&self.prefix);
                let legacy_key = format!("{legacy_prefix}/manifest.json");
                if let Ok(obj) = self.client.get_object().bucket(&self.bucket).key(&legacy_key).send().await {
                    let tmp = std::env::temp_dir()
                        .join(format!("synccloud-manifest-{}.json", uuid::Uuid::new_v4()));
                    write_object_to_file(obj, &tmp).await?;
                    let raw = tokio::fs::read_to_string(&tmp).await?;
                    let _ = tokio::fs::remove_file(&tmp).await;
                    if let Ok(m) = serde_json::from_str::<Manifest>(&raw) {
                        let _ = self.write_manifest(&m).await;
                        let _ = self.client.delete_object().bucket(&self.bucket).key(&legacy_key).send().await;
                        return Ok(Some(m));
                    }
                }
                Ok(None)
            }
            Err(e) => Err(anyhow::anyhow!("读取 manifest 失败: {e}")),
        }
    }

    /// HEAD 一次 manifest 对象取 etag/last-modified，作为增量快路径的廉价变更戳。
    async fn manifest_stamp(&self) -> anyhow::Result<Option<String>> {
        let key = self.meta_key("manifest.json");
        match self.client.head_object().bucket(&self.bucket).key(&key).send().await {
            Ok(out) => Ok(Some(format!(
                "etag{}|lm{}|len{}",
                out.e_tag().unwrap_or(""),
                out.last_modified().map(|t| t.secs()).unwrap_or(0),
                out.content_length().unwrap_or(0),
            ))),
            Err(e) => {
                let s = e.to_string();
                if s.contains("NoSuchKey") || s.contains("404") || s.contains("NotFound") {
                    Ok(None)
                } else {
                    Err(anyhow::anyhow!("探测 manifest 变更戳失败: {e}"))
                }
            }
        }
    }

    async fn write_manifest(&self, manifest: &Manifest) -> anyhow::Result<()> {
        let key = self.meta_key("manifest.json");
        let raw = serde_json::to_vec_pretty(manifest)?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(ByteStream::from(raw))
            .content_type("application/json")
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("写入 manifest 失败: {e}"))?;
        Ok(())
    }

    async fn put_file(&self, path: &str, src: &Path) -> anyhow::Result<()> {
        let key = self.key(path);
        // 共享模式打开：文件被编辑器/杀毒等占用时仍可读取上传
        let file = crate::sync::fslock::open_read_shared_async(src).await
            .map_err(|e| anyhow::anyhow!("打开本地文件失败 {}: {e}", src.display()))?;
        let len = file.metadata().await?.len();
        let stream = ByteStream::read_from()
            .file(file)
            .length(aws_smithy_types::byte_stream::Length::Exact(len))
            .build()
            .await
            .map_err(|e| anyhow::anyhow!("构造上传流失败 {}: {e}", src.display()))?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(stream)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("上传失败 s3://{}/{}: {e}", self.bucket, key))?;
        Ok(())
    }

    async fn get_file(&self, path: &str, dest: &Path) -> anyhow::Result<()> {
        let key = self.key(path);
        let obj = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("下载失败 s3://{}/{}: {e}", self.bucket, key))?;
        write_object_to_file(obj, dest).await?;
        Ok(())
    }

    async fn delete_file(&self, path: &str) -> anyhow::Result<()> {
        let key = self.key(path);
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("删除失败 s3://{}/{}: {e}", self.bucket, key))?;
        Ok(())
    }

    async fn move_file(&self, from: &str, to: &str) -> anyhow::Result<()> {
        let src_key = self.key(from);
        let dest_key = self.key(to);
        let copy_source = format!("{}/{}", self.bucket, src_key);
        self.client
            .copy_object()
            .bucket(&self.bucket)
            .copy_source(&copy_source)
            .key(&dest_key)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("S3 服务端复制失败 {}: {e}", dest_key))?;
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&src_key)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("S3 清理原对象失败 {}: {e}", src_key))?;
        Ok(())
    }

    async fn read_legacy_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        let key = self.key(".synccloud/manifest.json");
        match self.client.get_object().bucket(&self.bucket).key(&key).send().await {
            Ok(obj) => {
                let tmp = std::env::temp_dir()
                    .join(format!("synccloud-manifest-{}.json", uuid::Uuid::new_v4()));
                write_object_to_file(obj, &tmp).await?;
                let raw = tokio::fs::read_to_string(&tmp).await?;
                let _ = tokio::fs::remove_file(&tmp).await;
                Ok(Some(serde_json::from_str(&raw)?))
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("读取旧版 manifest 失败: {e}")),
        }
    }

    async fn cleanup_legacy_meta(&self) -> anyhow::Result<()> {
        // 根前缀同步整桶时元数据区与旧位置同位，不能清理
        if self.meta_prefix == META_DIR {
            return Ok(());
        }
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.key(".synccloud/manifest.json"))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("清理旧版 manifest 失败: {e}"))?;
        let legacy_locks = self.key(".synccloud/locks/");
        let mut resp = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(&legacy_locks)
            .into_paginator()
            .send();
        let mut keys = Vec::new();
        while let Some(page) = resp.next().await {
            let page = page.map_err(|e| anyhow::anyhow!("列举旧版锁失败: {e}"))?;
            for obj in page.contents() {
                if let Some(k) = obj.key() {
                    keys.push(k.to_string());
                }
            }
        }
        for k in keys {
            let _ = self
                .client
                .delete_object()
                .bucket(&self.bucket)
                .key(&k)
                .send()
                .await;
        }
        Ok(())
    }

    async fn list_objects(&self) -> anyhow::Result<Vec<RemoteEntry>> {
        let prefix = if self.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.prefix)
        };
        let mut resp = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(&prefix)
            .into_paginator()
            .send();
        let mut out = Vec::new();
        while let Some(page) = resp.next().await {
            let page = page.map_err(|e| anyhow::anyhow!("列举对象失败: {e}"))?;
            for obj in page.contents() {
                if let Some(key) = obj.key() {
                    if key.starts_with(&format!("{prefix}{META_DIR}/")) {
                        continue;
                    }
                    out.push(RemoteEntry {
                        key: key.strip_prefix(&prefix).unwrap_or(key).to_string(),
                        size: obj.size().unwrap_or(0) as u64,
                    });
                }
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl super::RawIo for S3Store {
    async fn put_text(&self, key: &str, content: &str) -> anyhow::Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(self.meta_key(key))
            .body(ByteStream::from(content.as_bytes().to_vec()))
            .send()
            .await?;
        Ok(())
    }

    async fn read_text(&self, key: &str) -> anyhow::Result<Option<String>> {
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.meta_key(key))
            .send()
            .await
        {
            Ok(obj) => {
                let mut buf = Vec::new();
                let mut stream = obj.body;
                while let Some(chunk) = stream.next().await {
                    buf.extend_from_slice(&chunk?[..]);
                }
                Ok(Some(String::from_utf8(buf)?))
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.meta_key(key))
            .send()
            .await?;
        Ok(())
    }

    async fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let full = self.meta_key(prefix);
        let strip = format!("{}/", self.meta_prefix);
        let mut resp = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(&full)
            .into_paginator()
            .send();
        let mut out = Vec::new();
        while let Some(page) = resp.next().await {
            let page = page.map_err(|e| anyhow::anyhow!("列举锁失败: {e}"))?;
            for obj in page.contents() {
                if let Some(k) = obj.key() {
                    out.push(k.strip_prefix(&strip).unwrap_or(k).to_string());
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_imports)]
    use super::*;

    #[test]
    fn key_joining() {
        // 构造不发起网络请求，直接手工组装验证 key 拼接逻辑
        // （new 是 async，这里仅测私有逻辑的等价行为）
        assert_eq!(join_key("", "a/b.txt"), "a/b.txt");
        assert_eq!(join_key("pre", "a/b.txt"), "pre/a/b.txt");
        assert_eq!(join_key("pre", "/leading.txt"), "pre/leading.txt");
    }

    fn join_key(prefix: &str, rel: &str) -> String {
        let rel = rel.trim_start_matches('/');
        if prefix.is_empty() {
            rel.to_string()
        } else {
            format!("{prefix}/{rel}")
        }
    }
}
