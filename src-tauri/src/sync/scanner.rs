//! 本地文件扫描：递归遍历 + 排除规则 + xxh3-128 内容哈希（mtime/size 命中缓存则复用）。

use std::io::{BufReader, Read};
use std::path::Path;
use std::time::UNIX_EPOCH;
use globset::{GlobSet, GlobSetBuilder};
use twox_hash::xxhash3_128::Hasher as XxHash128;
use walkdir::WalkDir;

use crate::sync::state::StateDb;
use crate::sync::types::{FileMeta, FileTree, RelPath};

/// 内置排除规则：版本库/依赖目录、编辑器/系统临时文件、同步引擎自身元数据。
pub const BUILTIN_IGNORES: &[&str] = &[
    ".synccloud*/**",
    ".synccloud*",
    "*.synccloud-part",
    "*.sc-stage-*",
    ".git",
    ".git/**",
    "node_modules",
    "node_modules/**",
    "~$*",
    "*.tmp",
    "*.swp",
    "*.swo",
    "Thumbs.db",
    "desktop.ini",
    ".DS_Store",
    "*.crdownload",
    "*.part",
];

pub struct Scanner<'a> {
    ignore: GlobSet,
    db: Option<&'a StateDb>,
}

#[derive(Debug, Default, Clone)]
pub struct ScanStats {
    pub files: usize,
    pub dirs: usize,
    pub hashed: usize,
    pub hash_cache_hits: usize,
    pub ignored: usize,
}

impl<'a> Scanner<'a> {
    pub fn new(extra_rules: &[String], db: Option<&'a StateDb>) -> anyhow::Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pat in BUILTIN_IGNORES {
            builder.add(globset::Glob::new(pat)?);
        }
        for rule in extra_rules {
            let r = rule.trim();
            if r.is_empty() {
                continue;
            }
            // `node_modules/` 这类目录规则去掉尾斜杠后按目录名匹配，
            // 其下内容由逐前缀段检查覆盖。
            let r = r.trim_end_matches('/');
            if r.is_empty() {
                continue;
            }
            builder.add(globset::Glob::new(r)?);
        }
        Ok(Self {
            ignore: builder.build()?,
            db,
        })
    }

    /// 判断相对路径是否被排除：
    /// 1) 整体匹配（支持 `build/dist/*` 这类路径 glob）；
    /// 2) 纯名字规则（不含 `/` 的 pattern，如 `node_modules`、`*.iso`）
    ///    对任意层级的同名段生效（`web/node_modules/x.js` 也命中 `node_modules`）。
    fn is_ignored(&self, rel: &str, _is_dir: bool) -> bool {
        if self.ignore.is_match(rel) {
            return true;
        }
        rel.split('/').any(|part| self.ignore.is_match(part))
    }

    /// 扫描目录，返回相对路径（`/` 分隔）→ 元数据。
    pub fn scan(&self, root: &Path) -> anyhow::Result<(FileTree, ScanStats)> {
        let mut tree = FileTree::new();
        let mut stats = ScanStats::default();

        for entry in WalkDir::new(root)
            .follow_links(false)
            .min_depth(1)
            .into_iter()
            .filter_entry(|e| {
                // 过滤根下的隐藏系统目录由 ignore 规则负责；此处仅保证根本身不被过滤
                if e.depth() == 0 {
                    return true;
                }
                let rel = rel_path(root, e.path());
                if rel.is_empty() {
                    return true;
                }
                let ok = !self.is_ignored(&rel, e.file_type().is_dir());
                if !ok {
                    stats.ignored += 1;
                }
                ok
            })
        {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "walkdir 条目读取失败，跳过");
                    continue;
                }
            };
            if entry.file_type().is_dir() {
                stats.dirs += 1;
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = rel_path(root, entry.path());
            if rel.is_empty() || self.is_ignored(&rel, false) {
                continue;
            }
            let meta = match std::fs::metadata(entry.path()) {
                Ok(m) => m,
                Err(_) => continue, // 竞态：扫描期间文件被删除
            };
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);

            // 哈希缓存命中则免读内容
            let hash = self
                .db
                .and_then(|db| db.cached_hash(&rel, meta.len(), mtime_ms))
                .map(|h| {
                    stats.hash_cache_hits += 1;
                    h
                })
                .unwrap_or_else(|| {
                    stats.hashed += 1;
                    let h = hash_file(entry.path()).unwrap_or_else(|e| {
                        tracing::warn!(path = %entry.path().display(), error = %e, "哈希计算失败（独占锁定，极罕见），使用占位哈希待下轮重试");
                        format!("unreadable-{}-{}", meta.len(), mtime_ms)
                    });
                    if let Some(db) = self.db {
                        db.put_hash_cache(&rel, meta.len(), mtime_ms, &h);
                    }
                    h
                });

            stats.files += 1;
            tree.insert(
                rel,
                FileMeta {
                    size: meta.len(),
                    mtime_ms,
                    hash,
                },
            );
        }
        Ok((tree, stats))
    }
}

/// 计算文件 xxh3-128 哈希（hex）。流式读取，1MiB 缓冲。
/// 以共享模式打开：文件被其他程序占用（编辑器/杀毒）时也能读取，
/// 同步不再被“文件被另一进程使用”阻塞。
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let f = crate::sync::fslock::open_read_shared(path)?;
    let mut reader = BufReader::with_capacity(1024 * 1024, f);
    let mut hasher = XxHash128::default();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.write(&buf[..n]);
    }
    let h: u128 = hasher.finish_128();
    Ok(format!("{h:032x}"))
}

/// 本地路径 → 统一相对路径（`/` 分隔）。
pub fn rel_path(root: &Path, path: &Path) -> RelPath {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

/// 将统一相对路径转为本地路径。
pub fn abs_path(root: &Path, rel: &str) -> std::path::PathBuf {
    let mut p = root.to_path_buf();
    for seg in rel.split('/') {
        p.push(seg);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_ignore() {
        let s = Scanner::new(&[], None).unwrap();
        assert!(s.is_ignored("Thumbs.db", false));
        assert!(s.is_ignored("~$report.docx", false));
        assert!(s.is_ignored(".synccloud/manifest.json", false));
        assert!(!s.is_ignored("docs/report.docx", false));
    }

    #[test]
    fn test_user_ignore() {
        let rules: Vec<String> = ["node_modules", "*.iso", "build/"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let s = Scanner::new(&rules, None).unwrap();
        assert!(s.is_ignored("node_modules", true));
        assert!(s.is_ignored("web/node_modules/x.js", false));
        assert!(s.is_ignored("ubuntu.iso", false));
        assert!(s.is_ignored("build/output.js", false));
        assert!(!s.is_ignored("src/main.rs", false));
    }

    #[test]
    fn test_hash_file() {
        let dir = std::env::temp_dir().join("synccloud-test-hash");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.txt");
        std::fs::write(&p, b"hello synccloud").unwrap();
        let h1 = hash_file(&p).unwrap();
        let h2 = hash_file(&p).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 32, "xxh3-128 应为 32 个 hex 字符");
        std::fs::write(&p, b"changed").unwrap();
        assert_ne!(hash_file(&p).unwrap(), h1);
    }

    #[test]
    fn test_scan_tree() {
        let dir = std::env::temp_dir().join("synccloud-test-scan");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"aaa").unwrap();
        std::fs::write(dir.join("sub/b.txt"), b"bbb").unwrap();
        std::fs::write(dir.join("c.tmp"), b"temp").unwrap(); // 应被忽略

        let s = Scanner::new(&[], None).unwrap();
        let (tree, stats) = s.scan(&dir).unwrap();
        assert!(tree.contains_key("a.txt"));
        assert!(tree.contains_key("sub/b.txt"));
        assert!(!tree.contains_key("c.tmp"));
        assert_eq!(tree.len(), 2);
        assert_eq!(stats.files, 2);
        let keys: Vec<&String> = tree.keys().collect();
        assert!(!keys.iter().any(|k| k.contains('\\')));
    }
}
