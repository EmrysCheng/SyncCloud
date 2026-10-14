//! 三方对比引擎：本地树 / 远端 manifest / 本地基线 → 动作计划。
//! 决策矩阵参考 Dropbox/Syncthing 的双向同步语义。

use serde::Serialize;
use crate::sync::types::{ConflictPolicy, Direction, FileMeta, FileTree, Manifest, RelPath};

/// 单个待执行动作。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// 本地新增/修改 → 上传
    Upload { path: RelPath, meta: FileMeta },
    /// 远端新增/修改 → 下载
    Download { path: RelPath, meta: FileMeta },
    /// 本地已删且远端未变 → 远端删除
    DeleteRemote { path: RelPath },
    /// 远端已删且本地未变 → 本地删除
    DeleteLocal { path: RelPath },
    /// 双方内容一致（如两台机器创建了相同内容）→ 仅更新基线
    Identical { path: RelPath, meta: FileMeta },
    /// 双方都改且内容不同 → 冲突：
    /// 远端版本落为正式文件，本地修改另存为冲突副本后作为新文件上传
    Conflict { path: RelPath, local: FileMeta, remote: FileMeta },
    /// 本地已删但远端被他人修改 → 保留远端（重新下载），主流云盘同款行为
    RemoteWinsOverDelete { path: RelPath, remote: FileMeta },
    /// 远端已删但本地被修改 → 保留本地（重新上传）
    LocalWinsOverDelete { path: RelPath, local: FileMeta },
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStats {
    pub uploads: usize,
    pub downloads: usize,
    pub delete_local: usize,
    pub delete_remote: usize,
    pub conflicts: usize,
    pub identical: usize,
    pub unchanged: usize,
    pub bytes_up: u64,
    pub bytes_down: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPlan {
    pub actions: Vec<Action>,
    pub stats: PlanStats,
}

/// 判断"本地相对基线是否发生变化"。内容哈希是权威标准；
/// mtime 变了但哈希相同（touch / 重新保存相同内容）不算实质变化。
fn local_changed(cur: &FileMeta, base: Option<&FileMeta>) -> bool {
    match base {
        None => true,
        Some(b) => b.hash != cur.hash,
    }
}

fn remote_changed(cur: &FileMeta, base: Option<&FileMeta>) -> bool {
    match base {
        None => true,
        Some(b) => b.hash != cur.hash,
    }
}

/// 生成同步计划。三方对比核心，纯函数（表驱动测试覆盖全矩阵）。
pub fn compare(
    local: &FileTree,
    remote: Option<&Manifest>,
    baseline: &FileTree,
    direction: Direction,
    conflict_policy: ConflictPolicy,
) -> SyncPlan {
    let mut actions = Vec::new();
    let mut stats = PlanStats::default();
    let remote_files = remote.map(|m| &m.files);

    // 收集三方路径并集
    let mut all_paths: Vec<&RelPath> = local.keys().collect();
    if let Some(rf) = remote_files {
        for k in rf.keys() {
            if !local.contains_key(k) {
                all_paths.push(k);
            }
        }
    }
    // 基线中存在但双方都不在的路径（双删）无需处理，但也可能存在于 baseline-only
    for k in baseline.keys() {
        let in_local = local.contains_key(k);
        let in_remote = remote_files.map(|r| r.contains_key(k)).unwrap_or(false);
        if !in_local && !in_remote {
            all_paths.push(k);
        }
    }
    all_paths.sort();

    for path in all_paths {
        let l = local.get(path);
        let r = remote_files.and_then(|rf| rf.get(path));
        let b = baseline.get(path);

        match (l, r) {
            (Some(lm), Some(rm)) => {
                let lch = local_changed(lm, b);
                let rch = remote_changed(rm, b);
                match (lch, rch) {
                    (false, false) => {
                        // 双方均无实质变化
                        stats.unchanged += 1;
                        // mtime 可能漂移（touch），基线跟随当前本地值以稳定哈希缓存
                        if let Some(bm) = b {
                            if bm.mtime_ms != lm.mtime_ms {
                                actions.push(Action::Identical {
                                    path: path.clone(),
                                    meta: lm.clone(),
                                });
                            }
                        }
                    }
                    (true, false) => {
                        // 仅本地变
                        if direction == Direction::DownloadOnly {
                            stats.unchanged += 1;
                        } else {
                            stats.uploads += 1;
                            stats.bytes_up += lm.size;
                            actions.push(Action::Upload {
                                path: path.clone(),
                                meta: lm.clone(),
                            });
                        }
                    }
                    (false, true) => {
                        // 仅远端变
                        match direction {
                            Direction::UploadOnly | Direction::Contribute => {
                                stats.unchanged += 1;
                            }
                            Direction::Mirror => {
                                // 镜像模式以本地为绝对基准：本地未改但远端被改，强制用本地版本覆盖远端
                                stats.uploads += 1;
                                stats.bytes_up += lm.size;
                                actions.push(Action::Upload {
                                    path: path.clone(),
                                    meta: lm.clone(),
                                });
                            }
                            Direction::Bidirectional | Direction::DownloadOnly => {
                                stats.downloads += 1;
                                stats.bytes_down += rm.size;
                                actions.push(Action::Download {
                                    path: path.clone(),
                                    meta: rm.clone(),
                                });
                            }
                        }
                    }
                    (true, true) => {
                        if lm.hash == rm.hash {
                            // 双方改成了相同内容
                            stats.identical += 1;
                            actions.push(Action::Identical {
                                path: path.clone(),
                                meta: lm.clone(),
                            });
                        } else {
                            match direction {
                                Direction::Mirror | Direction::Contribute | Direction::UploadOnly => {
                                    // 单向/镜像/贡献模式下以本地修改为准
                                    stats.uploads += 1;
                                    stats.bytes_up += lm.size;
                                    actions.push(Action::Upload {
                                        path: path.clone(),
                                        meta: lm.clone(),
                                    });
                                }
                                Direction::DownloadOnly => {
                                    stats.downloads += 1;
                                    stats.bytes_down += rm.size;
                                    actions.push(Action::Download {
                                        path: path.clone(),
                                        meta: rm.clone(),
                                    });
                                }
                                Direction::Bidirectional => {
                                    match conflict_policy {
                                        ConflictPolicy::BothCopies => {
                                            stats.conflicts += 1;
                                            stats.downloads += 1;
                                            stats.bytes_down += rm.size;
                                            actions.push(Action::Conflict {
                                                path: path.clone(),
                                                local: lm.clone(),
                                                remote: rm.clone(),
                                            });
                                        }
                                        ConflictPolicy::NewestWins => {
                                            if lm.mtime_ms >= rm.mtime_ms {
                                                stats.uploads += 1;
                                                stats.bytes_up += lm.size;
                                                actions.push(Action::Upload {
                                                    path: path.clone(),
                                                    meta: lm.clone(),
                                                });
                                            } else {
                                                stats.downloads += 1;
                                                stats.bytes_down += rm.size;
                                                actions.push(Action::Download {
                                                    path: path.clone(),
                                                    meta: rm.clone(),
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            (Some(lm), None) => {
                // 本地有、远端无 = 远端被删除（相对基线）或本地全新创建
                match b {
                    None => {
                        // 全新文件
                        if direction == Direction::DownloadOnly {
                            stats.unchanged += 1;
                        } else {
                            stats.uploads += 1;
                            stats.bytes_up += lm.size;
                            actions.push(Action::Upload {
                                path: path.clone(),
                                meta: lm.clone(),
                            });
                        }
                    }
                    Some(_) => {
                        // 远端被删：
                        // 如果本地已改动：本地胜出重新上传
                        if local_changed(lm, b) {
                            stats.uploads += 1;
                            stats.bytes_up += lm.size;
                            actions.push(Action::LocalWinsOverDelete {
                                path: path.clone(),
                                local: lm.clone(),
                            });
                        } else {
                            match direction {
                                Direction::Mirror | Direction::Contribute | Direction::UploadOnly => {
                                    // 镜像与单向模式下本地文件不可被轻易抹除，重新上报为上传以恢复云端
                                    stats.uploads += 1;
                                    stats.bytes_up += lm.size;
                                    actions.push(Action::Upload {
                                        path: path.clone(),
                                        meta: lm.clone(),
                                    });
                                }
                                Direction::Bidirectional | Direction::DownloadOnly => {
                                    stats.delete_local += 1;
                                    actions.push(Action::DeleteLocal { path: path.clone() });
                                }
                            }
                        }
                    }
                }
            }
            (None, Some(rm)) => {
                // 本地无、远端有 = 本地被删除（相对基线）或远端新文件
                match b {
                    None => {
                        // 远端新文件
                        match direction {
                            Direction::Mirror => {
                                // 镜像模式：本地没有的多余文件从远端清理，保持完全镜像
                                stats.delete_remote += 1;
                                actions.push(Action::DeleteRemote { path: path.clone() });
                            }
                            Direction::Contribute | Direction::UploadOnly => {
                                stats.unchanged += 1;
                            }
                            Direction::Bidirectional | Direction::DownloadOnly => {
                                stats.downloads += 1;
                                stats.bytes_down += rm.size;
                                actions.push(Action::Download {
                                    path: path.clone(),
                                    meta: rm.clone(),
                                });
                            }
                        }
                    }
                    Some(_) => {
                        // 本地被删：
                        match direction {
                            Direction::Contribute => {
                                // 贡献安全模式：核心特性为永不删除远端任何历史文件
                                stats.unchanged += 1;
                            }
                            Direction::DownloadOnly => {
                                stats.unchanged += 1;
                            }
                            Direction::Mirror => {
                                // 镜像模式：本地删则远端坚决删
                                stats.delete_remote += 1;
                                actions.push(Action::DeleteRemote { path: path.clone() });
                            }
                            Direction::UploadOnly => {
                                stats.delete_remote += 1;
                                actions.push(Action::DeleteRemote { path: path.clone() });
                            }
                            Direction::Bidirectional => {
                                if remote_changed(rm, b) {
                                    // 远端已被修改，保留他人修改
                                    stats.downloads += 1;
                                    stats.bytes_down += rm.size;
                                    actions.push(Action::RemoteWinsOverDelete {
                                        path: path.clone(),
                                        remote: rm.clone(),
                                    });
                                } else {
                                    stats.delete_remote += 1;
                                    actions.push(Action::DeleteRemote { path: path.clone() });
                                }
                            }
                        }
                    }
                }
            }
            (None, None) => {
                // 双方都已删除：基线清理即可，无需动作（替换基线时自然消失）
            }
        }
    }

    SyncPlan { actions, stats }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(hash: &str, size: u64, mtime: i64) -> FileMeta {
        FileMeta {
            size,
            mtime_ms: mtime,
            hash: hash.into(),
        }
    }

    fn tree(entries: &[(&str, FileMeta)]) -> FileTree {
        entries
            .iter()
            .map(|(p, m)| (p.to_string(), m.clone()))
            .collect()
    }

    fn manifest(tree: FileTree) -> Manifest {
        let mut m = Manifest::new("RemoteDevice");
        m.files = tree;
        m
    }

    fn plan_with(local: FileTree, remote: Option<Manifest>, base: FileTree) -> SyncPlan {
        compare(
            &local,
            remote.as_ref(),
            &base,
            Direction::Bidirectional,
            ConflictPolicy::BothCopies,
        )
    }

    fn kinds(p: &SyncPlan) -> Vec<String> {
        p.actions
            .iter()
            .map(|a| match a {
                Action::Upload { .. } => "upload",
                Action::Download { .. } => "download",
                Action::DeleteRemote { .. } => "del_remote",
                Action::DeleteLocal { .. } => "del_local",
                Action::Identical { .. } => "identical",
                Action::Conflict { .. } => "conflict",
                Action::RemoteWinsOverDelete { .. } => "remote_wins",
                Action::LocalWinsOverDelete { .. } => "local_wins",
            })
            .map(String::from)
            .collect()
    }

    // ---- 全矩阵表驱动测试 ----

    #[test]
    fn everything_in_sync() {
        let t = tree(&[("a.txt", meta("h1", 1, 100))]);
        let p = plan_with(t.clone(), Some(manifest(t.clone())), t);
        assert!(p.actions.is_empty());
        assert_eq!(p.stats.unchanged, 1);
    }

    #[test]
    fn local_new_file_upload() {
        let p = plan_with(
            tree(&[("new.txt", meta("h1", 1, 100))]),
            None,
            tree(&[]),
        );
        assert_eq!(kinds(&p), ["upload"]);
    }

    #[test]
    fn remote_new_file_download() {
        let p = plan_with(
            tree(&[]),
            Some(manifest(tree(&[("new.txt", meta("h1", 1, 100))]))),
            tree(&[]),
        );
        assert_eq!(kinds(&p), ["download"]);
    }

    #[test]
    fn local_modified_upload() {
        let base = tree(&[("a.txt", meta("h0", 1, 100))]);
        let remote = manifest(base.clone());
        let local = tree(&[("a.txt", meta("h1", 2, 200))]);
        let p = plan_with(local, Some(remote), base);
        assert_eq!(kinds(&p), ["upload"]);
    }

    #[test]
    fn remote_modified_download() {
        let base = tree(&[("a.txt", meta("h0", 1, 100))]);
        let remote = manifest(tree(&[("a.txt", meta("h2", 3, 300))]));
        let p = plan_with(base.clone(), Some(remote), base);
        assert_eq!(kinds(&p), ["download"]);
    }

    #[test]
    fn both_modified_same_content_identical() {
        let base = tree(&[("a.txt", meta("h0", 1, 100))]);
        let remote = manifest(tree(&[("a.txt", meta("h9", 5, 500))]));
        let local = tree(&[("a.txt", meta("h9", 5, 400))]);
        let p = plan_with(local, Some(remote), base);
        assert_eq!(kinds(&p), ["identical"]);
        assert_eq!(p.stats.identical, 1);
    }

    #[test]
    fn both_modified_different_conflict() {
        let base = tree(&[("a.txt", meta("h0", 1, 100))]);
        let remote = manifest(tree(&[("a.txt", meta("hr", 3, 300))]));
        let local = tree(&[("a.txt", meta("hl", 2, 200))]);
        let p = plan_with(local, Some(remote), base);
        assert_eq!(kinds(&p), ["conflict"]);
    }

    #[test]
    fn conflict_newest_wins_local() {
        let base = tree(&[("a.txt", meta("h0", 1, 100))]);
        let remote = manifest(tree(&[("a.txt", meta("hr", 3, 300))]));
        let local = tree(&[("a.txt", meta("hl", 2, 9000))]);
        let p = compare(&local, Some(remote).as_ref(), &base, Direction::Bidirectional, ConflictPolicy::NewestWins);
        assert_eq!(kinds(&p), ["upload"]);
    }

    #[test]
    fn conflict_newest_wins_remote() {
        let base = tree(&[("a.txt", meta("h0", 1, 100))]);
        let remote = manifest(tree(&[("a.txt", meta("hr", 3, 9000))]));
        let local = tree(&[("a.txt", meta("hl", 2, 200))]);
        let p = compare(&local, Some(remote).as_ref(), &base, Direction::Bidirectional, ConflictPolicy::NewestWins);
        assert_eq!(kinds(&p), ["download"]);
    }

    #[test]
    fn local_deleted_remote_unchanged_deletes_remote() {
        let base = tree(&[("a.txt", meta("h1", 1, 100))]);
        let remote = manifest(base.clone());
        let p = plan_with(tree(&[]), Some(remote), base);
        assert_eq!(kinds(&p), ["del_remote"]);
    }

    #[test]
    fn local_deleted_remote_modified_remote_wins() {
        let base = tree(&[("a.txt", meta("h0", 1, 100))]);
        let remote = manifest(tree(&[("a.txt", meta("hr", 3, 300))]));
        let p = plan_with(tree(&[]), Some(remote), base);
        assert_eq!(kinds(&p), ["remote_wins"]);
    }

    #[test]
    fn remote_deleted_local_unchanged_deletes_local() {
        let base = tree(&[("a.txt", meta("h1", 1, 100))]);
        let local = base.clone();
        let p = plan_with(local, Some(manifest(tree(&[]))), base);
        assert_eq!(kinds(&p), ["del_local"]);
    }

    #[test]
    fn remote_deleted_local_modified_local_wins() {
        let base = tree(&[("a.txt", meta("h0", 1, 100))]);
        let local = tree(&[("a.txt", meta("hl", 2, 200))]);
        let p = plan_with(local, Some(manifest(tree(&[]))), base);
        assert_eq!(kinds(&p), ["local_wins"]);
    }

    #[test]
    fn both_deleted_no_action() {
        let base = tree(&[("a.txt", meta("h1", 1, 100))]);
        let p = plan_with(tree(&[]), Some(manifest(tree(&[]))), base);
        assert!(p.actions.is_empty());
    }

    #[test]
    fn touch_only_no_real_change() {
        // mtime 变化但内容相同：不触发上传
        let base = tree(&[("a.txt", meta("h1", 1, 100))]);
        let remote = manifest(base.clone());
        let local = tree(&[("a.txt", meta("h1", 1, 999))]);
        let p = plan_with(local, Some(remote), base);
        assert_eq!(kinds(&p), ["identical"]); // 仅基线 mtime 跟随
    }

    #[test]
    fn direction_download_only_blocks_upload() {
        let base = tree(&[("a.txt", meta("h0", 1, 100))]);
        let remote = manifest(base.clone());
        let local = tree(&[("a.txt", meta("h1", 2, 200))]);
        let p = compare(&local, Some(remote).as_ref(), &base, Direction::DownloadOnly, ConflictPolicy::BothCopies);
        assert!(p.actions.is_empty());
    }

    #[test]
    fn direction_upload_only_blocks_download() {
        let base = tree(&[("a.txt", meta("h0", 1, 100))]);
        let remote = manifest(tree(&[("a.txt", meta("h2", 3, 300))]));
        let p = compare(&base, Some(remote).as_ref(), &base, Direction::UploadOnly, ConflictPolicy::BothCopies);
        assert!(p.actions.is_empty());
    }

    #[test]
    fn direction_contribute_never_deletes_remote() {
        let base = tree(&[("a.txt", meta("h0", 1, 100))]);
        let remote = manifest(base.clone());
        // 本地删除了 a.txt
        let local = tree(&[]);
        let p = compare(
            &local,
            Some(remote).as_ref(),
            &base,
            Direction::Contribute,
            ConflictPolicy::BothCopies,
        );
        // 增量贡献模式下，绝不删除远端
        assert!(p.actions.is_empty());
        assert_eq!(p.stats.delete_remote, 0);
    }

    #[test]
    fn direction_mirror_removes_untracked_remote() {
        let base = tree(&[]);
        // 远端存在新文件，但本地没有
        let remote = manifest(tree(&[("extra.txt", meta("hx", 1, 100))]));
        let local = tree(&[]);
        let p = compare(
            &local,
            Some(remote).as_ref(),
            &base,
            Direction::Mirror,
            ConflictPolicy::BothCopies,
        );
        // 镜像模式下，远端多余文件必须删除以完全保持与本地镜像
        assert_eq!(kinds(&p), ["del_remote"]);
        assert_eq!(p.stats.delete_remote, 1);
    }

    #[test]
    fn multi_file_mixed_actions() {
        let base = tree(&[
            ("keep.txt", meta("k", 1, 1)),
            ("local-edit.txt", meta("e0", 1, 1)),
            ("local-del.txt", meta("d0", 1, 1)),
            ("remote-del.txt", meta("r0", 1, 1)),
        ]);
        let remote = manifest(tree(&[
            ("keep.txt", meta("k", 1, 1)),
            ("local-edit.txt", meta("e0", 1, 1)),
            ("local-del.txt", meta("d0", 1, 1)),
            // remote-del.txt 远端已删，本地未变 → del_local
        ]));
        let local = tree(&[
            ("keep.txt", meta("k", 1, 1)),
            ("local-edit.txt", meta("e1", 2, 2)), // 本地改 → upload
            ("local-del.txt", meta("d0", 1, 1)),  // 仍在本地
            ("remote-del.txt", meta("r0", 1, 1)),
            ("brand-new.txt", meta("n", 5, 5)),   // 新文件 → upload
        ]);
        let p = plan_with(local, Some(remote), base);
        let k = kinds(&p);
        assert!(k.contains(&"upload".into()));
        assert!(k.contains(&"del_local".into()));
        assert_eq!(p.stats.uploads, 2);
        assert_eq!(p.stats.delete_local, 1);
        assert_eq!(p.stats.unchanged, 2);
    }
}
