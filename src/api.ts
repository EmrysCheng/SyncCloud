import { invoke } from "@tauri-apps/api/core";
import type {
  AppInfo,
  ActivityPage,
  AliyunQrInfo,
  BaiduLoginInfo,
  BackendConfig,
  CloudAccount,
  CloudAccountStatus,
  ConflictRecord,
  FolderConfig,
  FolderSummary,
  GlobalSettings,
  LogFileInfo,
  RecycleItem,
  SyncEvent,
  SyncPlan,
} from "./types";

export const api = {
  getAppInfo: () => invoke<AppInfo>("get_app_info"),
  getSettings: () => invoke<GlobalSettings>("get_settings"),
  saveSettings: (deviceName: string, settings: GlobalSettings) =>
    invoke<void>("save_settings", { deviceName, settings }),
  openExternal: (url: string) => invoke<void>("open_external", { url }),
  listFolders: () => invoke<FolderSummary[]>("list_folders"),
  addFolder: (folder: FolderConfig) => invoke<string>("add_folder", { folder }),
  updateFolder: (folder: FolderConfig) => invoke<void>("update_folder", { folder }),
  removeFolder: (folderId: string) => invoke<void>("remove_folder", { folderId }),
  syncNow: (folderId: string) => invoke<boolean>("sync_now", { folderId }),
  forcePushFolder: (folderId: string) => invoke<boolean>("force_push_folder", { folderId }),
  pauseFolder: (folderId: string) => invoke<void>("pause_folder", { folderId }),
  resumeFolder: (folderId: string) => invoke<void>("resume_folder", { folderId }),
  testBackend: (backend: BackendConfig) => invoke<string>("test_backend", { backend }),
  listConflicts: (folderId: string, includeResolved: boolean) =>
    invoke<ConflictRecord[]>("list_conflicts", { folderId, includeResolved }),
  resolveConflict: (folderId: string, conflictId: number, resolution: string) =>
    invoke<void>("resolve_conflict", { folderId, conflictId, resolution }),
  getRecentEvents: (limit: number) => invoke<SyncEvent[]>("get_recent_events", { limit }),
  baiduLoginStart: (appKey: string, secretKey: string) =>
    invoke<BaiduLoginInfo>("baidu_login_start", { appKey, secretKey }),
  baiduLoginPoll: (
    appKey: string,
    secretKey: string,
    deviceCode: string,
    authId?: string,
    accountName?: string,
  ) =>
    invoke<CloudAccount | null>("baidu_login_poll", {
      appKey,
      secretKey,
      deviceCode,
      authId,
      accountName,
    }),
  aliyunQrStart: () => invoke<AliyunQrInfo>("aliyun_qr_start"),
  aliyunQrPoll: (sid: string, authId?: string, accountName?: string) =>
    invoke<CloudAccount | null>("aliyun_qr_poll", { sid, authId, accountName }),
  aliyunAuthorized: (authId?: string) => invoke<boolean>("aliyun_authorized", { authId }),
  baiduAuthorized: (authId?: string, appKey?: string) =>
    invoke<boolean>("baidu_authorized", { authId, appKey }),
  isAutostart: () => invoke<boolean>("is_autostart"),
  setAutostart: (enabled: boolean) => invoke<void>("set_autostart", { enabled }),
  isShellMenuEnabled: () => invoke<boolean>("is_shell_menu_enabled"),
  setShellMenuEnabled: (enabled: boolean) => invoke<void>("set_shell_menu_enabled", { enabled }),
  setFileStorageMode: (folderId: string, path: string, mode: "keep_offline" | "free_space") =>
    invoke<number>("set_file_storage_mode", { folderId, path, mode }),
  listCloudAccounts: (provider?: string) =>
    invoke<CloudAccount[]>("list_cloud_accounts", { provider }),
  deleteCloudAccount: (id: string) => invoke<void>("delete_cloud_account", { id }),
  saveCloudAccount: (account: CloudAccount) => invoke<void>("save_cloud_account", { account }),
  changeDataDir: (newPath: string, copyExisting: boolean) =>
    invoke<string>("change_data_dir", { newPath, copyExisting }),
  restartApp: () => invoke<void>("restart_app"),
  openInExplorer: (path: string, select?: boolean) =>
    invoke<void>("open_path_in_explorer", { path, select: !!select }),
  openDataDir: () => invoke<void>("open_data_dir"),
  syncAllFolders: () => invoke<void>("sync_all_folders"),
  pauseAllFolders: () => invoke<void>("pause_all_folders"),
  resumeAllFolders: () => invoke<void>("resume_all_folders"),
  analyzeFolder: (folderId: string) =>
    invoke<SyncPlan>("analyze_folder", { folderId }),
  openRecycleBin: (folderId?: string) =>
    invoke<void>("open_recycle_bin", { folderId }),
  openRecycleBinDir: (folderId: string) =>
    invoke<void>("open_recycle_bin_dir", { folderId }),
  listRecycleBin: (folderId: string) =>
    invoke<RecycleItem[]>("list_recycle_bin", { folderId }),
  restoreRecycleFile: (folderId: string, itemId: string) =>
    invoke<string>("restore_recycle_file", { folderId, itemId }),
  cleanRecycleBin: (folderId: string) =>
    invoke<number>("clean_recycle_bin", { folderId }),
  deleteRecycleItem: (folderId: string, itemId: string) =>
    invoke<void>("delete_recycle_item", { folderId, itemId }),
  isAllPaused: () => invoke<boolean>("is_all_paused"),
  cloudAccountStatus: (accountId: string) =>
    invoke<CloudAccountStatus>("cloud_account_status", { accountId }),
  listLogFiles: () => invoke<LogFileInfo[]>("list_log_files"),
  readLogTail: (file: string, lines: number) =>
    invoke<string[]>("read_log_tail", { file, lines }),
  openLogDir: () => invoke<void>("open_log_dir"),
  getCurrentLogDir: () => invoke<string>("get_current_log_dir"),
  getActivityHistory: (
    folderId?: string,
    action?: string,
    keyword?: string,
    limit?: number,
    offset?: number,
  ) =>
    invoke<ActivityPage>("get_activity_history", { folderId, action, keyword, limit, offset }),
  cleanExpiredActivity: () => invoke<number>("clean_expired_activity"),
  clearActivityHistory: (folderId?: string) =>
    invoke<number>("clear_activity_history", { folderId }),
};
