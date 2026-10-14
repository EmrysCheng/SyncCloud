import { useEffect, useMemo, useState } from "react";
import { api } from "../api";
import type { ActivityItem, FolderSummary, SyncEvent } from "../types";

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
  const [dbHistory, setDbHistory] = useState<ActivityItem[]>([]);
  const [retentionDays, setRetentionDays] = useState<number>(7);
  const [cleaning, setCleaning] = useState(false);

  // 文件夹 ID -> 文件夹映射
  const folderMap = useMemo(() => {
    const map = new Map<string, FolderSummary>();
    for (const f of folders) {
      map.set(f.id, f);
    }
    return map;
  }, [folders]);

  // 从后端持久化历史数据库加载
  const loadDbHistory = () => {
    api
      .getActivityHistory(
        selectedFolder === "all" ? undefined : selectedFolder,
        selectedAction === "all" ? undefined : selectedAction,
        keyword.trim() || undefined,
        500
      )
      .then(setDbHistory)
      .catch(() => {});
  };

  useEffect(() => {
    api
      .getSettings()
      .then((s) => {
        if (s.activityRetentionDays !== undefined) {
          setRetentionDays(s.activityRetentionDays);
        }
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    loadDbHistory();
  }, [selectedFolder, selectedAction, keyword]);

  // 当收到实时事件时，重新刷新一次持久化库
  useEffect(() => {
    if (events.length > 0) {
      const timer = setTimeout(loadDbHistory, 300);
      return () => clearTimeout(timer);
    }
  }, [events.length]);

  async function handleCleanExpired() {
    setCleaning(true);
    try {
      const n = await api.cleanExpiredActivity();
      alert(`已根据保留周期（${retentionDays === 0 ? "永久保留" : `${retentionDays} 天`}）自动清理 ${n} 条过期历史数据。`);
      loadDbHistory();
    } catch (e) {
      alert(`清理失败: ${e}`);
    } finally {
      setCleaning(false);
    }
  }

  async function handleClearAll() {
    const targetName = selectedFolder === "all" ? "全部任务" : folderMap.get(selectedFolder)?.name ?? "当前任务";
    if (!window.confirm(`确定要清空「${targetName}」的所有传输历史记录吗？`)) {
      return;
    }
    try {
      const n = await api.clearActivityHistory(selectedFolder === "all" ? undefined : selectedFolder);
      alert(`已成功清空 ${n} 条历史记录。`);
      loadDbHistory();
    } catch (e) {
      alert(`清空失败: ${e}`);
    }
  }

  async function handleUpdateRetention(days: number) {
    setRetentionDays(days);
    try {
      const s = await api.getSettings();
      const appInfo = await api.getAppInfo();
      await api.saveSettings(appInfo.deviceName, { ...s, activityRetentionDays: days });
      // 切换后立即触发一次过期清理
      const n = await api.cleanExpiredActivity();
      if (n > 0) {
        loadDbHistory();
      }
    } catch (e) {
      alert(`更新保留周期失败: ${e}`);
    }
  }

  function locateFile(localRoot: string, relPath: string) {
    if (!localRoot) return;
    const cleanRoot = localRoot.replace(/[/\\]+$/, "");
    const cleanRel = relPath.replace(/^[/\\]+/, "");
    const fullPath = `${cleanRoot}\\${cleanRel.replace(/\//g, "\\")}`;
    api.openInExplorer(fullPath, true).catch(() => {
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
    return `${d.getMonth() + 1}-${d.getDate()} ${d.toLocaleTimeString("zh-CN", { hour12: false })}`;
  }

  function getActionBadge(item: ActivityItem) {
    if (item.eventType === "safety_blocked") {
      return <span className="act-badge badge-warn" style={{ background: "#fef3c7", color: "#b45309" }}>🛡️ 误删保护</span>;
    }
    if (item.eventType === "file_failed") {
      return <span className="act-badge badge-err">✗ 失败</span>;
    }
    if (item.eventType === "file_pending") {
      return <span className="act-badge badge-warn">⏳ 待替换</span>;
    }
    if (item.eventType === "conflict_created") {
      return <span className="act-badge badge-warn">⚠ 冲突副本</span>;
    }
    switch (item.action) {
      case "upload":
        return <span className="act-badge badge-up">↑ 上传</span>;
      case "download":
        return <span className="act-badge badge-down">↓ 下载</span>;
      case "delete_local":
      case "delete_remote":
        return <span className="act-badge badge-del">🗑 删除</span>;
      case "conflict":
        return <span className="act-badge badge-warn">⚠ 冲突</span>;
      default:
        return <span className="act-badge">{item.action || "同步"}</span>;
    }
  }

  return (
    <div className="page">
      <div className="page-header">
        <div>
          <h1>传输历史 · 同步动态</h1>
          <div className="header-sub">持久化追踪记录每个文件的变动、上传、下载与安全事件</div>
        </div>
        <div className="header-actions" style={{ display: "flex", alignItems: "center", gap: "8px" }}>
          <div style={{ display: "flex", alignItems: "center", gap: "6px", fontSize: "12px", color: "var(--muted)" }}>
            <span>保留周期：</span>
            <select
              className="input input-sm"
              style={{ padding: "3px 8px", fontSize: "12px" }}
              value={retentionDays}
              onChange={(e) => handleUpdateRetention(Number(e.target.value))}
            >
              <option value={3}>3 天</option>
              <option value={7}>7 天</option>
              <option value={14}>14 天</option>
              <option value={30}>30 天</option>
              <option value={0}>永久保留</option>
            </select>
          </div>
          <button
            className="btn btn-sm btn-ghost"
            disabled={cleaning}
            title="手动清理不在保留周期内的过期记录"
            onClick={handleCleanExpired}
          >
            🧹 清理过期
          </button>
          <button
            className="btn btn-sm btn-ghost"
            title="清空当前筛选范围内的历史条目"
            onClick={handleClearAll}
          >
            🗑 清空历史
          </button>
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
            <option value="all">全部文件夹</option>
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
            <option value="all">全部动作</option>
            <option value="upload">↑ 上传</option>
            <option value="download">↓ 下载</option>
            <option value="delete">🗑 删除</option>
            <option value="conflict">⚠ 冲突副本</option>
            <option value="failed">✗ 传输失败</option>
          </select>
        </div>

        <div className="filter-group filter-grow">
          <input
            className="input input-sm"
            type="search"
            placeholder="搜索文件路径、错误原因或任务名…"
            value={keyword}
            onChange={(e) => setKeyword(e.target.value)}
          />
        </div>

        <div className="filter-summary">共 {dbHistory.length} 条记录</div>
      </div>

      <div className="activity-list">
        {dbHistory.length === 0 ? (
          <div className="empty-state">
            <div className="empty-icon">📋</div>
            <h2>暂无传输动态</h2>
            <p>当本地文件发生改动或云端同步更新时，此处将持久记录每一个文件的同步明细，并受保留周期安全保护。</p>
          </div>
        ) : (
          dbHistory.map((item) => {
            const fileName = item.path ? item.path.split(/[/\\]/).pop() : "";
            const localRoot = item.folderLocalPath || folderMap.get(item.folderId)?.localPath || "";
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
                  {item.error && <div className="act-error">错误: {item.error}</div>}
                  {item.reason && <div className="act-copy">⏳ {item.reason}</div>}
                  {item.copy && <div className="act-copy">副本: {item.copy}</div>}
                </div>
                <div className="act-col-meta">
                  <span className="badge badge-plain">{item.folderName}</span>
                  <span className="act-time">{formatTime(item.timestamp)}</span>
                </div>
                <div className="act-col-ops">
                  {item.path && localRoot && (
                    <button
                      className="btn btn-sm btn-ghost"
                      title="在文件资源管理器中定位"
                      onClick={() => locateFile(localRoot, item.path!)}
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
