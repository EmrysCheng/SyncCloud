//! Git 后端：任意 Git 远端（GitHub / Gitee / Gitea / 自建服务器）作为同步存储。
//! 实现：本地缓存克隆 + 暂存区。
//! - `put_file` 先复制到暂存目录（不动 git 工作区）
//! - `write_manifest` 时：强制对齐工作区到远端最新 → 应用暂存 → 提交 → push
//! - manifest 与锁存放在独立 `synccloud-meta/<分支>` ref 上（不进同步分支，
//!   用户在仓库文件列表里看不到任何引擎文件）
//! - 版本历史 = 提交历史；push 被拒（他机新提交）时本轮报错，下轮 fetch 后重试
//! 依赖系统 git 命令行；https 远端将用户名+令牌嵌入 URL，SSH 使用用户已有密钥。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::process::Command;

use super::{RemoteEntry, RemoteStore};
use crate::sync::types::Manifest;

pub struct GitStore {
    remote_url: String,
    branch: String,
    username: String,
    password: String,
    /// 本地缓存克隆目录（由引擎注入，位于文件夹数据目录下）
    cache_dir: PathBuf,
    /// 暂存目录：待提交的文件副本
    stage_dir: PathBuf,
    /// 元数据暂存目录：待写入 meta ref 的锁文件副本
    meta_stage_dir: PathBuf,
    /// 暂存的删除清单
    pending_deletes: tokio::sync::Mutex<HashSet<String>>,
    /// 待从 meta ref 移除的键（锁删除）
    pending_meta_deletes: tokio::sync::Mutex<HashSet<String>>,
}

impl GitStore {
    pub fn new(remote_url: String, branch: String, username: String, password: String) -> Self {
        let base = std::env::temp_dir().join(format!("synccloud-git-{}", uuid::Uuid::new_v4()));
        Self {
            remote_url,
            branch,
            username,
            password,
            cache_dir: base.join("cache"),
            stage_dir: base.join("stage"),
            meta_stage_dir: base.join("meta-stage"),
            pending_deletes: Default::default(),
            pending_meta_deletes: Default::default(),
        }
    }

    /// 引擎注入缓存目录（`<folder-data>/git-cache`）。
    pub fn with_cache_dir(mut self, dir: PathBuf) -> Self {
        self.stage_dir = dir.join("git-stage");
        self.cache_dir = dir.join("git-cache");
        self.meta_stage_dir = dir.join("git-meta-stage");
        self
    }

    /// https 远端且提供了凭证时，将凭证嵌入 URL。
    fn authed_url(&self) -> String {
        if !self.remote_url.starts_with("http") || self.username.is_empty() || self.password.is_empty() {
            return self.remote_url.clone();
        }
        let user = percent_encoding::utf8_percent_encode(&self.username, percent_encoding::NON_ALPHANUMERIC);
        let pass = percent_encoding::utf8_percent_encode(&self.password, percent_encoding::NON_ALPHANUMERIC);
        self.remote_url
            .replacen("https://", &format!("https://{user}:{pass}@"), 1)
            .replacen("http://", &format!("http://{user}:{pass}@"), 1)
    }

    fn ref_of(&self) -> String {
        format!("refs/remotes/origin/{}", self.branch)
    }

    /// 元数据分支（manifest + 锁），与同步分支并行、永不进同步分支的文件树。
    fn meta_branch(&self) -> String {
        format!("synccloud-meta/{}", self.branch)
    }

    fn meta_ref_of(&self) -> String {
        format!("refs/remotes/origin/{}", self.meta_branch())
    }

    async fn git(&self, args: &[&str]) -> anyhow::Result<String> {
        let output = Command::new("git")
            .args(["-C"])
            .arg(&self.cache_dir)
            .args([
                "-c", "core.autocrlf=false",
                "-c", "core.safecrlf=false",
                "-c", "user.name=SyncCloud",
                "-c", "user.email=synccloud@device",
            ])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("无法启动 git（请确认已安装并在 PATH 中）: {e}"))?;
        if !output.status.success() {
            anyhow::bail!(
                "git {} 失败: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    async fn ensure_cache(&self) -> anyhow::Result<()> {
        if self.cache_dir.join(".git").exists() {
            return Ok(());
        }
        if let Some(parent) = self.cache_dir.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let url = self.authed_url();
        // 尝试克隆指定分支；远端为空仓库或分支不存在时退化为全量克隆后建分支
        let clone = Command::new("git")
            .args(["clone", "--no-checkout", "--branch", &self.branch, &url])
            .arg(&self.cache_dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await;
        match clone {
            Ok(out) if out.status.success() => {
                self.git(&["checkout", &self.branch]).await?;
                Ok(())
            }
            _ => {
                let _ = tokio::fs::remove_dir_all(&self.cache_dir).await;
                let out = Command::new("git")
                    .args(["clone", "--no-checkout", &url])
                    .arg(&self.cache_dir)
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .output()
                    .await
                    .map_err(|e| anyhow::anyhow!("无法启动 git: {e}"))?;
                if !out.status.success() {
                    anyhow::bail!("克隆仓库失败: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
                self.git(&["checkout", "-B", &self.branch]).await?;
                Ok(())
            }
        }
    }

    /// fetch 远端分支到 refs/remotes/origin/<branch>。
    async fn fetch(&self) -> anyhow::Result<()> {
        self.ensure_cache().await?;
        let url = self.authed_url();
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.cache_dir)
            .args(["fetch", "--force", &url, &format!("{}:{}", self.branch, self.ref_of())])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("无法启动 git: {e}"))?;
        // 远端分支尚不存在（新仓库）不是错误
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.contains("couldn't find remote ref") {
                anyhow::bail!("git fetch 失败: {}", err.trim());
            }
        }
        Ok(())
    }

    /// fetch 元数据分支。分支不存在（首次使用）不是错误。
    async fn fetch_meta(&self) -> anyhow::Result<()> {
        self.ensure_cache().await?;
        let url = self.authed_url();
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.cache_dir)
            .args(["fetch", "--force", &url, &format!("{}:{}", self.meta_branch(), self.meta_ref_of())])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("无法启动 git: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.contains("couldn't find remote ref") {
                anyhow::bail!("git fetch 元数据分支失败: {}", err.trim());
            }
        }
        Ok(())
    }

    /// meta ref 是否存在（远端）。
    async fn has_meta_ref(&self) -> anyhow::Result<bool> {
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.cache_dir)
            .args(["rev-parse", "--verify", &self.meta_ref_of()])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await?;
        Ok(out.status.success())
    }

    /// 用索引管线（read-tree/update-index/write-tree/commit-tree）构建并推送
    /// 元数据分支：manifest.json + 暂存的锁文件，不触碰同步分支的工作区。
    async fn push_meta(&self, manifest: &Manifest) -> anyhow::Result<()> {
        self.fetch_meta().await?;
        let has_meta = self.has_meta_ref().await?;
        let meta_ref = self.meta_ref_of();
        if has_meta {
            self.git(&["read-tree", &meta_ref]).await?;
        } else {
            self.git(&["read-tree", "--empty"]).await?;
        }
        // manifest blob
        let tmp = std::env::temp_dir()
            .join(format!("synccloud-git-meta-{}.json", uuid::Uuid::new_v4()));
        tokio::fs::write(&tmp, serde_json::to_vec_pretty(manifest)?).await?;
        let tmp_str = tmp.to_string_lossy().to_string();
        let hash = self.git(&["hash-object", "-w", &tmp_str]).await?;
        let _ = tokio::fs::remove_file(&tmp).await;
        let hash = hash.trim();
        self.git(&["update-index", "--add", "--cacheinfo", &format!("100644,{hash},manifest.json")])
            .await?;
        // 应用暂存的锁文件
        if self.meta_stage_dir.exists() {
            let mut stack = vec![self.meta_stage_dir.clone()];
            while let Some(dir) = stack.pop() {
                let mut it = tokio::fs::read_dir(&dir).await?;
                while let Some(entry) = it.next_entry().await? {
                    let p = entry.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if let Ok(rel) = p.strip_prefix(&self.meta_stage_dir) {
                        let rel_key = rel.to_string_lossy().replace('\\', "/");
                        let ps = p.to_string_lossy().to_string();
                        let h = self.git(&["hash-object", "-w", &ps]).await?;
                        self.git(&["update-index", "--add", "--cacheinfo", &format!("100644,{},{}", h.trim(), rel_key)])
                            .await?;
                    }
                }
            }
            let _ = tokio::fs::remove_dir_all(&self.meta_stage_dir).await;
        }
        // 应用待删除的锁
        {
            let mut dels = self.pending_meta_deletes.lock().await;
            for rel in dels.iter() {
                let _ = self.git(&["update-index", "--force-remove", rel]).await;
            }
            dels.clear();
        }
        let tree = self.git(&["write-tree"]).await?;
        let tree = tree.trim();
        let msg = format!("meta: {} [{}] files={} seq={}", manifest.device, manifest.updated_at, manifest.files.len(), manifest.seq);
        let commit = if has_meta {
            let parent = self.git(&["rev-parse", &meta_ref]).await?;
            self.git(&["commit-tree", tree, "-p", parent.trim(), "-m", &msg]).await?
        } else {
            self.git(&["commit-tree", tree, "-m", &msg]).await?
        };
        let url = self.authed_url();
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.cache_dir)
            .args(["push", &url, &format!("{}:refs/heads/{}", commit.trim(), self.meta_branch())])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).to_string();
            if err.contains("non-fast-forward") || err.contains("rejected") {
                anyhow::bail!("git push 元数据被拒绝（另一设备刚更新），本轮同步将重试");
            }
            anyhow::bail!("git push 元数据分支失败: {}", err.trim());
        }
        Ok(())
    }

    async fn show_to_file(&self, git_path: &str, dest: &Path) -> anyhow::Result<()> {
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.cache_dir)
            .args(["cat-file", "blob", &format!("{}:{}", self.ref_of(), git_path)])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await?;
        if !out.status.success() {
            anyhow::bail!("读取 {git_path} 失败: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(dest, &out.stdout).await?;
        Ok(())
    }

    async fn show_to_string(&self, git_path: &str) -> anyhow::Result<Option<String>> {
        self.show_ref_path(&self.ref_of(), git_path).await
    }

    async fn show_to_string_meta(&self, git_path: &str) -> anyhow::Result<Option<String>> {
        self.show_ref_path(&self.meta_ref_of(), git_path).await
    }

    async fn show_ref_path(&self, git_ref: &str, git_path: &str) -> anyhow::Result<Option<String>> {
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.cache_dir)
            .args(["show", &format!("{}:{}", git_ref, git_path)])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await?;
        if out.status.success() {
            Ok(Some(String::from_utf8_lossy(&out.stdout).to_string()))
        } else {
            // 分支未创建 / 路径缺失等一律视为不存在
            Ok(None)
        }
    }

    /// 工作区强制对齐到远端分支最新（丢弃本地未提交修改，untracked 保留）。
    async fn align_worktree(&self) -> anyhow::Result<()> {
        let has_remote = Command::new("git")
            .args(["-C"])
            .arg(&self.cache_dir)
            .args(["rev-parse", "--verify", &self.ref_of()])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await?
            .status
            .success();
        if has_remote {
            self.git(&["checkout", "-f", "-B", &self.branch, &self.ref_of()]).await?;
        } else {
            self.git(&["checkout", "-B", &self.branch]).await?;
        }
        Ok(())
    }

    async fn commit_and_push(&self, manifest: &Manifest) -> anyhow::Result<()> {
        self.git(&["add", "-A"]).await?;
        let status = self.git(&["status", "--porcelain"]).await?;
        let msg = format!("sync: {} [{}] files={} seq={}", manifest.device, manifest.updated_at, manifest.files.len(), manifest.seq);
        if !status.trim().is_empty() {
            self.git(&["commit", "-m", &msg]).await?;
        } else {
            return Ok(()); // 无变更无需推送
        }
        let url = self.authed_url();
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.cache_dir)
            .args(["push", &url, &format!("{}:{}", self.branch, self.branch)])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).to_string();
            if err.contains("non-fast-forward") || err.contains("rejected") {
                anyhow::bail!("git push 被拒绝（另一设备刚推送了新提交），本轮同步将重试");
            }
            anyhow::bail!("git push 失败: {}", err.trim());
        }
        Ok(())
    }
}

fn cache_rel(base: &Path, rel: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    for seg in rel.split('/') {
        p.push(seg);
    }
    p
}

#[async_trait]
impl RemoteStore for GitStore {
    fn kind(&self) -> &'static str {
        "git"
    }

    fn raw_io(&self) -> &dyn super::RawIo {
        self
    }

    async fn test(&self) -> anyhow::Result<()> {
        let url = self.authed_url();
        let out = Command::new("git")
            .args(["ls-remote", "--heads", &url])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("无法启动 git（请确认已安装）: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "Git 远端连接失败: {}（检查地址与访问令牌）",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    async fn read_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        self.fetch_meta().await?;
        match self.show_to_string_meta("manifest.json").await? {
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            None => Ok(None),
        }
    }

    async fn read_legacy_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        self.fetch().await?;
        match self.show_to_string(".synccloud/manifest.json").await? {
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            None => Ok(None),
        }
    }

    async fn cleanup_legacy_meta(&self) -> anyhow::Result<()> {
        // 从工作区删除旧版元数据；随下次提交从同步分支移除
        let _ = tokio::fs::remove_dir_all(cache_rel(&self.cache_dir, ".synccloud")).await;
        Ok(())
    }

    async fn write_manifest(&self, manifest: &Manifest) -> anyhow::Result<()> {
        self.fetch().await?;
        self.align_worktree().await?;

        // 应用暂存的上传
        let mut applied = 0u32;
        if self.stage_dir.exists() {
            let mut stack = vec![self.stage_dir.clone()];
            while let Some(dir) = stack.pop() {
                let mut it = tokio::fs::read_dir(&dir).await?;
                while let Some(entry) = it.next_entry().await? {
                    let p = entry.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if let Ok(rel) = p.strip_prefix(&self.stage_dir) {
                        let rel_key = rel.to_string_lossy().replace('\\', "/");
                        let dest = cache_rel(&self.cache_dir, &rel_key);
                        if let Some(parent) = dest.parent() {
                            tokio::fs::create_dir_all(parent).await?;
                        }
                        tokio::fs::copy(&p, &dest).await?;
                        applied += 1;
                    }
                }
            }
            let _ = tokio::fs::remove_dir_all(&self.stage_dir).await;
        }
        // 应用暂存的删除
        {
            let mut dels = self.pending_deletes.lock().await;
            for rel in dels.iter() {
                let target = cache_rel(&self.cache_dir, rel);
                let _ = tokio::fs::remove_file(&target).await;
            }
            dels.clear();
        }
        // 清掉工作区可能遗留的旧版元数据目录（随本次提交从同步分支移除）
        let _ = tokio::fs::remove_dir_all(cache_rel(&self.cache_dir, ".synccloud")).await;

        self.commit_and_push(manifest).await?;
        // manifest 本体写入独立元数据分支
        self.push_meta(manifest).await?;
        let _ = applied;
        Ok(())
    }

    async fn put_file(&self, path: &str, src: &Path) -> anyhow::Result<()> {
        let dest = cache_rel(&self.stage_dir, path);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::copy(src, &dest).await?;
        Ok(())
    }

    async fn get_file(&self, path: &str, dest: &Path) -> anyhow::Result<()> {
        self.fetch().await?;
        self.show_to_file(path, dest).await
    }

    async fn delete_file(&self, path: &str) -> anyhow::Result<()> {
        self.pending_deletes.lock().await.insert(path.to_string());
        Ok(())
    }

    async fn list_objects(&self) -> anyhow::Result<Vec<RemoteEntry>> {
        self.fetch().await?;
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.cache_dir)
            .args(["ls-tree", "-r", "--format=%(objectsize)%09%(path)", &self.ref_of()])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await?;
        let mut items = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some((size, path)) = line.split_once('\t') {
                if path.starts_with(".synccloud/") {
                    continue;
                }
                items.push(RemoteEntry {
                    key: path.to_string(),
                    size: size.parse().unwrap_or(0),
                });
            }
        }
        Ok(items)
    }
}

/// 锁 = 元数据分支上的普通文件（push 原子性兜底跨机竞态）；
/// 写入先落本地 meta 暂存目录，随下一次 write_manifest 推送到 meta ref。
#[async_trait]
impl super::RawIo for GitStore {
    async fn put_text(&self, key: &str, content: &str) -> anyhow::Result<()> {
        self.ensure_cache().await?;
        let p = cache_rel(&self.meta_stage_dir, key);
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&p, content).await?;
        Ok(())
    }

    async fn read_text(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.fetch_meta().await?;
        self.show_to_string_meta(key).await
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.pending_meta_deletes.lock().await.insert(key.to_string());
        Ok(())
    }

    async fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        self.fetch_meta().await?;
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.cache_dir)
            .args(["ls-tree", "-r", "--name-only", &self.meta_ref_of()])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.starts_with(prefix))
            .map(String::from)
            .collect())
    }
}
