import { useEffect, useMemo, useState } from "react";
import { api } from "../api";
import type { BackendConfig, FolderSummary, SyncEvent, SyncPlan } from "../types";
import FolderSettingsModal from "../components/FolderSettingsModal";
import PlanPreviewModal from "../components/PlanPreviewModal";
import RecycleBinModal from "../components/RecycleBinModal";

const BACKEND_LABEL: Record<string, string> = {
  s3: "S3 兼容",
  webdav: "WebDAV",
  git: "Git",
  baidu: "百度网盘",
  aliyun: "阿里云盘",
};

const DIRECTION_LABEL: Record<string, string> = {
  bidirectional: "双向同步",
  mirror: "镜像同步",
  contribute: "安全贡献备份",
  upload_only: "仅上传",
  download_only: "仅下载",
};

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
  return `${(n / 1024 ** 3).toFixed(2)} GB`;
}

function fmtTime(iso: string | null): string {
  if (!iso) return "从未";
  try {
    const d = new Date(iso);
    return d.toLocaleString("zh-CN", { hour12: false });
  } catch {
    return iso;
  }
}

function getFolderId(e: SyncEvent): string | null {
  return (e as any).folderId ?? (e as any).folder_id ?? null;
}

function fmtEventTime(ts?: number): string {
  if (!ts) return "";
  const d = new Date(ts);
  const pad = (n: number) => (n < 10 ? `0${n}` : String(n));
  return `[${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}] `;
}

export default function Folders({
  events,
  onGoto,
}: {
  events: SyncEvent[];
  onGoto: (p: "add" | "conflicts" | "settings" | "folders") => void;
}) {
  const [folders, setFolders] = useState<FolderSummary[]>([]);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [analyzingId, setAnalyzingId] = useState<string | null>(null);
  const [previewTarget, setPreviewTarget] = useState<{
    folder: FolderSummary;
    plan: SyncPlan;
  } | null>(null);
  const [recycleBinFolder, setRecycleBinFolder] = useState<FolderSummary | null>(null);
  const [healthMap, setHealthMap] = useState<
    Record<string, { status: "checking" | "ok" | "err"; msg?: string }>
  >({});

  const refresh = () => api.listFolders().then(setFolders).catch(() => {});

  useEffect(() => {
    refresh();
    const t = setInterval(refresh, 3000);
    return () => clearInterval(t);
  }, []);

  // 事件到达时也刷新状态
  useEffect(() => {
    if (events.length) refresh();
  }, [events.length]);

  async function act(fn: () => Promise<unknown>, id?: string) {
    if (id) setBusy(id);
    try {
      await fn();
    } catch (e) {
      alert(String(e));
    } finally {
      if (id) setBusy(null);
      refresh();
    }
  }

  async function testHealth(folderId: string, backend: BackendConfig) {
    setHealthMap((prev) => ({ ...prev, [folderId]: { status: "checking" } }));
    try {
      const res = await api.testBackend(backend);
      setHealthMap((prev) => ({ ...prev, [folderId]: { status: "ok", msg: res } }));
    } catch (e) {
      setHealthMap((prev) => ({ ...prev, [folderId]: { status: "err", msg: String(e) } }));
    }
  }

  async function handleAnalyze(folder: FolderSummary) {
    setAnalyzingId(folder.id);
    try {
      const plan = await api.analyzeFolder(folder.id);
      setPreviewTarget({ folder, plan });
    } catch (e) {
      alert(`差异分析失败: ${e}`);
    } finally {
      setAnalyzingId(null);
    }
  }

  const anyActive = folders.some((f) => f.status?.state !== "paused");

  async function handleSyncAll() {
    await act(() => api.syncAllFolders());
  }

  async function handleTogglePauseAll() {
    if (anyActive) {
      await act(() => api.pauseAllFolders());
    } else {
      await act(() => api.resumeAllFolders());
    }
  }

  if (folders.length === 0) {
    return (
      <div className="empty-state">
        <div className="empty-icon">☁️</div>
        <h2>还没有同步文件夹</h2>
        <p>将本地任意文件夹与 百度网盘 / 阿里云盘 / WebDAV / S3 / Git 建立双向同步。</p>
        <button className="btn btn-primary" onClick={() => onGoto("add")}>
          ＋ 添加第一个同步文件夹
        </button>
      </div>
    );
  }

  const idleCount = folders.filter((f) => f.status?.state === "idle").length;
  const syncingCount = folders.filter((f) => f.status?.state === "syncing").length;
  const pausedCount = folders.filter((f) => f.status?.state === "paused").length;
  const errorCount = folders.filter(
    (f) => f.status?.state === "error" || (f.status?.lastReport?.conflicts ?? 0) > 0,
  ).length;

  return (
    <div className="page">
      <div className="page-header">
        <div>
          <h1>同步文件夹</h1>
          <div className="header-sub">多存储统一对齐 · 差异预演分析与安全同步</div>
        </div>
        <div className="header-actions">
          <button className="btn btn-sm" onClick={handleSyncAll}>
            ⟳ 全部立即同步
          </button>
          <button className="btn btn-sm" onClick={handleTogglePauseAll}>
            {anyActive ? "⏸ 全部暂停" : "▶ 全部恢复"}
          </button>
          <button className="btn btn-primary btn-sm" onClick={() => onGoto("add")}>
            ＋ 添加文件夹
          </button>
        </div>
      </div>

      <div className="folders-dashboard">
        <div className="dash-stat-item">
          <span className="dash-num">{folders.length}</span>
          <span className="dash-label">全部任务</span>
        </div>
        <div className="dash-stat-item">
          <span className="dash-num stat-green">{idleCount}</span>
          <span className="dash-label">已对齐同步</span>
        </div>
        {syncingCount > 0 && (
          <div className="dash-stat-item">
            <span className="dash-num stat-blue pulsing-text">{syncingCount}</span>
            <span className="dash-label">正在同步</span>
          </div>
        )}
        {pausedCount > 0 && (
          <div className="dash-stat-item">
            <span className="dash-num stat-orange">{pausedCount}</span>
            <span className="dash-label">已暂停</span>
          </div>
        )}
        {errorCount > 0 && (
          <div className="dash-stat-item">
            <span className="dash-num stat-red">{errorCount}</span>
            <span className="dash-label">异常/冲突</span>
          </div>
        )}
      </div>

      <div className="folder-list">
        {folders.map((f) => (
          <FolderCard
            key={f.id}
            folder={f}
            events={events}
            health={healthMap[f.id]}
            expanded={expanded === f.id}
            busy={busy === f.id}
            analyzing={analyzingId === f.id}
            onToggle={() => setExpanded(expanded === f.id ? null : f.id)}
            onSync={() => act(() => api.syncNow(f.id), f.id)}
            onAnalyze={() => handleAnalyze(f)}
            onPause={() => act(() => api.pauseFolder(f.id), f.id)}
            onResume={() => act(() => api.resumeFolder(f.id), f.id)}
            onTestHealth={() => testHealth(f.id, f.backend)}
            onOpenRecycleBin={() => setRecycleBinFolder(f)}
            onRefreshed={refresh}
            onRemove={() => {
              if (
                confirm(`确定删除同步任务「${f.name}」吗？\n（本地文件与云端数据都会保留）`)
              ) {
                act(() => api.removeFolder(f.id), f.id);
              }
            }}
          />
        ))}
      </div>

      {recycleBinFolder && (
        <RecycleBinModal
          folder={recycleBinFolder}
          onClose={() => setRecycleBinFolder(null)}
        />
      )}

      {previewTarget && (
        <PlanPreviewModal
          folder={previewTarget.folder}
          plan={previewTarget.plan}
          onClose={() => setPreviewTarget(null)}
          onExecuteSync={() =>
            act(() => api.syncNow(previewTarget.folder.id), previewTarget.folder.id)
          }
        />
      )}
    </div>
  );
}

function StateDot({ state }: { state: string }) {
  const map: Record<string, [string, string]> = {
    idle: ["#22c55e", "已同步"],
    syncing: ["#3b82f6", "同步中"],
    paused: ["#f59e0b", "已暂停"],
    error: ["#ef4444", "错误"],
  };
  const [color, label] = map[state] ?? ["#94a3b8", state];
  const style = state === "syncing" ? "state-dot pulsing" : "state-dot";
  return (
    <span className="state">
      <span className={style} style={{ background: color }} />
      {label}
    </span>
  );
}

/** 从事件流推算当前周期的进度（自最近一次 cycle_started 起）。 */
function useCycleProgress(folderId: string, events: SyncEvent[], syncing: boolean) {
  return useMemo(() => {
    if (!syncing) return null;
    let start = -1;
    for (let i = events.length - 1; i >= 0; i--) {
      const e = events[i];
      if (getFolderId(e) === folderId && e.type === "cycle_started") {
        start = i;
        break;
      }
    }
    if (start < 0) return null;
    let total = 0;
    let done = 0;
    for (let i = start; i < events.length; i++) {
      const e = events[i];
      if (getFolderId(e) !== folderId) continue;
      if (e.type === "planned") {
        const s: any = e.stats;
        const delLocal = s.deleteLocal ?? s.delete_local ?? 0;
        const delRemote = s.deleteRemote ?? s.delete_remote ?? 0;
        total =
          (s.uploads ?? 0) +
          (s.downloads ?? 0) +
          delLocal +
          delRemote +
          (s.conflicts ?? 0);
      } else if (e.type === "file_done") {
        done += 1;
      }
    }
    if (total <= 0) return null;
    return { total, done: Math.min(done, total) };
  }, [events, folderId, syncing]);
}

function FolderCard({
  folder: f,
  events,
  health,
  expanded,
  busy,
  analyzing,
  onToggle,
  onSync,
  onAnalyze,
  onPause,
  onResume,
  onRemove,
  onRefreshed,
  onOpenRecycleBin,
  onTestHealth,
}: {
  folder: FolderSummary;
  events: SyncEvent[];
  health?: { status: "checking" | "ok" | "err"; msg?: string };
  expanded: boolean;
  busy: boolean;
  analyzing: boolean;
  onToggle: () => void;
  onSync: () => void;
  onAnalyze: () => void;
  onPause: () => void;
  onResume: () => void;
  onRemove: () => void;
  onRefreshed: () => void;
  onOpenRecycleBin: () => void;
  onTestHealth: () => void;
}) {
  const [editing, setEditing] = useState(false);
  const st = f.status;
  const r = st?.lastReport;
  const syncing = st?.state === "syncing";
  const progress = useCycleProgress(f.id, events, syncing);
  const folderEvents = useMemo(() => {
    return events
      .filter((e) => getFolderId(e) === f.id)
      .slice(-60)
      .reverse();
  }, [events, f.id]);
  const safetyEvent = useMemo(() => {
    return folderEvents.find((e) => e.type === "safety_blocked") as
      | { type: "safety_blocked"; reason: string; deletedCount: number; totalCount: number }
      | undefined;
  }, [folderEvents]);

  function locateFile(relPath: string) {
    const cleanRoot = f.localPath.replace(/[/\\]+$/, "");
    const cleanRel = relPath.replace(/^[/\\]+/, "");
    const fullPath = `${cleanRoot}\\${cleanRel.replace(/\//g, "\\")}`;
    api.openInExplorer(fullPath, true).catch(() => {
      api.openInExplorer(cleanRoot, false).catch(() => {});
    });
  }

  return (
    <div className={`folder-card ${f.enabled ? "" : "disabled"}`}>
      <div className="folder-head" onClick={onToggle}>
        <div className="folder-title">
          <span className="folder-name">{f.name}</span>
          <StateDot state={st?.state ?? "idle"} />
        </div>
        <div className="folder-meta">
          <span className="badge">{BACKEND_LABEL[f.backend.type] ?? f.backend.type}</span>
          {health && (
            <span
              className={`badge ${health.status === "ok" ? "badge-ok" : health.status === "err" ? "badge-err" : ""}`}
              title={health.msg}
            >
              {health.status === "checking"
                ? "探测中…"
                : health.status === "ok"
                ? "● 云端在线"
                : "● 离线/失效"}
            </span>
          )}
          <span className="badge badge-plain">{DIRECTION_LABEL[f.direction]}</span>
          <span
            className="path clickable-path"
            title="点击在资源管理器中打开此文件夹"
            onClick={(e) => {
              e.stopPropagation();
              api.openInExplorer(f.localPath, false);
            }}
          >
            📂 {f.localPath} <span className="open-icon">↗</span>
          </span>
          <span className="muted">上次同步：{fmtTime(st?.lastSyncAt ?? null)}</span>
        </div>
        {safetyEvent && (
          <div
            style={{
              marginTop: "8px",
              padding: "10px 12px",
              borderRadius: "6px",
              background: "#fffbeb",
              border: "1px solid #fde68a",
              color: "#92400e",
              fontSize: "13px",
              display: "flex",
              justifyContent: "space-between",
              alignItems: "center",
            }}
          >
            <div>
              <strong>🛡️ 误删保护拦截：</strong>
              <span>{safetyEvent.reason}</span>
            </div>
            <button
              className="btn btn-sm btn-primary"
              style={{ marginLeft: "10px", whiteSpace: "nowrap" }}
              onClick={onAnalyze}
            >
              差异分析核验
            </button>
          </div>
        )}
        {st?.lastError && <div className="folder-error">⚠ {st.lastError}</div>}
        {progress && (
          <div className="folder-progress">
            <div className="progress-track">
              <div
                className="progress-bar"
                style={{ width: `${(progress.done / progress.total) * 100}%` }}
              />
            </div>
            <div className="progress-info">
              <span className="progress-text">
                {progress.done}/{progress.total} 个文件 (
                {Math.round((progress.done / progress.total) * 100)}%)
              </span>
              <span className="progress-speed">⟳ 正在同步传输…</span>
            </div>
          </div>
        )}
        {r && (
          <div className="folder-report">
            {r.skipped > 0 ? (
              <span className="badge badge-ok">无变化 · 已跳过</span>
            ) : (
              <>
                ↑{r.uploaded} · ↓{r.downloaded}
                {r.conflicts > 0 && ` · ⚠冲突 ${r.conflicts}`}
                {r.failed > 0 && ` · ✗失败 ${r.failed}`}
                {r.uploaded + r.downloaded + r.failed > 0 &&
                  ` · ${fmtBytes(r.bytesUploaded + r.bytesDownloaded)}`}
                {` · ${(r.tookMs / 1000).toFixed(1)}s`}
              </>
            )}
          </div>
        )}
      </div>

      <div className="folder-actions">
        <button
          className="btn btn-sm btn-ghost"
          disabled={busy || analyzing}
          title="仅扫描并对比本地与云端差异，预演将发生的文件变动，不实际上传或删除"
          onClick={onAnalyze}
        >
          {analyzing ? "🔍 分析中…" : "🔍 分析差异"}
        </button>
        <button className="btn btn-sm" disabled={busy || analyzing} onClick={onSync}>
          ⟳ 立即同步
        </button>
        {st?.state === "paused" ? (
          <button className="btn btn-sm" disabled={busy} onClick={onResume}>
            ▶ 恢复
          </button>
        ) : (
          <button className="btn btn-sm" disabled={busy} onClick={onPause}>
            ⏸ 暂停
          </button>
        )}
        <button
          className="btn btn-sm"
          title="在资源管理器中打开此文件夹"
          onClick={() => api.openInExplorer(f.localPath, false)}
        >
          📁 打开目录
        </button>
        <button
          className="btn btn-sm btn-ghost"
          title="查看被同步删除或被覆盖文件的安全备份（支持一键还原与清理）"
          onClick={onOpenRecycleBin}
        >
          🛡️ 安全回收站
        </button>
        <button
          className="btn btn-sm btn-ghost"
          title="检测此存储后端的网络与授权连通性"
          onClick={onTestHealth}
        >
          ⚡ 探测云端
        </button>
        <button className="btn btn-sm" onClick={() => setEditing(true)}>
          ⚙ 设置
        </button>
        <button className="btn btn-sm btn-danger" disabled={busy} onClick={onRemove}>
          删除
        </button>
        <button className="btn btn-sm btn-ghost" onClick={onToggle}>
          {expanded ? "收起日志 ▲" : "展开日志 ▼"}
        </button>
      </div>

      {expanded && (
        <div className="event-log">
          <div className="event-log-header">
            <span>同步操作明细（最近 {folderEvents.length} 条）</span>
          </div>
          {folderEvents.length === 0 && (
            <div className="muted event-row">暂无最近同步事件，点击上方「立即同步」即可查看实时过程</div>
          )}
          {folderEvents.map((e, i) => (
            <div className="event-row flex-row-between" key={i}>
              <div>
                <span className="event-time">{fmtEventTime(e.timestamp)}</span>
                <span className="event-content">{describeEvent(e)}</span>
              </div>
              {"path" in e && e.path && (
                <button
                  className="btn-tiny-locate"
                  title="在资源管理器中定位"
                  onClick={() => locateFile(e.path)}
                >
                  📁
                </button>
              )}
            </div>
          ))}
        </div>
      )}

      {editing && (
        <FolderSettingsModal
          folder={f}
          onClose={() => setEditing(false)}
          onSaved={onRefreshed}
        />
      )}
    </div>
  );
}

function describeEvent(e: SyncEvent): string {
  switch (e.type) {
    case "cycle_started":
      return `▶ 开始同步`;
    case "scan_done": {
      const hits = (e as any).cacheHits ?? (e as any).cache_hits ?? 0;
      return `🔍 扫描完成：${e.files} 文件（新哈希 ${e.hashed}，缓存命中 ${hits}，忽略 ${e.ignored}）`;
    }
    case "cycle_skipped":
      return `⚡ 增量检查：本地与远端均无变化，已跳过`;
    case "planned": {
      const s: any = e.stats;
      const dl = s.deleteLocal ?? s.delete_local ?? 0;
      const dr = s.deleteRemote ?? s.delete_remote ?? 0;
      return `📋 计划 → 上传 ${s.uploads}，下载 ${s.downloads}，删除本地 ${dl}，删除远端 ${dr}，冲突 ${s.conflicts}，无变化 ${s.unchanged}`;
    }
    case "file_done": {
      const act: Record<string, string> = {
        upload: "↑ 已上传",
        download: "↓ 已下载",
        delete_local: "🗑 已删除本地",
        delete_remote: "🗑 已删除远端",
        conflict: "⚠ 已处理冲突",
        identical: "= 内容一致",
        remote_wins: "↓ 远端恢复（本地曾删除）",
        local_wins: "↑ 本地恢复（远端曾删除）",
      };
      return `${act[e.action] ?? e.action} ${e.path}`;
    }
    case "file_failed": {
      const retry = (e as any).retryInSecs ?? (e as any).retry_in_secs ?? 5;
      return `✗ 失败 ${e.path}：${e.error}（${retry}s 后重试）`;
    }
    case "conflict_created":
      return `⚠ 冲突副本已生成：${e.copy}`;
    case "cycle_done": {
      const rep: any = e.report;
      const took = ((rep.tookMs ?? rep.took_ms ?? 0) / 1000).toFixed(1);
      return rep.skipped > 0
        ? `⚡ 完成（无变化，已跳过 ${took}s）`
        : `✓ 完成 ↑${rep.uploaded} ↓${rep.downloaded} ⚠${rep.conflicts} ✗${rep.failed}（${took}s）`;
    }
    case "error":
      return `✗ 错误：${e.message}`;
    default:
      return JSON.stringify(e);
  }
}
