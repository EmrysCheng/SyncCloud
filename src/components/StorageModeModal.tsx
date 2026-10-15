import { useEffect, useState } from "react";
import type { FolderSummary } from "../types";

export default function StorageModeModal({
  folder,
  onClose,
  onApply,
}: {
  folder: FolderSummary;
  onClose: () => void;
  onApply: (mode: "keep_offline" | "free_space") => void;
}) {
  const [mode, setMode] = useState<"free_space" | "keep_offline">("free_space");

  // 监听 ESC 键关闭
  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") onClose();
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [onClose]);

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal-card"
        onClick={(e) => e.stopPropagation()}
        style={{ maxWidth: "520px" }}
      >
        <div className="modal-header">
          <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
            <span style={{ fontSize: "20px" }}>☁️</span>
            <div>
              <h3>按需存储管理 · 坚果云同款云桥</h3>
              <div className="modal-sub">
                切换本地文件存储方式，支持一键释放电脑磁盘空间或全量离线
              </div>
            </div>
          </div>
          <button className="btn-close" onClick={onClose} title="关闭">
            ✕
          </button>
        </div>

        <div className="modal-body" style={{ padding: "18px 22px", gap: "14px" }}>
          {/* 任务目标卡片 */}
          <div className="force-push-target-card" style={{ gridTemplateColumns: "1fr" }}>
            <div className="target-item">
              <span className="target-label">同步任务：</span>
              <strong className="target-value">{folder.name}</strong>
            </div>
            <div className="target-item">
              <span className="target-label">本地目录：</span>
              <code className="target-path" title={folder.localPath}>
                {folder.localPath}
              </code>
            </div>
          </div>

          {/* 两种存储方式选项卡片 */}
          <div style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
            <label
              className={`rule-item ${mode === "free_space" ? "rule-block" : ""}`}
              style={{
                cursor: "pointer",
                padding: "12px 14px",
                borderWidth: mode === "free_space" ? "2px" : "1px",
              }}
            >
              <input
                type="radio"
                name="storage_mode"
                checked={mode === "free_space"}
                onChange={() => setMode("free_space")}
                style={{ marginTop: "3px" }}
              />
              <div className="rule-content">
                <div className="rule-title" style={{ fontSize: "13px", display: "flex", alignItems: "center", gap: "6px" }}>
                  <span>☁️ 释放本地空间 (不占用电脑空间)</span>
                  {mode === "free_space" && (
                    <span className="badge badge-green" style={{ fontSize: "10px" }}>推荐</span>
                  )}
                </div>
                <div className="rule-desc" style={{ marginTop: "4px" }}>
                  文件在云端保留完整数据，本地转换为 <strong>0 字节脱机占位符</strong>并标记云朵图标，<strong>彻底释放本地硬盘空间</strong>。
                </div>
              </div>
            </label>

            <label
              className={`rule-item ${mode === "keep_offline" ? "rule-up" : ""}`}
              style={{
                cursor: "pointer",
                padding: "12px 14px",
                borderWidth: mode === "keep_offline" ? "2px" : "1px",
              }}
            >
              <input
                type="radio"
                name="storage_mode"
                checked={mode === "keep_offline"}
                onChange={() => setMode("keep_offline")}
                style={{ marginTop: "3px" }}
              />
              <div className="rule-content">
                <div className="rule-title" style={{ fontSize: "13px" }}>
                  📥 保持离线可用 (完整保存在本地)
                </div>
                <div className="rule-desc" style={{ marginTop: "4px" }}>
                  从云端<strong>完整下载恢复文件全部物理内容</strong>，清除脱机标记，确保断网离线时随时打开与编辑。
                </div>
              </div>
            </label>
          </div>

          <div
            style={{
              fontSize: "12px",
              color: "var(--muted)",
              background: "var(--bg-soft)",
              padding: "8px 12px",
              borderRadius: "6px",
              lineHeight: 1.45,
            }}
          >
            💡 <strong>系统右键联动：</strong>您也可以随时在 Windows 文件资源管理器中右击任意文件或文件夹，选择 <code>SyncCloud &gt; 保持离线可用</code> 或 <code>不占用电脑空间</code> 进行单项调整。
          </div>
        </div>

        <div className="modal-footer flex-row-between" style={{ padding: "12px 20px" }}>
          <button className="btn btn-sm" onClick={onClose}>
            取消
          </button>
          <button
            className="btn btn-sm btn-primary"
            onClick={() => {
              onApply(mode);
              onClose();
            }}
            style={{ fontWeight: 600, padding: "6px 18px" }}
          >
            确认应用此存储方式
          </button>
        </div>
      </div>
    </div>
  );
}
