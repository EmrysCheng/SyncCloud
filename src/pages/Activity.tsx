import { useEffect, useMemo, useRef, useState } from "react";
import { api } from "../api";
import type { ActivityItem, FolderSummary, SyncEvent } from "../types";

// 默认每页展示 10 条；在默认窗口（1180x760）及小窗口下无需滚动条即可完整展示一页
const DEFAULT_PAGE_SIZE = 10;
const STORAGE_KEY = "synccloud_activity_page_size";
const PRESET_PAGE_SIZES = [10, 15, 20, 50];

function getInitialPageSize(): number {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved) {
      const parsed = parseInt(saved, 10);
      if (!isNaN(parsed) && parsed >= 1 && parsed <= 200) {
        // 若为旧版本遗留的 6 或 8，规范化为新的标准默认值 10
        if (parsed === 6 || parsed === 8) return 10;
        return parsed;
      }
    }
  } catch {}
  return DEFAULT_PAGE_SIZE;
}

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
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(1);
  const [retentionDays, setRetentionDays] = useState<number>(7);
  const [cleaning, setCleaning] = useState(false);

  // 每页条数与自定义设置（仅保留：10, 15, 20, 50, 自定义）
  const [pageSize, setPageSize] = useState<number>(getInitialPageSize);
  const [isCustomSize, setIsCustomSize] = useState<boolean>(
    () => !PRESET_PAGE_SIZES.includes(getInitialPageSize()),
  );
  const [customSizeInput, setCustomSizeInput] = useState<string>(() =>
    String(getInitialPageSize()),
  );
  const [jumpPage, setJumpPage] = useState<string>("");
  const [dropupOpen, setDropupOpen] = useState(false);
  const dropupRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    function handleClickOutside(e: MouseEvent) {
      if (dropupRef.current && !dropupRef.current.contains(e.target as Node)) {
        setDropupOpen(false);
      }
    }
    if (dropupOpen) {
      document.addEventListener("mousedown", handleClickOutside);
      return () => document.removeEventListener("mousedown", handleClickOutside);
    }
  }, [dropupOpen]);

  const totalPages = Math.max(1, Math.ceil(total / pageSize));

  // 文件夹 ID -> 文件夹映射
  const folderMap = useMemo(() => {
    const map = new Map<string, FolderSummary>();
    for (const f of folders) {
      map.set(f.id, f);
    }
    return map;
  }, [folders]);

  // 从后端持久化历史数据库加载（带分页与条数）
  const loadDbHistory = (pageNo: number, currentSize = pageSize) => {
    api
      .getActivityHistory(
        selectedFolder === "all" ? undefined : selectedFolder,
        selectedAction === "all" ? undefined : selectedAction,
        keyword.trim() || undefined,
        currentSize,
        (pageNo - 1) * currentSize,
      )
      .then((res) => {
        setDbHistory(res.items);
        setTotal(res.total);
        // 当前页越界（清理/筛选后总数变小）时自动回到最后一页
        const maxPage = Math.max(1, Math.ceil(res.total / currentSize));
        if (pageNo > maxPage) {
          setPage(maxPage);
        }
      })
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
    loadDbHistory(page, pageSize);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedFolder, selectedAction, keyword, page, pageSize]);

  // 当收到实时事件时，重新刷新一次持久化库（保持在当前页）
  useEffect(() => {
    if (events.length > 0) {
      const timer = setTimeout(() => loadDbHistory(page, pageSize), 300);
      return () => clearTimeout(timer);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [events.length]);

  function handleSizeSelectChange(val: string) {
    if (val === "custom") {
      setIsCustomSize(true);
      setCustomSizeInput(String(pageSize));
    } else {
      setIsCustomSize(false);
      const n = Number(val);
      applyNewPageSize(n);
    }
  }

  function applyCustomSize() {
    const parsed = parseInt(customSizeInput, 10);
    if (!isNaN(parsed) && parsed >= 1 && parsed <= 200) {
      applyNewPageSize(parsed);
      if (PRESET_PAGE_SIZES.includes(parsed)) {
        setIsCustomSize(false);
      }
    } else {
      // 非法输入重置回原数值
      setCustomSizeInput(String(pageSize));
      if (PRESET_PAGE_SIZES.includes(pageSize)) {
        setIsCustomSize(false);
      }
    }
  }

  function applyNewPageSize(newSize: number) {
    const clamped = Math.max(1, Math.min(200, newSize));
    try {
      localStorage.setItem(STORAGE_KEY, String(clamped));
    } catch {}
    // 计算在新条数下对应的前序条目所在页码，平滑定位
    const currentOffset = (page - 1) * pageSize;
    const newPage = Math.max(1, Math.floor(currentOffset / clamped) + 1);
    setPageSize(clamped);
    setPage(newPage);
    loadDbHistory(newPage, clamped);
  }

  function handleJump() {
    const p = parseInt(jumpPage, 10);
    if (!isNaN(p) && p >= 1 && p <= totalPages) {
      setPage(p);
      setJumpPage("");
    }
  }

  async function handleCleanExpired() {
    setCleaning(true);
    try {
      const n = await api.cleanExpiredActivity();
      alert(`已根据保留周期（${retentionDays === 0 ? "永久保留" : `${retentionDays} 天`}）自动清理 ${n} 条过期历史数据。`);
      setPage(1);
      loadDbHistory(1, pageSize);
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
      setPage(1);
      loadDbHistory(1, pageSize);
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
        setPage(1);
        loadDbHistory(1, pageSize);
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
    <div className="page activity-page">
      <div className="page-header">
        <div>
          <h1>传输历史 · 同步动态</h1>
          <div className="header-sub">持久化追踪记录每个文件的变动、上传、下载与安全事件</div>
        </div>
        <div className="header-actions">
          <div className="retention-selector-wrap">
            <span className="retention-label">保留周期：</span>
            <select
              className="select-custom retention-select"
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
            className="btn-toolbar"
            disabled={cleaning}
            title="手动清理不在保留周期内的过期记录"
            onClick={handleCleanExpired}
          >
            <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="m14 12-8.5 8.5a2.12 2.12 0 1 1-3-3L11 9" />
              <path d="M16 16l6-6" />
              <path d="m18 8 2-2" />
              <path d="m14.5 4.5 5 5" />
            </svg>
            <span>清理过期</span>
          </button>
          <button
            className="btn-toolbar btn-toolbar-danger"
            title="清空当前筛选范围内的历史条目"
            onClick={handleClearAll}
          >
            <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <polyline points="3 6 5 6 21 6" />
              <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
            </svg>
            <span>清空历史</span>
          </button>
        </div>
      </div>

      <div className="activity-filter-bar">
        <div className="filter-group">
          <label>同步文件夹：</label>
          <select
            className="select-custom filter-select"
            value={selectedFolder}
            onChange={(e) => {
              setSelectedFolder(e.target.value);
              setPage(1);
            }}
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
            className="select-custom filter-select"
            value={selectedAction}
            onChange={(e) => {
              setSelectedAction(e.target.value);
              setPage(1);
            }}
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
          <div className="modern-search-box" style={{ width: "100%" }}>
            <svg className="msb-icon" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <circle cx="11" cy="11" r="8" />
              <line x1="21" y1="21" x2="16.65" y2="16.65" />
            </svg>
            <input
              className="msb-input"
              type="search"
              placeholder="搜索文件路径、错误原因或任务名…"
              value={keyword}
              onChange={(e) => {
                setKeyword(e.target.value);
                setPage(1);
              }}
            />
            {keyword && (
              <button
                type="button"
                className="msb-clear"
                title="清空搜索"
                onClick={() => {
                  setKeyword("");
                  setPage(1);
                }}
              >
                ✕
              </button>
            )}
          </div>
        </div>
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
                      className="act-locate-btn"
                      title="在文件资源管理器中定位文件"
                      onClick={() => locateFile(localRoot, item.path!)}
                    >
                      <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                        <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
                      </svg>
                      <span>定位</span>
                    </button>
                  )}
                </div>
              </div>
            );
          })
        )}
      </div>

      {total > 0 && (
        <div className="pagination-bar">
          <div className="pagination-left">
            <span className="pagination-total-text">
              共 <strong>{total}</strong> 条记录
            </span>
            <div className="pagination-dropup-container" ref={dropupRef}>
              <span className="pagination-label">每页</span>
              <button
                type="button"
                className="pagination-dropup-btn"
                onClick={() => setDropupOpen(!dropupOpen)}
                title="选择每页展示条数"
              >
                <span>{isCustomSize ? `自定义 (${pageSize})` : `${pageSize} 条/页`}</span>
                <svg
                  className={`dropup-arrow ${dropupOpen ? "open" : ""}`}
                  width="10"
                  height="6"
                  viewBox="0 0 10 6"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="1.5"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                >
                  <path d="M1 5l4-4 4 4" />
                </svg>
              </button>

              {dropupOpen && (
                <div className="pagination-dropup-menu">
                  <div className="dropup-header">选择每页条数</div>
                  {[10, 15, 20, 50].map((size) => (
                    <button
                      key={size}
                      type="button"
                      className={`dropup-item ${!isCustomSize && pageSize === size ? "active" : ""}`}
                      onClick={() => {
                        setDropupOpen(false);
                        handleSizeSelectChange(String(size));
                      }}
                    >
                      <span>{size} 条/页</span>
                      {!isCustomSize && pageSize === size && <span className="dropup-check">✓</span>}
                    </button>
                  ))}
                  <button
                    type="button"
                    className={`dropup-item ${isCustomSize ? "active" : ""}`}
                    onClick={() => {
                      setDropupOpen(false);
                      handleSizeSelectChange("custom");
                    }}
                  >
                    <span>自定义...</span>
                    {isCustomSize && <span className="dropup-check">✓</span>}
                  </button>
                </div>
              )}

              {isCustomSize && (
                <div className="custom-size-box">
                  <input
                    type="number"
                    min={1}
                    max={200}
                    className="input input-sm custom-size-input"
                    value={customSizeInput}
                    onChange={(e) => setCustomSizeInput(e.target.value)}
                    onBlur={applyCustomSize}
                    onKeyDown={(e) => e.key === "Enter" && applyCustomSize()}
                    title="输入 1-200 按回车生效"
                    placeholder="数量"
                    autoFocus
                  />
                  <span className="pagination-label">条</span>
                </div>
              )}
            </div>
          </div>

          <div className="pagination-center">
            <button
              className="pagination-btn pagination-arrow"
              disabled={page <= 1}
              onClick={() => setPage((p) => Math.max(1, p - 1))}
              title="上一页"
            >
              ← 上一页
            </button>
            <div className="pagination-pages">
              {buildPageNumbers(page, totalPages).map((p, idx) =>
                p === "..." ? (
                  <span key={`ellipsis-${idx}`} className="pagination-ellipsis">…</span>
                ) : (
                  <button
                    key={p}
                    className={`pagination-num-btn${p === page ? " active" : ""}`}
                    onClick={() => setPage(p)}
                  >
                    {p}
                  </button>
                ),
              )}
            </div>
            <button
              className="pagination-btn pagination-arrow"
              disabled={page >= totalPages}
              onClick={() => setPage((p) => Math.min(totalPages, p + 1))}
              title="下一页"
            >
              下一页 →
            </button>
          </div>

          <div className="pagination-right">
            <span className="pagination-page-stat">
              <strong>{page}</strong> / {totalPages} 页
            </span>
            <div className="pagination-jump">
              <span className="pagination-label">跳至</span>
              <input
                type="number"
                min={1}
                max={totalPages}
                className="input input-sm pagination-jump-input"
                value={jumpPage}
                onChange={(e) => setJumpPage(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleJump();
                }}
                placeholder={String(page)}
              />
              <span className="pagination-label">页</span>
              <button
                className="pagination-jump-btn"
                title="按 Enter 或点击跳转"
                onClick={handleJump}
              >
                ↵
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// 生成分页页码序列：首页/尾页必现，中间窗口展示当前页附近页码，超出部分用省略号
function buildPageNumbers(current: number, totalPages: number): (number | "...")[] {
  if (totalPages <= 7) {
    return Array.from({ length: totalPages }, (_, i) => i + 1);
  }
  const pages = new Set<number>([1, totalPages, current - 1, current, current + 1]);
  const sorted = [...pages].filter((p) => p >= 1 && p <= totalPages).sort((a, b) => a - b);
  const out: (number | "...")[] = [];
  let prev = 0;
  for (const p of sorted) {
    if (p - prev > 1) {
      out.push("...");
    }
    out.push(p);
    prev = p;
  }
  return out;
}
