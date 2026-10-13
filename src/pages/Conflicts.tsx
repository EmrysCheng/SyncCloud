import { useEffect, useMemo, useState } from "react";
import { api } from "../api";
import type { ConflictRecord, FolderSummary } from "../types";

export default function Conflicts() {
  const [folders, setFolders] = useState<FolderSummary[]>([]);
  const [folderId, setFolderId] = useState<string>("");
  const [records, setRecords] = useState<ConflictRecord[]>([]);
  const [showResolved, setShowResolved] = useState(false);
  const [busyId, setBusyId] = useState<number | null>(null);

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

  function locateFile(relPath?: string | null) {
    if (!activeFolder || !relPath) return;
    const cleanRoot = activeFolder.localPath.replace(/[/\\]+$/, "");
    const cleanRel = relPath.replace(/^[/\\]+/, "");
    const fullPath = `${cleanRoot}\\${cleanRel.replace(/\//g, "\\")}`;
    api.openInExplorer(fullPath, true).catch(() => {
      api.openInExplorer(cleanRoot, false).catch(() => {});
    });
  }

  const unresolvedCount = records.filter((r) => !r.resolved).length;

  if (folders.length === 0) {
    return (
      <div className="page">
        <div className="page-header">
          <h1>冲突中心</h1>
        </div>
        <div className="empty-state">
          <div className="empty-icon">⚠️</div>
          <h2>暂无同步任务</h2>
          <p>请先添加同步文件夹。多设备同时修改相同文件时，将在此处提供一键安全解决。</p>
        </div>
      </div>
    );
  }

  return (
    <div className="page">
      <div className="page-header">
        <div>
          <h1>冲突中心</h1>
          <div className="header-sub">
            多设备同时修改冲突检测 · 一键无损解决与文件定位
          </div>
        </div>
        <div className="header-actions">
          <select
            className="input input-sm"
            value={folderId}
            onChange={(e) => setFolderId(e.target.value)}
          >
            {folders.map((f) => (
              <option key={f.id} value={f.id}>
                📂 {f.name}
              </option>
            ))}
          </select>
          <label className="checkbox-inline">
            <input
              type="checkbox"
              checked={showResolved}
              onChange={(e) => setShowResolved(e.target.checked)}
            />
            包含已解决
          </label>
        </div>
      </div>

      <div className="conflict-banner">
        <div className="banner-icon">ℹ️</div>
        <div>
          <strong>冲突保护机制：</strong>
          当多台设备同时修改同一文件时，SyncCloud 会自动保留远端原名，并将本机修改另存为「(冲突副本 - 设备名 时间戳)」文件，<strong>绝不丢失任何一方的数据</strong>。您可在下方直接对比并一键决定保留策略。
        </div>
      </div>

      {records.length === 0 ? (
        <div className="empty-state">
          <div className="empty-icon">🎉</div>
          <h2>{showResolved ? "暂无冲突记录" : "当前无未解决冲突"}</h2>
          <p>所有文件均已安全同步对齐。如果不同设备发生并发编辑，冲突副本将在此处列出。</p>
        </div>
      ) : (
        <div className="conflict-list">
          {records.map((r) => {
            const fileName = r.path.split(/[/\\]/).pop();
            const copyName = r.conflictCopy ? r.conflictCopy.split(/[/\\]/).pop() : null;
            return (
              <div key={r.id} className={`conflict-card ${r.resolved ? "resolved" : ""}`}>
                <div className="conflict-card-head">
                  <div className="conflict-title-col">
                    <span className="conflict-icon">⚠️</span>
                    <div>
                      <div className="conflict-filename">{fileName}</div>
                      <div className="conflict-filepath">{r.path}</div>
                    </div>
                  </div>
                  <div>
                    {r.resolved ? (
                      <span className="badge badge-ok">✓ 已解决：{r.resolution}</span>
                    ) : (
                      <span className="badge badge-warn">待解决</span>
                    )}
                  </div>
                </div>

                <div className="conflict-card-body">
                  <div className="conflict-versions">
                    <div className="version-box">
                      <div className="ver-tag">本地原文件</div>
                      <div className="ver-name" title={r.path}>{fileName}</div>
                      <div className="ver-meta">指纹: {r.localHash.slice(0, 10)}…</div>
                      <button
                        className="btn btn-sm btn-ghost"
                        onClick={() => locateFile(r.path)}
                      >
                        📁 定位原文件
                      </button>
                    </div>

                    <div className="version-sep">⚡ 并发修改 ⚡</div>

                    <div className="version-box">
                      <div className="ver-tag ver-tag-copy">冲突保护副本</div>
                      <div className="ver-name" title={r.conflictCopy ?? ""}>
                        {copyName ?? "未指定副本"}
                      </div>
                      <div className="ver-meta">指纹: {r.remoteHash.slice(0, 10)}…</div>
                      {r.conflictCopy && (
                        <button
                          className="btn btn-sm btn-ghost"
                          onClick={() => locateFile(r.conflictCopy)}
                        >
                          📁 定位冲突副本
                        </button>
                      )}
                    </div>
                  </div>

                  {!r.resolved && (
                    <div className="conflict-actions-bar">
                      <span className="act-hint">选择解决方案：</span>
                      <button
                        className="btn btn-sm"
                        disabled={busyId === r.id}
                        title="保留本地原文件，系统自动物理删除冲突副本"
                        onClick={() => handleResolve(r.id, "keep_local")}
                      >
                        保留本地原版（清理副本）
                      </button>
                      <button
                        className="btn btn-sm"
                        disabled={busyId === r.id}
                        title="以冲突副本为准，自动重命名覆盖原文件并清理副本"
                        onClick={() => handleResolve(r.id, "keep_copy")}
                      >
                        保留冲突副本（覆盖原版）
                      </button>
                      <button
                        className="btn btn-sm btn-ghost"
                        disabled={busyId === r.id}
                        title="保留两份文件各自独立存在并继续同步"
                        onClick={() => handleResolve(r.id, "keep_both")}
                      >
                        保留两者
                      </button>
                    </div>
                  )}
                </div>

                <div className="conflict-card-foot">
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
