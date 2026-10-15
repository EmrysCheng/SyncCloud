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
    /// 本地重命名/移动（对标 FreeFileSync 与 GoodSync，零网络下载）
    MoveLocal { from: RelPath, to: RelPath, meta: FileMeta },
    /// 远端重命名/移动（服务端移动或增量覆盖，避免重新上传）
    MoveRemote { from: RelPath, to: RelPath, meta: FileMeta },
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
    #[serde(default)]
    pub moved: usize,
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
    compare_with_ignore(local, remote, baseline, direction, conflict_policy, &|_| false)
}

/// 支持路径排除判断的三方对比。
pub fn compare_with_ignore(
    local: &FileTree,
    remote: Option<&Manifest>,
    baseline: &FileTree,
    direction: Direction,
    conflict_policy: ConflictPolicy,
    is_ignored: &dyn Fn(&str) -> bool,
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
                // 若该路径命中排除规则（例如隐藏文件开启 ignore_hidden 或匹配用户规则）：
                if is_ignored(path) {
                    match b {
                        Some(_) => {
                            // 基线中曾经有，现在本地已删除或被用户忽略：
                            // 双向/镜像/仅上传模式下，推动删除存储后端中的残留文件，绝不能逆向拉取到本地！
                            match direction {
                                Direction::Bidirectional | Direction::Mirror | Direction::UploadOnly => {
                                    stats.delete_remote += 1;
                                    actions.push(Action::DeleteRemote { path: path.clone() });
                                }
                                _ => {
                                    stats.unchanged += 1;
                                }
                            }
                        }
                        None => {
                            // 远端存在但本地已配置排除且基线无记录：直接跳过，绝不下载拉取到本地
                            stats.unchanged += 1;
                        }
                    }
                    continue;
                }

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

    // 移动与重命名感知检测（对标 FreeFileSync 与 GoodSync）
    detect_moves(
        &mut actions,
        &mut stats,
        local,
        remote_files,
        baseline,
        direction,
    );

    SyncPlan { actions, stats }
}

/// 移动与重命名感知检测（对标 FreeFileSync 与 GoodSync）。
/// 当一侧存在删除、另一侧存在新增，且内容哈希和大小一致时，
/// 将其智能升格为移动/重命名动作，免去全量重新上传或下载。
fn detect_moves(
    actions: &mut Vec<Action>,
    stats: &mut PlanStats,
    local: &FileTree,
    _remote: Option<&FileTree>,
    baseline: &FileTree,
    direction: Direction,
) {
    if direction == Direction::Contribute {
        // Contribute 模式只增不删，不参与移动消除
        return;
    }

    // 1. 探测远端移动/重命名 -> 升格为本地 MoveLocal（免网络下载）
    if direction == Direction::Bidirectional || direction == Direction::DownloadOnly {
        let mut del_locals: Vec<(usize, RelPath)> = Vec::new();
        let mut downloads: Vec<(usize, RelPath, FileMeta)> = Vec::new();

        for (idx, action) in actions.iter().enumerate() {
            match action {
                Action::DeleteLocal { path } => {
                    del_locals.push((idx, path.clone()));
                }
                Action::Download { path, meta } if meta.size > 0 && !meta.hash.is_empty() => {
                    downloads.push((idx, path.clone(), meta.clone()));
                }
                _ => {}
            }
        }

        let mut matched_del = std::collections::HashSet::new();
        let mut matched_down = std::collections::HashSet::new();
        let mut new_moves = Vec::new();

        for &(down_idx, ref to_path, ref rm) in &downloads {
            // 匹配本地仍存有该内容（待删）的原文件
            let candidate = del_locals.iter().find(|(del_idx, from_path)| {
                !matched_del.contains(del_idx)
                    && local.get(from_path).map(|lm| lm.hash == rm.hash).unwrap_or(false)
            });

            if let Some(&(del_idx, ref from_path)) = candidate {
                matched_del.insert(del_idx);
                matched_down.insert(down_idx);
                new_moves.push((
                    del_idx,
                    down_idx,
                    Action::MoveLocal {
                        from: from_path.clone(),
                        to: to_path.clone(),
                        meta: rm.clone(),
                    },
                    rm.size,
                ));
            }
        }

        if !new_moves.is_empty() {
            let mut remove_indices: Vec<usize> = Vec::new();
            for (del_idx, down_idx, move_action, size) in new_moves {
                remove_indices.push(del_idx);
                remove_indices.push(down_idx);
                actions.push(move_action);
                stats.delete_local = stats.delete_local.saturating_sub(1);
                stats.downloads = stats.downloads.saturating_sub(1);
                stats.bytes_down = stats.bytes_down.saturating_sub(size);
                stats.moved += 1;
            }
            remove_indices.sort_unstable();
            for idx in remove_indices.into_iter().rev() {
                actions.remove(idx);
            }
        }
    }

    // 2. 探测本地移动/重命名 -> 升格为远端 MoveRemote（免全量重新上传）
    if direction == Direction::Bidirectional || direction == Direction::Mirror || direction == Direction::UploadOnly {
        let mut del_remotes: Vec<(usize, RelPath)> = Vec::new();
        let mut uploads: Vec<(usize, RelPath, FileMeta)> = Vec::new();

        for (idx, action) in actions.iter().enumerate() {
            match action {
                Action::DeleteRemote { path } => {
                    del_remotes.push((idx, path.clone()));
                }
                Action::Upload { path, meta } if meta.size > 0 && !meta.hash.is_empty() => {
                    uploads.push((idx, path.clone(), meta.clone()));
                }
                _ => {}
            }
        }

        let mut matched_del = std::collections::HashSet::new();
        let mut matched_up = std::collections::HashSet::new();
        let mut new_moves = Vec::new();

        for &(up_idx, ref to_path, ref lm) in &uploads {
            // 匹配基线中记录的原内容且本地已删的远端项
            let candidate = del_remotes.iter().find(|(del_idx, from_path)| {
                !matched_del.contains(del_idx)
                    && baseline.get(from_path).map(|bm| bm.hash == lm.hash).unwrap_or(false)
            });

            if let Some(&(del_idx, ref from_path)) = candidate {
                matched_del.insert(del_idx);
                matched_up.insert(up_idx);
                new_moves.push((
                    del_idx,
                    up_idx,
                    Action::MoveRemote {
                        from: from_path.clone(),
                        to: to_path.clone(),
                        meta: lm.clone(),
                    },
                    lm.size,
                ));
            }
        }

        if !new_moves.is_empty() {
            let mut remove_indices: Vec<usize> = Vec::new();
            for (del_idx, up_idx, move_action, size) in new_moves {
                remove_indices.push(del_idx);
                remove_indices.push(up_idx);
                actions.push(move_action);
                stats.delete_remote = stats.delete_remote.saturating_sub(1);
                stats.uploads = stats.uploads.saturating_sub(1);
                stats.bytes_up = stats.bytes_up.saturating_sub(size);
                stats.moved += 1;
            }
            remove_indices.sort_unstable();
            for idx in remove_indices.into_iter().rev() {
                actions.remove(idx);
            }
        }
    }
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
                Action::MoveLocal { .. } => "move_local",
                Action::MoveRemote { .. } => "move_remote",
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

    #[test]
    fn test_detect_remote_rename_to_move_local() {
        // 基线：old.txt
        let base = tree(&[("old.txt", meta("content_hash", 1024, 100))]);
        // 远端重命名为 new.txt，old.txt 已不存在
        let remote = manifest(tree(&[("new.txt", meta("content_hash", 1024, 200))]));
        // 本地仍是 old.txt
        let local = tree(&[("old.txt", meta("content_hash", 1024, 100))]);

        let p = plan_with(local, Some(remote), base);
        assert_eq!(kinds(&p), ["move_local"]);
        assert_eq!(p.stats.moved, 1);
        assert_eq!(p.stats.downloads, 0);
        assert_eq!(p.stats.delete_local, 0);
        assert_eq!(p.stats.bytes_down, 0);
    }

    #[test]
    fn test_detect_local_rename_to_move_remote() {
        // 基线：doc.docx
        let base = tree(&[("doc.docx", meta("doc_hash", 2048, 100))]);
        // 远端仍是 doc.docx
        let remote = manifest(tree(&[("doc.docx", meta("doc_hash", 2048, 100))]));
        // 本地已重命名为 renamed.docx，原 doc.docx 已不存在
        let local = tree(&[("renamed.docx", meta("doc_hash", 2048, 200))]);

        let p = plan_with(local, Some(remote), base);
        assert_eq!(kinds(&p), ["move_remote"]);
        assert_eq!(p.stats.moved, 1);
        assert_eq!(p.stats.uploads, 0);
        assert_eq!(p.stats.delete_remote, 0);
        assert_eq!(p.stats.bytes_up, 0);
    }

    #[test]
    fn test_ignored_file_on_remote_never_downloads() {
        // 远端有 .obsidian/main.js，本地无，基线无；该文件被忽略
        let remote = manifest(tree(&[(".obsidian/main.js", meta("hash1", 100, 100))]));
        let p = compare_with_ignore(
            &tree(&[]),
            Some(&remote),
            &tree(&[]),
            Direction::Bidirectional,
            ConflictPolicy::BothCopies,
            &|path| path.starts_with('.'),
        );
        // 绝不能判定为 download，应直接忽略跳过
        assert_eq!(kinds(&p), Vec::<String>::new());
        assert_eq!(p.stats.downloads, 0);
    }

    #[test]
    fn test_ignored_file_in_baseline_deletes_remote() {
        // 基线有 .obsidian/main.js，本地已删除（或新配置了忽略），远端仍存在
        let base = tree(&[(".obsidian/main.js", meta("hash1", 100, 100))]);
        let remote = manifest(base.clone());
        let p = compare_with_ignore(
            &tree(&[]),
            Some(&remote),
            &base,
            Direction::Bidirectional,
            ConflictPolicy::BothCopies,
            &|path| path.starts_with('.'),
        );
        // 双向同步下应推送删除存储后端中的文件，绝不重新下载到本地
        assert_eq!(kinds(&p), ["del_remote"]);
        assert_eq!(p.stats.delete_remote, 1);
        assert_eq!(p.stats.downloads, 0);
    }
}
