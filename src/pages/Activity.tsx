import { useMemo, useState } from "react";
import { api } from "../api";
import type { FolderSummary, SyncEvent } from "../types";

export default function Activity({
  events,
  folders,
}: {
  events: SyncEvent[];
  folders: FolderSummary[];
}) {
  const [selectedFolder, setSelectedFolder] = useState<string>("all");
  const [selectedAction, setSelectedAction] = useState<string>("all");
  const [keyword, setKeyword] = useState("");

  // 文件夹 ID -> 文件夹映射
  const folderMap = useMemo(() => {
    const map = new Map<string, FolderSummary>();
    for (const f of folders) {
      map.set(f.id, f);
    }
    return map;
  }, [folders]);

  // 提取出文件操作类和关键事件
  const activityList = useMemo(() => {
    const list: Array<{
      id: string | number;
      folderId: string;
      folderName: string;
      folderLocalPath: string;
      type: string;
      path?: string;
      action?: string;
      error?: string;
      reason?: string;
      copy?: string;
      timestamp?: number;
    }> = [];

    for (let i = events.length - 1; i >= 0; i--) {
      const e = events[i];
      const fid = (e as any).folderId ?? (e as any).folder_id;
      if (!fid) continue;
      const f = folderMap.get(fid);
      const folderName = f ? f.name : "未知任务";
      const folderLocalPath = f ? f.localPath : "";

      if (e.type === "file_done") {
        list.push({
          id: `${i}-${e.path}`,
          folderId: fid,
          folderName,
          folderLocalPath,
          type: "file_done",
          path: e.path,
          action: e.action,
          timestamp: e.timestamp,
        });
      } else if (e.type === "file_failed") {
        list.push({
          id: `${i}-${e.path}`,
          folderId: fid,
          folderName,
          folderLocalPath,
          type: "file_failed",
          path: e.path,
          error: e.error,
          timestamp: e.timestamp,
        });
      } else if (e.type === "file_pending") {
        list.push({
          id: `${i}-${e.path}`,
          folderId: fid,
          folderName,
          folderLocalPath,
          type: "file_pending",
          path: e.path,
          reason: e.reason,
          timestamp: e.timestamp,
        });
      } else if (e.type === "conflict_created") {
        list.push({
          id: `${i}-${e.path}`,
          folderId: fid,
          folderName,
          folderLocalPath,
          type: "conflict_created",
          path: e.path,
          copy: e.copy,
          timestamp: e.timestamp,
        });
      }
    }
    return list;
  }, [events, folderMap]);

  // 按条件过滤
  const filtered = useMemo(() => {
    const kw = keyword.trim().toLowerCase();
    return activityList.filter((item) => {
      if (selectedFolder !== "all" && item.folderId !== selectedFolder) {
        return false;
      }
      if (selectedAction !== "all") {
        if (selectedAction === "upload" && item.action !== "upload") return false;
        if (selectedAction === "download" && item.action !== "download") return false;
        if (selectedAction === "delete" && !item.action?.includes("delete")) return false;
        if (selectedAction === "conflict" && item.type !== "conflict_created") return false;
        if (selectedAction === "failed" && item.type !== "file_failed") return false;
        if (selectedAction === "pending" && item.type !== "file_pending" && item.action !== "pending_replace") return false;
      }
      if (kw && !(item.path ?? "").toLowerCase().includes(kw) && !item.folderName.toLowerCase().includes(kw)) {
        return false;
      }
      return true;
    });
  }, [activityList, selectedFolder, selectedAction, keyword]);

  function locateFile(localRoot: string, relPath: string) {
    if (!localRoot) return;
    const cleanRoot = localRoot.replace(/[/\\]+$/, "");
    const cleanRel = relPath.replace(/^[/\\]+/, "");
    const fullPath = `${cleanRoot}\\${cleanRel.replace(/\//g, "\\")}`;
    api.openInExplorer(fullPath, true).catch(() => {
      // 若单文件定位失败，尝试打开根目录
      api.openInExplorer(cleanRoot, false).catch(() => {});
    });
  }

  function formatTime(ts?: number): string {
    if (!ts) return "刚刚";
    const now = Date.now();
    const diff = Math.floor((now - ts) / 1000);
    if (diff < 5) return "刚刚";
    if (diff < 60) return `${diff} 秒前`;
    if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`;
    const d = new Date(ts);
    return d.toLocaleTimeString("zh-CN", { hour12: false });
  }

  function getActionBadge(item: (typeof activityList)[0]) {
    if (item.type === "file_failed") {
      return <span className="act-badge badge-err">✗ 失败</span>;
    }
    if (item.type === "file_pending") {
      return <span className="act-badge badge-warn">⏳ 待替换</span>;
    }
    if (item.type === "conflict_created") {
      return <span className="act-badge badge-warn">⚠ 冲突副本</span>;
    }
    switch (item.action) {
      case "upload":
        return <span className="act-badge badge-up">↑ 上传</span>;
      case "download":
        return <span className="act-badge badge-down">↓ 下载</span>;
      case "pending_replace":
        return <span className="act-badge badge-up">⏳ 已应用</span>;
      case "delete_local":
      case "delete_remote":
        return <span className="act-badge badge-del">🗑 删除</span>;
      case "conflict":
        return <span className="act-badge badge-warn">⚠ 冲突</span>;
      default:
        return <span className="act-badge">{item.action}</span>;
    }
  }

  return (
    <div className="page">
      <div className="page-header">
        <div>
          <h1>传输历史 · 同步动态</h1>
          <div className="header-sub">实时跟踪记录每个文件的变动、上传、下载与同步事件</div>
        </div>
      </div>

      <div className="activity-filter-bar">
        <div className="filter-group">
          <label>同步文件夹：</label>
          <select
            className="input input-sm"
            value={selectedFolder}
            onChange={(e) => setSelectedFolder(e.target.value)}
          >
            <option value="all">全部文件夹 ({activityList.length})</option>
            {folders.map((f) => (
              <option key={f.id} value={f.id}>
                {f.name}
              </option>
            ))}
          </select>
        </div>

        <div className="filter-group">
          <label>动作类型：</label>
          <select
            className="input input-sm"
            value={selectedAction}
            onChange={(e) => setSelectedAction(e.target.value)}
          >
            <option value="all">全部类型</option>
            <option value="upload">↑ 上传</option>
            <option value="download">↓ 下载</option>
            <option value="delete">🗑 删除</option>
            <option value="conflict">⚠ 冲突</option>
            <option value="pending">⏳ 待替换</option>
            <option value="failed">✗ 失败</option>
          </select>
        </div>

        <div className="filter-group filter-grow">
          <input
            className="input input-sm"
            type="search"
            placeholder="搜索文件路径或任务名…"
            value={keyword}
            onChange={(e) => setKeyword(e.target.value)}
          />
        </div>

        <div className="filter-summary">共 {filtered.length} 条记录</div>
      </div>

      <div className="activity-list">
        {filtered.length === 0 ? (
          <div className="empty-state">
            <div className="empty-icon">📋</div>
            <h2>暂无传输动态</h2>
            <p>当本地文件发生改动或云端同步更新时，此处将实时展示每一个文件的同步明细。</p>
          </div>
        ) : (
          filtered.map((item) => {
            const fileName = item.path ? item.path.split(/[/\\]/).pop() : "";
            return (
              <div key={item.id} className="activity-card">
                <div className="act-col-icon">{getActionBadge(item)}</div>
                <div className="act-col-main">
                  <div className="act-filename" title={item.path}>
                    {fileName || item.path}
                  </div>
                  <div className="act-filepath" title={item.path}>
                    {item.path}
                  </div>
                  {item.error && <div className="act-error">原因: {item.error}</div>}
                  {item.reason && <div className="act-copy">⏳ {item.reason}</div>}
                  {item.copy && <div className="act-copy">副本路径: {item.copy}</div>}
                </div>
                <div className="act-col-meta">
                  <span className="badge badge-plain">{item.folderName}</span>
                  <span className="act-time">{formatTime(item.timestamp)}</span>
                </div>
                <div className="act-col-ops">
                  {item.path && item.folderLocalPath && (
                    <button
                      className="btn btn-sm btn-ghost"
                      title="在文件资源管理器中定位"
                      onClick={() => locateFile(item.folderLocalPath, item.path!)}
                    >
                      📁 定位
                    </button>
                  )}
                </div>
              </div>
            );
          })
        )}
      </div>
    </div>
  );
}
