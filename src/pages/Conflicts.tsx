import { useEffect, useMemo, useState } from "react";
import { api } from "../api";
import type { ConflictRecord, FolderSummary } from "../types";

function getFileIcon(path: string): string {
  const ext = path.split(".").pop()?.toLowerCase() || "";
  if (["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"].includes(ext)) return "🖼️";
  if (["doc", "docx", "wps", "txt", "md", "pdf"].includes(ext)) return "📄";
  if (["xls", "xlsx", "csv"].includes(ext)) return "📊";
  if (["ppt", "pptx"].includes(ext)) return "📽️";
  if (["zip", "rar", "7z", "tar", "gz"].includes(ext)) return "📦";
  if (["js", "ts", "tsx", "jsx", "rs", "py", "java", "c", "cpp", "go", "json", "html", "css"].includes(ext)) return "💻";
  return "📁";
}

export default function Conflicts() {
  const [folders, setFolders] = useState<FolderSummary[]>([]);
  const [folderId, setFolderId] = useState<string>("");
  const [records, setRecords] = useState<ConflictRecord[]>([]);
  const [showResolved, setShowResolved] = useState(false);
  const [busyId, setBusyId] = useState<number | null>(null);
  const [batchResolving, setBatchResolving] = useState(false);
  const [keyword, setKeyword] = useState("");

  useEffect(() => {
    api.listFolders().then((fs) => {
      setFolders(fs);
      if (!folderId && fs.length) setFolderId(fs[0].id);
    });
  }, []);

  const refreshConflicts = () => {
    if (folderId) {
      api
        .listConflicts(folderId, showResolved)
        .then(setRecords)
        .catch(() => setRecords([]));
    }
  };

  useEffect(() => {
    refreshConflicts();
  }, [folderId, showResolved]);

  const activeFolder = useMemo(() => {
    return folders.find((f) => f.id === folderId);
  }, [folders, folderId]);

  async function handleResolve(id: number, resolution: string) {
    setBusyId(id);
    try {
      await api.resolveConflict(folderId, id, resolution);
      refreshConflicts();
    } catch (e) {
      alert(`处理冲突失败: ${e}`);
    } finally {
      setBusyId(null);
    }
  }

  async function handleBatchResolve(resolution: "keep_local" | "keep_both") {
    const unresolved = records.filter((r) => !r.resolved);
    if (unresolved.length === 0) return;
    const actionText = resolution === "keep_local" ? "全部保留本地原版（清理副本）" : "全部保留两者（独立同步）";
    if (!window.confirm(`确定要对当前 ${unresolved.length} 个未解决冲突执行「${actionText}」吗？`)) {
      return;
    }
    setBatchResolving(true);
    try {
      for (const r of unresolved) {
        await api.resolveConflict(folderId, r.id, resolution);
      }
      refreshConflicts();
    } catch (e) {
      alert(`批量处理失败: ${e}`);
    } finally {
      setBatchResolving(false);
    }
  }

  function locateFile(relPath?: string | null) {
    if (!activeFolder || !relPath) return;
    const cleanRoot = activeFolder.localPath.replace(/[/\\]+$/, "");
    const cleanRel = relPath.replace(/^[/\\]+/, "");
    const fullPath = `${cleanRoot}\\${cleanRel.replace(/\//g, "\\")}`;
    api.openInExplorer(fullPath, true).catch(() => {
      api.openInExplorer(cleanRoot, false).catch(() => {});
    });
  }

  const filteredRecords = useMemo(() => {
    const kw = keyword.trim().toLowerCase();
    if (!kw) return records;
    return records.filter(
      (r) =>
        r.path.toLowerCase().includes(kw) ||
        (r.conflictCopy && r.conflictCopy.toLowerCase().includes(kw))
    );
  }, [records, keyword]);

  const unresolvedList = useMemo(() => records.filter((r) => !r.resolved), [records]);
  const resolvedList = useMemo(() => records.filter((r) => r.resolved), [records]);

  if (folders.length === 0) {
    return (
      <div className="page">
        <div className="page-header">
          <div>
            <h1>冲突中心</h1>
            <div className="header-sub">多设备并发修改检测与无损安全决策</div>
          </div>
        </div>
        <div className="conflict-empty-guard">
          <div className="guard-shield-icon">🛡️</div>
          <h2>暂无同步任务</h2>
          <p>请先在「同步文件夹」中添加需要同步的目录。当不同电脑发生同时编辑时，SyncCloud 将在此提供一键决策。</p>
        </div>
      </div>
    );
  }

  return (
    <div className="page">
      {/* 顶部标题与控制胶囊 */}
      <div className="page-header">
        <div>
          <div style={{ display: "flex", alignItems: "center", gap: "10px" }}>
            <h1>冲突中心</h1>
            {unresolvedList.length > 0 ? (
              <span className="conflict-chip-alert">
                ⚡ {unresolvedList.length} 项待决断
              </span>
            ) : (
              <span className="conflict-chip-safe">
                ✓ 零冲突对齐
              </span>
            )}
          </div>
          <div className="header-sub">
            多设备并发保护 · 自动另存为隔离副本 · 100% 绝不丢数据
          </div>
        </div>

        <div className="header-actions" style={{ display: "flex", alignItems: "center", gap: "10px", flexWrap: "wrap" }}>
          <div className="folder-select-capsule">
            <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="fsc-icon">
              <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
            </svg>
            <select
              className="select-custom modern-folder-select"
              value={folderId}
              onChange={(e) => setFolderId(e.target.value)}
            >
              {folders.map((f) => (
                <option key={f.id} value={f.id}>
                  {f.name}
                </option>
              ))}
            </select>
          </div>
          <label className="checkbox-pill">
            <input
              type="checkbox"
              checked={showResolved}
              onChange={(e) => setShowResolved(e.target.checked)}
            />
            <span>包含已解决 ({resolvedList.length})</span>
          </label>
        </div>
      </div>

      {/* 现代高质感冲突工具栏（取代旧版粗糙长条） */}
      <div className="conflict-toolbar">
        <div className="conflict-toolbar-left">
          <div className={`conflict-stat-pill ${unresolvedList.length > 0 ? "has-conflicts" : "is-clean"}`}>
            <span className="csp-dot"></span>
            <span className="csp-label">待解决冲突：</span>
            <strong className="csp-val">{unresolvedList.length}</strong>
          </div>
          <div className="conflict-stat-pill is-neutral">
            <span className="csp-label">已保护归档：</span>
            <strong className="csp-val">{resolvedList.length}</strong>
          </div>
          {unresolvedList.length > 1 && (
            <div className="batch-actions-group">
              <button
                className="btn btn-sm btn-ghost"
                disabled={batchResolving}
                onClick={() => handleBatchResolve("keep_local")}
                title="全部保留本地原版，并自动物理清理副本"
              >
                🛡️ 全部保留本地
              </button>
              <button
                className="btn btn-sm btn-ghost"
                disabled={batchResolving}
                onClick={() => handleBatchResolve("keep_both")}
                title="全部保留两端各自独立存在"
              >
                📂 全部保留两者
              </button>
            </div>
          )}
        </div>

        <div className="conflict-toolbar-right">
          <div className="modern-search-box">
            <svg className="msb-icon" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <circle cx="11" cy="11" r="8" />
              <line x1="21" y1="21" x2="16.65" y2="16.65" />
            </svg>
            <input
              type="search"
              className="msb-input"
              placeholder="搜索冲突文件路径或副本…"
              value={keyword}
              onChange={(e) => setKeyword(e.target.value)}
            />
            {keyword && (
              <button
                type="button"
                className="msb-clear"
                title="清空搜索"
                onClick={() => setKeyword("")}
              >
                ✕
              </button>
            )}
          </div>
        </div>
      </div>

      {filteredRecords.length === 0 ? (
        <div className="conflict-empty-guard">
          <div className="guard-shield-glow">
            <span className="guard-shield-icon">🛡️</span>
          </div>
          <h2>{showResolved ? "暂无冲突记录" : "当前无待解决冲突 · 数据安全对齐中"}</h2>
          <p className="guard-desc">
            所有设备的文件变动均已平稳合并。SyncCloud 在检测到冲突时绝不覆盖本地或云端任何文件，而是自动以时间戳副本隔离存储，让您随时回溯与决策。
          </p>
          <div className="guard-features">
            <div className="gf-item">
              <span className="gf-icon">🔒</span>
              <div>
                <strong>零数据丢失</strong>
                <p>双方版本物理独立留存</p>
              </div>
            </div>
            <div className="gf-item">
              <span className="gf-icon">⚡</span>
              <div>
                <strong>即时冲突分流</strong>
                <p>多机同步不中断锁死</p>
              </div>
            </div>
            <div className="gf-item">
              <span className="gf-icon">⚖️</span>
              <div>
                <strong>一键透明决断</strong>
                <p>保留原版、覆盖或双留</p>
              </div>
            </div>
          </div>
        </div>
      ) : (
        <div className="conflict-cards-container">
          {filteredRecords.map((r) => {
            const fileName = r.path.split(/[/\\]/).pop() || r.path;
            const copyName = r.conflictCopy ? r.conflictCopy.split(/[/\\]/).pop() : "未知副本";
            const icon = getFileIcon(r.path);
            const isResolved = r.resolved;

            return (
              <div
                key={r.id}
                className={`conflict-modern-card ${isResolved ? "is-resolved" : ""}`}
              >
                {/* 卡片头部 */}
                <div className="cmc-header">
                  <div className="cmc-title-group">
                    <span className="cmc-file-icon">{icon}</span>
                    <div style={{ minWidth: 0 }}>
                      <div className="cmc-filename" title={fileName}>
                        {fileName}
                      </div>
                      <div className="cmc-filepath" title={r.path}>
                        <code>{r.path}</code>
                      </div>
                    </div>
                  </div>
                  <div className="cmc-badge-group">
                    {isResolved ? (
                      <span className="badge badge-ok">
                        ✓ 已解决：{r.resolution === "keep_local" ? "保留本地原版" : r.resolution === "keep_copy" ? "采用冲突副本" : "两者均保留"}
                      </span>
                    ) : (
                      <span className="badge badge-warn cmc-pulse">
                        ⚡ 并发冲突 · 待决断
                      </span>
                    )}
                  </div>
                </div>

                {/* 左右版本对比主视区 */}
                <div className="cmc-body">
                  <div className="cmc-split-grid">
                    {/* 本地原版卡片 */}
                    <div className="cmc-version-pane pane-local">
                      <div className="pane-tag tag-local">
                        <span>💻 本地基准版本</span>
                        <span className="pane-hash" title={`SHA256: ${r.localHash}`}>
                          #{r.localHash.slice(0, 8)}
                        </span>
                      </div>
                      <div className="pane-file-title" title={r.path}>
                        {fileName}
                      </div>
                      <div className="pane-hint">本机编辑的工作区原文件</div>
                      <button
                        className="btn btn-sm btn-ghost pane-locate-btn"
                        onClick={() => locateFile(r.path)}
                      >
                        📁 定位本地文件
                      </button>
                    </div>

                    {/* 中间对比指示连线 */}
                    <div className="cmc-vs-bridge">
                      <span className="vs-badge">VS</span>
                      <span className="vs-sub">并发修改</span>
                    </div>

                    {/* 冲突副本卡片 */}
                    <div className="cmc-version-pane pane-copy">
                      <div className="pane-tag tag-copy">
                        <span>☁️ 冲突保护副本</span>
                        <span className="pane-hash" title={`SHA256: ${r.remoteHash}`}>
                          #{r.remoteHash.slice(0, 8)}
                        </span>
                      </div>
                      <div className="pane-file-title text-warn" title={r.conflictCopy ?? ""}>
                        {copyName}
                      </div>
                      <div className="pane-hint">由其他设备同步产生的冲突存档</div>
                      {r.conflictCopy && (
                        <button
                          className="btn btn-sm btn-ghost pane-locate-btn"
                          onClick={() => locateFile(r.conflictCopy)}
                        >
                          📁 定位冲突副本
                        </button>
                      )}
                    </div>
                  </div>

                  {/* 决策解决方案按钮组 */}
                  {!isResolved && (
                    <div className="cmc-actions-panel">
                      <div className="cmc-actions-hint">
                        <span>👉 请选择保留决策：</span>
                      </div>
                      <div className="cmc-btn-group">
                        <button
                          className="btn-resolve btn-resolve-local"
                          disabled={busyId === r.id}
                          onClick={() => handleResolve(r.id, "keep_local")}
                          title="保留本地原文件，系统自动物理删除冲突副本"
                        >
                          <div className="br-title">🛡️ 保留本地原版</div>
                          <div className="br-sub">清理冲突副本</div>
                        </button>

                        <button
                          className="btn-resolve btn-resolve-copy"
                          disabled={busyId === r.id}
                          onClick={() => handleResolve(r.id, "keep_copy")}
                          title="以冲突副本覆盖原文件，并清理副本"
                        >
                          <div className="br-title">🔄 采用冲突副本</div>
                          <div className="br-sub">覆盖本地原文件</div>
                        </button>

                        <button
                          className="btn-resolve btn-resolve-both"
                          disabled={busyId === r.id}
                          onClick={() => handleResolve(r.id, "keep_both")}
                          title="两份文件各自独立保留并继续同步"
                        >
                          <div className="br-title">📂 两者均保留</div>
                          <div className="br-sub">各自独立同步</div>
                        </button>
                      </div>
                    </div>
                  )}
                </div>

                {/* 卡片底栏信息 */}
                <div className="cmc-footer">
                  <span>检测时间：{r.createdAt}</span>
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
