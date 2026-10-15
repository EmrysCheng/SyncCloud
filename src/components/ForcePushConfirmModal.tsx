import { useEffect, useState } from "react";
import type { FolderSummary } from "../types";

export default function ForcePushConfirmModal({
  folder,
  onClose,
  onConfirm,
}: {
  folder: FolderSummary;
  onClose: () => void;
  onConfirm: () => void;
}) {
  const [checked, setChecked] = useState(false);

  // 监听 ESC 键关闭
  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") {
        onClose();
      }
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [onClose]);

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal-card modal-force-push"
        onClick={(e) => e.stopPropagation()}
        style={{ maxWidth: "560px" }}
      >
        <div className="modal-header">
          <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
            <span style={{ fontSize: "20px" }}>🚀</span>
            <div>
              <h3>强制推送 · 本地覆盖远端</h3>
              <div className="modal-sub">
                将以本地文件为唯一事实标准，单向镜像覆盖云端存储
              </div>
            </div>
          </div>
          <button className="btn-close" onClick={onClose} title="关闭">
            ✕
          </button>
        </div>

        <div className="modal-body" style={{ padding: "20px 24px", gap: "16px" }}>
          {/* 醒目警告横幅 */}
          <div className="force-push-alert-card">
            <div className="alert-icon-wrap">
              <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Z" />
                <line x1="12" y1="9" x2="12" y2="13" />
                <line x1="12" y1="17" x2="12.01" y2="17" />
              </svg>
            </div>
            <div>
              <div className="alert-title">此操作将绕过安全防误删拦截，直接修改云端数据</div>
              <div className="alert-desc">
                适用于本地已完成重构、归档或批量删除，需要彻底清空云端多余历史残留并重新拉齐的场景。
              </div>
            </div>
          </div>

          {/* 任务目标卡片 */}
          <div className="force-push-target-card">
            <div className="target-item">
              <span className="target-label">同步任务：</span>
              <strong className="target-value">{folder.name}</strong>
            </div>
            <div className="target-item">
              <span className="target-label">存储类型：</span>
              <span className="badge badge-plain">{folder.backend.type}</span>
            </div>
            <div className="target-item" style={{ gridColumn: "1 / -1" }}>
              <span className="target-label">本地目录：</span>
              <code className="target-path" title={folder.localPath}>{folder.localPath}</code>
            </div>
          </div>

          {/* 核心执行准则清单 */}
          <div className="force-push-rules">
            <div className="rule-item rule-del">
              <div className="rule-icon">🗑️</div>
              <div className="rule-content">
                <div className="rule-title">云端多余文件彻底清除</div>
                <div className="rule-desc">
                  本地已删除或不存在的文件，将<strong>直接从云端永久删除</strong>（绕过防误删熔断保护限制）。
                </div>
              </div>
            </div>

            <div className="rule-item rule-up">
              <div className="rule-icon">⬆️</div>
              <div className="rule-content">
                <div className="rule-title">本地内容完整推送覆盖</div>
                <div className="rule-desc">
                  本地当前存在且有改动的文件，将<strong>完整推送到云端</strong>，覆盖云端旧版本。
                </div>
              </div>
            </div>

            <div className="rule-item rule-block">
              <div className="rule-icon">🛡️</div>
              <div className="rule-content">
                <div className="rule-title">绝不反向拉取下载</div>
                <div className="rule-desc">
                  单向镜像保护机制，<strong>绝不从云端向本地下载任何文件</strong>，本地目录保持绝对纯净。
                </div>
              </div>
            </div>
          </div>

          {/* 安全确认勾选 */}
          <label className="force-push-confirm-check">
            <input
              type="checkbox"
              checked={checked}
              onChange={(e) => setChecked(e.target.checked)}
            />
            <span>我已核对本地文件无误，确认以此目录状态强制覆盖云端数据</span>
          </label>
        </div>

        <div className="modal-footer flex-row-between" style={{ padding: "12px 20px" }}>
          <div className="muted" style={{ fontSize: "12px" }}>
            提示：执行完成后，任务的常规同步设置依然保持不变
          </div>
          <div style={{ display: "flex", gap: "10px" }}>
            <button className="btn btn-sm" onClick={onClose}>
              取消
            </button>
            <button
              className="btn btn-sm btn-danger"
              disabled={!checked}
              onClick={onConfirm}
              style={{
                opacity: checked ? 1 : 0.6,
                cursor: checked ? "pointer" : "not-allowed",
                fontWeight: 600,
                padding: "6px 16px",
              }}
            >
              🚀 确认强制覆盖云端
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
