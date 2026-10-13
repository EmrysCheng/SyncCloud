import { useEffect, useState } from "react";
import { api } from "../api";
import type { ConflictRecord, FolderSummary } from "../types";

export default function Conflicts() {
  const [folders, setFolders] = useState<FolderSummary[]>([]);
  const [folderId, setFolderId] = useState<string>("");
  const [records, setRecords] = useState<ConflictRecord[]>([]);
  const [showResolved, setShowResolved] = useState(false);

  useEffect(() => {
    api.listFolders().then((fs) => {
      setFolders(fs);
      if (!folderId && fs.length) setFolderId(fs[0].id);
    });
  }, []);

  useEffect(() => {
    if (folderId) {
      api.listConflicts(folderId, showResolved).then(setRecords).catch(() => setRecords([]));
    }
  }, [folderId, showResolved]);

  async function resolve(id: number, resolution: string) {
    await api.resolveConflict(folderId, id, resolution);
    api.listConflicts(folderId, showResolved).then(setRecords);
  }

  if (folders.length === 0) {
    return (
      <div className="page">
        <h1>冲突中心</h1>
        <div className="empty-state small">
          <p>暂无同步文件夹。冲突记录将在多台设备同时修改同一文件后出现。</p>
        </div>
      </div>
    );
  }

  return (
    <div className="page">
      <div className="page-header">
        <h1>冲突中心</h1>
        <div className="row">
          <select value={folderId} onChange={(e) => setFolderId(e.target.value)}>
            {folders.map((f) => (
              <option key={f.id} value={f.id}>
                {f.name}
              </option>
            ))}
          </select>
          <label className="checkbox-inline">
            <input
              type="checkbox"
              checked={showResolved}
              onChange={(e) => setShowResolved(e.target.checked)}
            />
            显示已解决
          </label>
        </div>
      </div>
      <p className="muted">
        冲突发生时（多台设备同时修改同一文件），远端版本保留原名，本机修改自动保存为
        「(冲突副本 - 设备名 日期)」文件，两个版本都会保留并同步。此处可标记处理结果。
      </p>
      {records.length === 0 ? (
        <div className="empty-state small">
          <p>{showResolved ? "没有冲突记录" : "🎉 没有未处理的冲突"}</p>
        </div>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>文件</th>
              <th>冲突副本</th>
              <th>发生时间</th>
              <th>状态</th>
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            {records.map((r) => (
              <tr key={r.id} className={r.resolved ? "resolved" : ""}>
                <td title={`本地 ${r.localHash.slice(0, 12)} / 远端 ${r.remoteHash.slice(0, 12)}`}>{r.path}</td>
                <td>{r.conflictCopy ?? "-"}</td>
                <td>{r.createdAt}</td>
                <td>
                  {r.resolved ? (
                    <span className="badge badge-ok">已解决（{r.resolution}）</span>
                  ) : (
                    <span className="badge badge-warn">待处理</span>
                  )}
                </td>
                <td>
                  {!r.resolved && (
                    <div className="row">
                      <button className="btn btn-sm" onClick={() => resolve(r.id, "保留两版本")}>
                        保留两版本
                      </button>
                      <button className="btn btn-sm" onClick={() => resolve(r.id, "已手动处理")}>
                        已手动处理
                      </button>
                    </div>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
