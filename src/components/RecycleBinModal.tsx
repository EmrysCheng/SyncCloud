import { useState, useEffect } from "react";
import { api } from "../api";
import { FolderSummary, RecycleItem } from "../types";

function formatBytes(bytes: number) {
  if (bytes === 0) return "0 B";
  const k = 1024;
  const sizes = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + " " + sizes[i];
}

function formatDate(iso: string) {
  try {
    const d = new Date(iso);
    return d.toLocaleString("zh-CN", { hour12: false });
  } catch {
    return iso;
  }
}

export default function RecycleBinModal({
  folder,
  onClose,
}: {
  folder: FolderSummary;
  onClose: () => void;
}) {
  const [items, setItems] = useState<RecycleItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [searchTerm, setSearchTerm] = useState("");
  const [typeFilter, setTypeFilter] = useState<"all" | "deleted" | "overwritten">("all");
  const [restoringId, setRestoringId] = useState<string | null>(null);
  const [message, setMessage] = useState<{ text: string; type: "success" | "error" } | null>(null);

  async function loadItems() {
    setLoading(true);
    try {
      const list = await api.listRecycleBin(folder.id);
      setItems(list);
    } catch (e) {
      setMessage({ text: `加载回收站失败: ${e}`, type: "error" });
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    loadItems();
  }, [folder.id]);

  async function handleRestore(item: RecycleItem) {
    if (!window.confirm(`确定要将「${item.originalPath}」还原到同步文件夹原位置吗？`)) {
      return;
    }
    setRestoringId(item.id);
    setMessage(null);
    try {
      const restoredPath = await api.restoreRecycleFile(folder.id, item.id);
      setMessage({ text: `✓ 已成功还原文件: ${restoredPath}`, type: "success" });
      loadItems();
    } catch (e) {
      setMessage({ text: `还原失败: ${e}`, type: "error" });
    } finally {
      setRestoringId(null);
    }
  }

  async function handleDelete(item: RecycleItem) {
    if (!window.confirm(`确定要彻底删除该备份项「${item.originalPath}」吗？`)) {
      return;
    }
    try {
      await api.deleteRecycleItem(folder.id, item.id);
      setItems((prev) => prev.filter((i) => i.id !== item.id));
      setMessage({ text: "已彻底删除该归档项", type: "success" });
    } catch (e) {
      setMessage({ text: `删除失败: ${e}`, type: "error" });
    }
  }

  async function handleCleanAll() {
    if (!window.confirm("确定要清空该文件夹的安全回收站吗？所有归档副本将被彻底删除，无法找回。")) {
      return;
    }
    try {
      const count = await api.cleanRecycleBin(folder.id);
      setItems([]);
      setMessage({ text: `已清空回收站，共清理 ${count} 个归档文件`, type: "success" });
    } catch (e) {
      setMessage({ text: `清空失败: ${e}`, type: "error" });
    }
  }

  async function handleOpenDir() {
    try {
      await api.openRecycleBinDir(folder.id);
    } catch (e) {
      setMessage({ text: `打开目录失败: ${e}`, type: "error" });
    }
  }

  const filtered = items.filter((it) => {
    if (typeFilter !== "all" && it.action !== typeFilter) return false;
    if (searchTerm.trim()) {
      const s = searchTerm.toLowerCase();
      return it.originalPath.toLowerCase().includes(s) || it.fileName.toLowerCase().includes(s);
    }
    return true;
  });

  const totalSize = items.reduce((acc, it) => acc + it.size, 0);

  return (
    <div className="modal-backdrop">
      <div
        className="modal modal-card"
        style={{
          width: "900px",
          maxWidth: "95vw",
          maxHeight: "88vh",
          display: "flex",
          flexDirection: "column",
          padding: 0,
          background: "#ffffff",
          borderRadius: "10px",
          boxShadow: "0 20px 45px rgba(0,0,0,0.2)",
          overflow: "hidden",
        }}
      >
        {/* Modal Header */}
        <div
          style={{
            padding: "16px 20px",
            borderBottom: "1px solid var(--border-color)",
            display: "flex",
            justifyContent: "space-between",
            alignItems: "center",
            background: "#fafbfc",
          }}
        >
          <div>
            <h3 style={{ margin: 0, fontSize: "16px", display: "flex", alignItems: "center", gap: "8px" }}>
              <span>🛡️ 安全防误删回收站</span>
              <span className="badge badge-plain">{folder.name}</span>
            </h3>
            <span className="hint" style={{ fontSize: "12px", marginTop: "2px", display: "block" }}>
              对标 FreeFileSync 与 Syncthing：覆盖与本地删除的文件在此安全保留，绝不污染同步目录，随时一键还原。
            </span>
          </div>
          <button className="btn btn-sm" onClick={onClose}>
            ✕ 关闭
          </button>
        </div>

        {/* Toolbar */}
        <div
          style={{
            padding: "12px 20px",
            borderBottom: "1px solid var(--border-color)",
            display: "flex",
            justifyContent: "space-between",
            alignItems: "center",
            flexWrap: "wrap",
            gap: "10px",
            background: "#fff",
          }}
        >
          <div style={{ display: "flex", gap: "10px", alignItems: "center" }}>
            <input
              type="text"
              placeholder="搜索路径或文件名…"
              value={searchTerm}
              onChange={(e) => setSearchTerm(e.target.value)}
              style={{ width: "220px", padding: "6px 10px", fontSize: "13px" }}
            />
            <div className="btn-group" style={{ display: "flex", gap: "4px" }}>
              <button
                className={`btn btn-sm ${typeFilter === "all" ? "btn-primary" : ""}`}
                onClick={() => setTypeFilter("all")}
              >
                全部 ({items.length})
              </button>
              <button
                className={`btn btn-sm ${typeFilter === "deleted" ? "btn-primary" : ""}`}
                onClick={() => setTypeFilter("deleted")}
              >
                🗑 删除归档 ({items.filter((i) => i.action === "deleted").length})
              </button>
              <button
                className={`btn btn-sm ${typeFilter === "overwritten" ? "btn-primary" : ""}`}
                onClick={() => setTypeFilter("overwritten")}
              >
                📝 覆盖归档 ({items.filter((i) => i.action === "overwritten").length})
              </button>
            </div>
          </div>

          <div style={{ display: "flex", gap: "8px", alignItems: "center" }}>
            <span className="hint" style={{ fontSize: "12px" }}>
              占用: <strong>{formatBytes(totalSize)}</strong>
            </span>
            <button className="btn btn-sm" onClick={handleOpenDir} title="在文件资源管理器中打开物理回收站目录">
              📁 打开目录
            </button>
            <button
              className="btn btn-sm btn-danger"
              disabled={items.length === 0}
              onClick={handleCleanAll}
            >
              清空回收站
            </button>
          </div>
        </div>

        {/* Feedback message banner */}
        {message && (
          <div
            style={{
              padding: "8px 20px",
              fontSize: "13px",
              background: message.type === "success" ? "#e6ffed" : "#ffeef0",
              color: message.type === "success" ? "#22863a" : "#cb2431",
              borderBottom: "1px solid var(--border-color)",
            }}
          >
            {message.text}
          </div>
        )}

        {/* File Table Content */}
        <div style={{ flex: 1, overflowY: "auto", padding: "0 20px" }}>
          {loading ? (
            <div style={{ padding: "40px", textAlign: "center", color: "var(--color-muted)" }}>
              正在加载回收站清单…
            </div>
          ) : filtered.length === 0 ? (
            <div style={{ padding: "48px 20px", textAlign: "center", color: "var(--color-muted)" }}>
              <div style={{ fontSize: "32px", marginBottom: "8px" }}>🍃</div>
              <div>回收站空空如也，未产生任何归档文件</div>
            </div>
          ) : (
            <table className="table" style={{ width: "100%", borderCollapse: "collapse", fontSize: "13px" }}>
              <thead>
                <tr style={{ position: "sticky", top: 0, background: "#f6f8fa", zIndex: 1, borderBottom: "1px solid var(--border-color)" }}>
                  <th style={{ padding: "10px 8px", textAlign: "left", width: "90px" }}>归档类型</th>
                  <th style={{ padding: "10px 8px", textAlign: "left" }}>原相对路径</th>
                  <th style={{ padding: "10px 8px", textAlign: "right", width: "90px" }}>文件大小</th>
                  <th style={{ padding: "10px 8px", textAlign: "left", width: "150px" }}>归档时间</th>
                  <th style={{ padding: "10px 8px", textAlign: "right", width: "140px" }}>操作</th>
                </tr>
              </thead>
              <tbody>
                {filtered.map((item) => (
                  <tr key={item.id} style={{ borderBottom: "1px solid #f0f2f5" }}>
                    <td style={{ padding: "8px" }}>
                      <span
                        className={`badge ${
                          item.action === "deleted" ? "badge-red" : "badge-orange"
                        }`}
                        style={{ fontSize: "11px" }}
                      >
                        {item.action === "deleted" ? "🗑 本地删除" : "📝 覆盖备份"}
                      </span>
                    </td>
                    <td style={{ padding: "8px", wordBreak: "break-all" }}>
                      <code style={{ fontSize: "12px", background: "none", color: "inherit", padding: 0 }}>
                        {item.originalPath}
                      </code>
                    </td>
                    <td style={{ padding: "8px", textAlign: "right", color: "var(--color-muted)" }}>
                      {formatBytes(item.size)}
                    </td>
                    <td style={{ padding: "8px", color: "var(--color-muted)", fontSize: "12px" }}>
                      {formatDate(item.archivedAt)}
                    </td>
                    <td style={{ padding: "8px", textAlign: "right" }}>
                      <button
                        className="btn btn-sm btn-primary"
                        style={{ marginRight: "6px" }}
                        disabled={restoringId === item.id}
                        onClick={() => handleRestore(item)}
                      >
                        {restoringId === item.id ? "还原中…" : "↺ 还原"}
                      </button>
                      <button
                        className="btn btn-sm"
                        style={{ color: "var(--color-danger)" }}
                        onClick={() => handleDelete(item)}
                      >
                        彻底删除
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>

        {/* Footer */}
        <div
          style={{
            padding: "12px 20px",
            borderTop: "1px solid var(--border-color)",
            display: "flex",
            justifyContent: "space-between",
            alignItems: "center",
            background: "#fafbfc",
          }}
        >
          <span className="hint" style={{ fontSize: "12px" }}>
            提示：点击「↺ 还原」将把文件重新恢复到本地原文件夹中，并自动参与后续同步。
          </span>
          <button className="btn" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
