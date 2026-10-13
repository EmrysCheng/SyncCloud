//! 同步引擎核心类型定义。

use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// 统一相对路径表示：始终使用 `/` 分隔，跨平台一致。
pub type RelPath = String;

/// 文件元数据（本地/远端/基线通用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileMeta {
    pub size: u64,
    /// 修改时间（Unix 毫秒）
    pub mtime_ms: i64,
    /// xxh3-128 内容哈希（hex 小写）
    pub hash: String,
}

pub type FileTree = HashMap<RelPath, FileMeta>;

/// 远端权威清单：记录"远端当前应有的文件树"。
/// 每次成功上传/删除后由持锁者原子更新，是三方对比中"远端"一侧的权威数据源。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// 单调递增序号，每台机器写 manifest 时基于读到的最大值 +1
    pub seq: u64,
    /// 最后写入该 manifest 的设备名
    pub device: String,
    pub updated_at: String,
    pub files: FileTree,
}

impl Manifest {
    pub fn new(device: &str) -> Self {
        Self {
            version: 1,
            seq: 0,
            device: device.to_string(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            files: FileTree::new(),
        }
    }
}

/// 同步方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    Bidirectional,
    UploadOnly,
    DownloadOnly,
}

/// 冲突处理策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// 保留双方：远端版本落为正式文件，本地修改另存为冲突副本（Dropbox 默认行为）
    #[default]
    BothCopies,
    /// 较新 mtime 的一方胜出，败者仍生成冲突副本以防万一
    NewestWins,
}

/// 同步触发方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// 实时：文件监控触发 + 周期兜底扫描
    #[default]
    Realtime,
    /// 定时间隔轮询
    Interval,
    /// 仅手动
    Manual,
}

/// 一轮同步的统计报告。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CycleReport {
    pub uploaded: u32,
    pub downloaded: u32,
    pub deleted_local: u32,
    pub deleted_remote: u32,
    pub conflicts: u32,
    pub conflicts_resolved_content_identical: u32,
    pub failed: u32,
    pub skipped: u32,
    pub bytes_uploaded: u64,
    pub bytes_downloaded: u64,
    pub took_ms: u64,
}
