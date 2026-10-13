import { useState } from "react";
import { api } from "../api";
import type { ConflictPolicy, Direction, FolderConfig, SyncMode } from "../types";

export default function FolderSettingsModal({
  folder,
  onClose,
  onSaved,
}: {
  folder: FolderConfig;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [name, setName] = useState(folder.name);
  const [direction, setDirection] = useState<Direction>(folder.direction);
  const [conflictPolicy, setConflictPolicy] = useState<ConflictPolicy>(folder.conflictPolicy);
  const [syncMode, setSyncMode] = useState<SyncMode>(folder.syncMode ?? "realtime");
  const [intervalSecs, setIntervalSecs] = useState(folder.intervalSecs ?? 300);
  const [ignoreRulesText, setIgnoreRulesText] = useState((folder.ignoreRules || []).join("\n"));
  const [saving, setSaving] = useState(false);

  async function handleSave() {
    setSaving(true);
    try {
      const rules = ignoreRulesText
        .split("\n")
        .map((l) => l.trim())
        .filter((l) => l.length > 0 && !l.startsWith("#"));

      const updated: FolderConfig = {
        ...folder,
        name: name.trim() || folder.name,
        direction,
        conflictPolicy,
        syncMode,
        intervalSecs: Number(intervalSecs) || 300,
        ignoreRules: rules,
      };

      await api.updateFolder(updated);
      onSaved();
      onClose();
    } catch (e) {
      alert(`保存失败: ${e}`);
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal-card" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h3>⚙ 文件夹设置 · {folder.name}</h3>
          <button className="btn-close" onClick={onClose}>
            ✕
          </button>
        </div>

        <div className="modal-body">
          <div className="field">
            <label>任务名称</label>
            <input
              type="text"
              className="input"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="请输入同步任务名称"
            />
          </div>

          <div className="field">
            <label>同步方向</label>
            <div className="radio-cards">
              <label className={`radio-card ${direction === "bidirectional" ? "selected" : ""}`}>
                <input
                  type="radio"
                  name="direction"
                  value="bidirectional"
                  checked={direction === "bidirectional"}
                  onChange={() => setDirection("bidirectional")}
                />
                <div>
                  <div className="rc-title">⇄ 双向同步</div>
                  <div className="rc-sub">本地与云端双向变动均自动对齐（推荐）</div>
                </div>
              </label>
              <label className={`radio-card ${direction === "upload_only" ? "selected" : ""}`}>
                <input
                  type="radio"
                  name="direction"
                  value="upload_only"
                  checked={direction === "upload_only"}
                  onChange={() => setDirection("upload_only")}
                />
                <div>
                  <div className="rc-title">↑ 仅单向备份（上传）</div>
                  <div className="rc-sub">仅将本地新增和修改上传至云端</div>
                </div>
              </label>
              <label className={`radio-card ${direction === "download_only" ? "selected" : ""}`}>
                <input
                  type="radio"
                  name="direction"
                  value="download_only"
                  checked={direction === "download_only"}
                  onChange={() => setDirection("download_only")}
                />
                <div>
                  <div className="rc-title">↓ 仅单向镜像（下载）</div>
                  <div className="rc-sub">仅将云端内容同步拉取至本地</div>
                </div>
              </label>
            </div>
          </div>

          <div className="field">
            <label>冲突解决策略</label>
            <select
              className="input"
              value={conflictPolicy}
              onChange={(e) => setConflictPolicy(e.target.value as ConflictPolicy)}
            >
              <option value="both_copies">保留两者（生成冲突副本，安全无损，推荐）</option>
              <option value="newest_wins">自动覆盖（以修改时间较新者为准）</option>
            </select>
          </div>

          <div className="field">
            <label>同步触发模式</label>
            <select
              className="input"
              value={syncMode}
              onChange={(e) => setSyncMode(e.target.value as SyncMode)}
            >
              <option value="realtime">实时监听（本地文件变动即刻触发 + 周期扫描兜底）</option>
              <option value="interval">定时间隔轮询</option>
              <option value="manual">仅手动触发</option>
            </select>
          </div>

          {syncMode === "interval" && (
            <div className="field">
              <label>轮询间隔（秒）</label>
              <input
                type="number"
                className="input"
                min="10"
                value={intervalSecs}
                onChange={(e) => setIntervalSecs(Number(e.target.value))}
              />
            </div>
          )}

          <div className="field">
            <label>自定义排除规则（每行一条 Glob 匹配，已默认忽略 .git/node_modules/临时文件等）</label>
            <textarea
              className="input code-area"
              rows={4}
              value={ignoreRulesText}
              onChange={(e) => setIgnoreRulesText(e.target.value)}
              placeholder="例如：&#10;dist&#10;*.log&#10;.cache/**&#10;temp/"
            />
          </div>
        </div>

        <div className="modal-footer">
          <button className="btn" onClick={onClose}>
            取消
          </button>
          <button className="btn btn-primary" disabled={saving} onClick={handleSave}>
            {saving ? "保存中…" : "保存并应用"}
          </button>
        </div>
      </div>
    </div>
  );
}
