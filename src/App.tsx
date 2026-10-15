import { useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api } from "./api";
import type { SyncEvent } from "./types";
import Folders from "./pages/Folders";
import Activity from "./pages/Activity";
import AddFolder from "./pages/AddFolder";
import Conflicts from "./pages/Conflicts";
import Settings from "./pages/Settings";
import type { FolderSummary } from "./types";

type Page = "folders" | "activity" | "add" | "conflicts" | "settings";

export default function App() {
  const [page, setPage] = useState<Page>("folders");
  const [events, setEvents] = useState<SyncEvent[]>([]);
  const [folders, setFolders] = useState<FolderSummary[]>([]);
  const [toast, setToast] = useState<string | null>(null);
  const toastTimer = useRef<number | null>(null);

  const refreshFolders = () => api.listFolders().then(setFolders).catch(() => {});

  useEffect(() => {
    refreshFolders();
    const t = setInterval(refreshFolders, 3000);
    return () => clearInterval(t);
  }, []);

  // 订阅后端事件（全局事件流）
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    listen<SyncEvent>("synccloud://event", (e) => {
      const payload: SyncEvent = { ...e.payload, timestamp: Date.now() };
      setEvents((prev) => {
        const next = [...prev, payload];
        return next.length > 500 ? next.slice(next.length - 500) : next;
      });
      if (e.payload.type === "error") {
        showToast(`${e.payload.folder}: ${e.payload.message}`);
      }
    }).then((fn) => (unlisten = fn));
    // 补拉历史事件
    api
      .getRecentEvents(200)
      .then((evs) => {
        const enriched = evs.map((ev) => ({
          ...ev,
          timestamp: ev.timestamp ?? Date.now(),
        }));
        setEvents(enriched);
      })
      .catch(() => {});
    return () => {
      unlisten?.();
    };
  }, []);

  function showToast(msg: string) {
    setToast(msg);
    if (toastTimer.current) window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToast(null), 6000);
  }

  const navItems = useMemo(
    () => [
      { key: "folders" as Page, icon: "📁", label: "同步文件夹" },
      { key: "activity" as Page, icon: "📋", label: "传输历史" },
      { key: "conflicts" as Page, icon: "⚠️", label: "冲突中心" },
      { key: "settings" as Page, icon: "⚙️", label: "设置" },
    ],
    [],
  );

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <span className="brand-icon">☁️</span>
          <div>
            <div className="brand-name">SyncCloud</div>
            <div className="brand-sub">多后端文件同步</div>
          </div>
        </div>
        <nav>
          {navItems.map((n) => (
            <button
              key={n.key}
              className={`nav-item ${page === n.key ? "active" : ""}`}
              onClick={() => setPage(n.key)}
            >
              <span>{n.icon}</span>
              {n.label}
            </button>
          ))}
        </nav>
        <button className="btn btn-primary add-btn" onClick={() => setPage("add")}>
          ＋ 添加同步文件夹
        </button>
        <div className="sidebar-footer">
          <EventTicker events={events} />
          <button
            type="button"
            className="sidebar-github-link"
            title="访问 SyncCloud GitHub 开源仓库"
            onClick={() => api.openExternal("https://github.com/EmrysCheng/SyncCloud")}
          >
            <svg width="13" height="13" viewBox="0 0 24 24" fill="currentColor">
              <path d="M12 0c-6.626 0-12 5.373-12 12 0 5.302 3.438 9.8 8.207 11.387.599.111.793-.261.793-.577v-2.234c-3.338.726-4.033-1.416-4.033-1.416-.546-1.387-1.333-1.756-1.333-1.756-1.089-.745.083-.729.083-.729 1.205.084 1.839 1.237 1.839 1.237 1.07 1.834 2.807 1.304 3.492.997.107-.775.418-1.305.762-1.604-2.665-.305-5.467-1.334-5.467-5.931 0-1.311.469-2.381 1.236-3.221-.124-.303-.535-1.524.117-3.176 0 0 1.008-.322 3.301 1.23.957-.266 1.983-.399 3.003-.404 1.02.005 2.047.138 3.006.404 2.291-1.552 3.297-1.23 3.297-1.23.653 1.653.242 2.874.118 3.176.77.84 1.235 1.911 1.235 3.221 0 4.609-2.807 5.624-5.479 5.921.43.372.823 1.102.823 2.222v3.293c0 .319.192.694.801.576 4.765-1.589 8.199-6.086 8.199-11.386 0-6.627-5.373-12-12-12z" />
            </svg>
            <span>GitHub 项目主页</span>
          </button>
        </div>
      </aside>
      <main className="main">
        {page === "folders" && <Folders events={events} onGoto={setPage} />}
        {page === "activity" && <Activity events={events} folders={folders} />}
        {page === "add" && <AddFolder onDone={() => setPage("folders")} />}
        {page === "conflicts" && <Conflicts />}
        {page === "settings" && <Settings />}
      </main>
      {toast && (
        <div className="toast" onClick={() => setToast(null)}>
          {toast}
        </div>
      )}
    </div>
  );
}

function EventTicker({ events }: { events: SyncEvent[] }) {
  const last = events[events.length - 1];
  if (!last) return <div className="ticker muted">暂无活动</div>;
  let text = "";
  switch (last.type) {
    case "cycle_started":
      text = `开始同步 ${last.folder}`;
      break;
    case "scan_done":
      text = `扫描完成：${last.files} 个文件`;
      break;
    case "cycle_skipped":
      text = `⚡ ${last.folder}：无变化，已跳过`;
      break;
    case "planned":
      text = `计划：↑${last.stats.uploads} ↓${last.stats.downloads} 冲突 ${last.stats.conflicts}`;
      break;
    case "file_done":
      text = `${last.action === "upload" ? "已上传" : last.action === "download" ? "已下载" : last.action === "pending_replace" ? "占用释放，已应用待替换文件" : last.action} ${last.path}`;
      break;
    case "file_pending":
      text = `⏳ ${last.path} 被占用，已入待替换队列`;
      break;
    case "conflict_created":
      text = `冲突副本：${last.copy}`;
      break;
    case "cycle_done":
      text = `同步完成（${Math.round(last.report.tookMs / 100) / 10}s）`;
      break;
    case "error":
      text = `错误：${last.message.slice(0, 80)}`;
      break;
    default:
      text = "…";
  }
  return <div className="ticker" title={text}>{text}</div>;
}
