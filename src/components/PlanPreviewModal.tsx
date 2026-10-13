import { useMemo, useState } from "react";
import { api } from "../api";
import type { FolderSummary, PlanAction, SyncPlan } from "../types";

function fmtBytes(n?: number): string {
  if (!n || n <= 0) return "0 B";
  if (n < 1024) return `${n} B`;
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
  return `${(n / 1024 ** 3).toFixed(2)} GB`;
}

const ACTION_CONFIG: Record<
  string,
  { label: string; badgeClass: string; desc: string }
> = {
  upload: {
    label: "↑ 待上传",
    badgeClass: "badge-blue",
    desc: "本地新增或修改，将上传至云端",
  },
  download: {
    label: "↓ 待下载",
    badgeClass: "badge-green",
    desc: "云端新增或修改，将下载至本地",
  },
  delete_local: {
    label: "🗑 删除本地",
    badgeClass: "badge-red",
    desc: "云端已删除，本地将同步移入安全回收站",
  },
  delete_remote: {
    label: "🗑 删除远端",
    badgeClass: "badge-orange",
    desc: "本地已删除，云端将同步删除",
  },
  conflict: {
    label: "⚠️ 冲突预警",
    badgeClass: "badge-warn",
    desc: "双向同时修改，将自动生成保护副本",
  },
  identical: {
    label: "= 内容一致",
    badgeClass: "badge-plain",
    desc: "两端内容完全一致，仅记录基线",
  },
  remote_wins_over_delete: {
    label: "↓ 远端恢复",
    badgeClass: "badge-green",
    desc: "本地曾删除但云端已被修改，以云端为准重新下载",
  },
  local_wins_over_delete: {
    label: "↑ 本地恢复",
    badgeClass: "badge-blue",
    desc: "云端曾删除但本地已被修改，重新上传至云端",
  },
};

export default function PlanPreviewModal({
  folder,
  plan,
  onClose,
  onExecuteSync,
}: {
  folder: FolderSummary;
  plan: SyncPlan;
  onClose: () => void;
  onExecuteSync: () => void;
}) {
  const [filter, setFilter] = useState<string>("all");
  const s = plan.stats;

  const filteredActions = useMemo(() => {
    if (filter === "all") return plan.actions;
    if (filter === "upload") {
      return plan.actions.filter(
        (a) => a.kind === "upload" || a.kind === "local_wins_over_delete",
      );
    }
    if (filter === "download") {
      return plan.actions.filter(
        (a) => a.kind === "download" || a.kind === "remote_wins_over_delete",
      );
    }
    if (filter === "delete") {
      return plan.actions.filter(
        (a) => a.kind === "delete_local" || a.kind === "delete_remote",
      );
    }
    if (filter === "conflict") {
      return plan.actions.filter((a) => a.kind === "conflict");
    }
    return plan.actions;
  }, [plan.actions, filter]);

  function locateFile(relPath: string) {
    const cleanRoot = folder.localPath.replace(/[/\\]+$/, "");
    const cleanRel = relPath.replace(/^[/\\]+/, "");
    const fullPath = `${cleanRoot}\\${cleanRel.replace(/\//g, "\\")}`;
    api.openInExplorer(fullPath, true).catch(() => {
      api.openInExplorer(cleanRoot, false).catch(() => {});
    });
  }

  function getActionSize(a: PlanAction): number {
    return a.meta?.size ?? a.local?.size ?? a.remote?.size ?? 0;
  }

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal modal-wide"
        onClick={(e) => e.stopPropagation()}
        style={{ maxWidth: 880 }}
      >
        <div className="modal-header">
          <div>
            <h3>🔍 同步差异分析与执行预览</h3>
            <div className="modal-sub">
              文件夹：<strong>{folder.name}</strong> · 三方对比已完成，未做任何磁盘与云端修改
            </div>
          </div>
          <button className="btn-close" onClick={onClose}>
            ✕
          </button>
        </div>

        <div className="modal-body" style={{ maxHeight: "72vh", overflowY: "auto" }}>
          {/* 汇总统计面板 */}
          <div className="plan-stats-grid">
            <div className="plan-stat-card">
              <span className="p-num stat-blue">{s.uploads}</span>
              <span className="p-lbl">待上传文件</span>
              <span className="p-sub">{fmtBytes(s.bytesUp)}</span>
            </div>
            <div className="plan-stat-card">
              <span className="p-num stat-green">{s.downloads}</span>
              <span className="p-lbl">待下载文件</span>
              <span className="p-sub">{fmtBytes(s.bytesDown)}</span>
            </div>
            <div className="plan-stat-card">
              <span className="p-num stat-orange">{s.deleteLocal + s.deleteRemote}</span>
              <span className="p-lbl">待删除文件</span>
              <span className="p-sub">本地 {s.deleteLocal} / 远端 {s.deleteRemote}</span>
            </div>
            <div className="plan-stat-card">
              <span className="p-num stat-red">{s.conflicts}</span>
              <span className="p-lbl">冲突预警</span>
              <span className="p-sub">{s.conflicts > 0 ? "将生成副本" : "无冲突"}</span>
            </div>
            <div className="plan-stat-card">
              <span className="p-num stat-muted">{s.unchanged}</span>
              <span className="p-lbl">两端无变化</span>
              <span className="p-sub">已保持一致</span>
            </div>
          </div>

          {/* 差异状态过滤条 */}
          <div className="plan-filter-bar">
            <div className="filter-tabs">
              <button
                className={`tab-btn ${filter === "all" ? "active" : ""}`}
                onClick={() => setFilter("all")}
              >
                全部计划 ({plan.actions.length})
              </button>
              <button
                className={`tab-btn ${filter === "upload" ? "active" : ""}`}
                onClick={() => setFilter("upload")}
              >
                待上传 ({s.uploads})
              </button>
              <button
                className={`tab-btn ${filter === "download" ? "active" : ""}`}
                onClick={() => setFilter("download")}
              >
                待下载 ({s.downloads})
              </button>
              <button
                className={`tab-btn ${filter === "delete" ? "active" : ""}`}
                onClick={() => setFilter("delete")}
              >
                待删除 ({s.deleteLocal + s.deleteRemote})
              </button>
              <button
                className={`tab-btn ${filter === "conflict" ? "active" : ""}`}
                onClick={() => setFilter("conflict")}
              >
                冲突 ({s.conflicts})
              </button>
            </div>
          </div>

          {/* 差异清单明细 */}
          {filteredActions.length === 0 ? (
            <div className="empty-state" style={{ padding: "30px 0" }}>
              <div className="empty-icon">✓</div>
              <h3>此分类下无待处理文件</h3>
              <p className="muted">本地与远端在该分类下已完全对齐。</p>
            </div>
          ) : (
            <div className="plan-table-container">
              <table className="plan-table">
                <thead>
                  <tr>
                    <th style={{ width: 110 }}>预期操作</th>
                    <th>相对路径</th>
                    <th style={{ width: 90, textAlign: "right" }}>大小</th>
                    <th style={{ width: 60, textAlign: "center" }}>定位</th>
                  </tr>
                </thead>
                <tbody>
                  {filteredActions.map((a, i) => {
                    const cfg = ACTION_CONFIG[a.kind] ?? {
                      label: a.kind,
                      badgeClass: "badge-plain",
                      desc: a.kind,
                    };
                    const size = getActionSize(a);
                    return (
                      <tr key={i}>
                        <td>
                          <span
                            className={`badge ${cfg.badgeClass}`}
                            title={cfg.desc}
                          >
                            {cfg.label}
                          </span>
                        </td>
                        <td>
                          <div className="plan-path" title={a.path}>
                            {a.path}
                          </div>
                        </td>
                        <td style={{ textAlign: "right" }} className="muted">
                          {size > 0 ? fmtBytes(size) : "-"}
                        </td>
                        <td style={{ textAlign: "center" }}>
                          <button
                            className="btn-tiny-locate"
                            title="在资源管理器中定位"
                            onClick={() => locateFile(a.path)}
                          >
                            📁
                          </button>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </div>

        <div className="modal-footer flex-row-between">
          <div className="muted" style={{ fontSize: 13 }}>
            🛡️ <strong>安全保障：</strong>本地删除或覆盖的文件将自动归档至防误删回收站
          </div>
          <div className="modal-actions">
            <button className="btn" onClick={onClose}>
              取消
            </button>
            <button
              className="btn btn-primary"
              onClick={() => {
                onExecuteSync();
                onClose();
              }}
            >
              ✓ 确认并立即执行同步 ({plan.actions.length} 项)
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
