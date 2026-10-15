// 与 Rust 侧 serde 序列化对应的类型定义。
// 注意：枚举 tag 与字段命名与 Rust 的 rename_all 保持一致。

export type Direction = "bidirectional" | "mirror" | "contribute" | "upload_only" | "download_only";
export type ConflictPolicy = "both_copies" | "newest_wins";
export type SyncMode = "realtime" | "interval" | "manual";
export type TransferPriority = "smallest_first" | "newest_first" | "largest_first" | "order_alpha";
export type EncryptionMode = "off" | "passphrase";

export interface BackendConfig {
  type: "s3" | "webdav" | "git" | "baidu" | "aliyun";
  // S3
  endpoint?: string;
  region?: string;
  bucket?: string;
  accessKeyId?: string;
  secretAccessKey?: string;
  pathStyle?: boolean;
  rootPrefix?: string;
  // WebDAV
  url?: string;
  username?: string;
  password?: string;
  rootPath?: string;
  // Git
  remoteUrl?: string;
  branch?: string;
  // Baidu / Aliyun
  authId?: string;
  accountName?: string;
  appKey?: string;
  secretKey?: string;
  appId?: string;
  appSecret?: string;
  remoteDir?: string;
}

export interface FolderConfig {
  id: string;
  name: string;
  localPath: string;
  backend: BackendConfig;
  direction: Direction;
  conflictPolicy: ConflictPolicy;
  ignoreRules: string[];
  syncMode: SyncMode;
  intervalSecs: number;
  encryption: { mode: EncryptionMode };
  transferPriority?: TransferPriority;
  maxFileSizeMb?: number | null;
  ignoreHidden?: boolean;
  enabled: boolean;
}

export interface CycleReport {
  uploaded: number;
  downloaded: number;
  deletedLocal: number;
  deletedRemote: number;
  moved?: number;
  conflicts: number;
  conflictsResolvedContentIdentical: number;
  failed: number;
  skipped: number;
  bytesUploaded: number;
  bytesDownloaded: number;
  tookMs: number;
}

export interface FolderStatus {
  folderId: string;
  state: "idle" | "syncing" | "paused" | "error";
  lastSyncAt: string | null;
  lastError: string | null;
  lastReport: CycleReport | null;
}

export interface FolderSummary extends FolderConfig {
  status: FolderStatus | null;
}

export interface GlobalSettings {
  concurrency: number;
  maxRetries: number;
  fullScanIntervalMin: number;
  largeFileThreshold: number;
  language: string;
  closeToTray: boolean;
  baiduAppKey: string;
  baiduSecretKey: string;
  aliyunAppId: string;
  aliyunAppSecret: string;
  logLevel: string;
  logToFile: boolean;
  deletionGuard: boolean;
  deletionGuardThreshold: number;
  customLogDir?: string | null;
  activityRetentionDays?: number;
  maxUploadKbps: number;
  maxDownloadKbps: number;
}

export interface ActivityItem {
  id: number;
  folderId: string;
  folderName: string;
  folderLocalPath: string;
  eventType: string;
  path?: string;
  action?: string;
  error?: string;
  reason?: string;
  copy?: string;
  timestamp: number;
}

export interface ActivityPage {
  items: ActivityItem[];
  total: number;
}

export interface AppInfo {
  deviceName: string;
  deviceId: string;
  version: string;
  dataDir: string;
  defaultDataDir: string;
}

export type SyncEvent = (
  | { type: "cycle_started"; folderId: string; folder: string }
  | { type: "scan_done"; folderId: string; files: number; hashed: number; cacheHits: number; ignored: number }
  | { type: "cycle_skipped"; folderId: string; folder: string }
  | { type: "planned"; folderId: string; stats: PlanStats }
  | { type: "file_done"; folderId: string; path: string; action: string }
  | { type: "file_failed"; folderId: string; path: string; error: string; retryInSecs: number }
  | { type: "file_pending"; folderId: string; path: string; reason: string }
  | { type: "conflict_created"; folderId: string; path: string; copy: string }
  | { type: "safety_blocked"; folderId: string; folder: string; deletedCount: number; totalCount: number; reason: string }
  | { type: "cycle_done"; folderId: string; folder: string; report: CycleReport }
  | { type: "error"; folderId: string; folder: string; message: string }
) & {
  timestamp?: number;
  folder_id?: string;
};

export interface PlanStats {
  uploads: number;
  downloads: number;
  deleteLocal: number;
  deleteRemote: number;
  conflicts: number;
  identical: number;
  unchanged: number;
  bytesUp: number;
  bytesDown: number;
}

export interface ConflictRecord {
  id: number;
  path: string;
  localHash: string;
  remoteHash: string;
  conflictCopy: string | null;
  createdAt: string;
  resolved: boolean;
  resolution: string | null;
}

export interface BaiduLoginInfo {
  userCode: string;
  verificationUrl: string;
  qrcodeUrl: string;
  deviceCode: string;
  interval: number;
}

export interface AliyunQrInfo {
  qrUrl: string;
  linkUrl: string;
  sid: string;
}

export interface CloudAccount {
  id: string;
  provider: "baidu" | "aliyun";
  name: string;
  appKey?: string;
  secretKey?: string;
  createdAt: string;
}

export interface CloudAccountStatus {
  accountId: string;
  authorized: boolean;
  ok: boolean;
  detail: string;
  checkedAt: string;
}

export interface LogFileInfo {
  name: string;
  size: number;
  modified: string;
}

export interface FileMeta {
  hash: string;
  size: number;
  mtimeMs: number;
}

export type PlanActionKind =
  | "upload"
  | "download"
  | "delete_remote"
  | "delete_local"
  | "move_local"
  | "move_remote"
  | "conflict"
  | "identical"
  | "remote_wins_over_delete"
  | "local_wins_over_delete";

export interface PlanAction {
  kind: PlanActionKind;
  path?: string;
  from?: string;
  to?: string;
  meta?: FileMeta;
  local?: FileMeta;
  remote?: FileMeta;
}

export interface PlanStats {
  uploads: number;
  downloads: number;
  deleteLocal: number;
  deleteRemote: number;
  moved?: number;
  conflicts: number;
  identical: number;
  unchanged: number;
  bytesUp: number;
  bytesDown: number;
}

export interface SyncPlan {
  actions: PlanAction[];
  stats: PlanStats;
}

export interface RecycleItem {
  id: string;
  fileName: string;
  originalPath: string;
  size: number;
  action: "deleted" | "overwritten";
  archivedAt: string;
}

export interface FolderFileItem {
  name: string;
  relPath: string;
  isDir: boolean;
  size: number;
  localSize: number;
  mtimeMs: number;
  storageMode: "keep_offline" | "free_space";
  ext: string;
}

