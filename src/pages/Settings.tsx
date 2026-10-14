import { useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api } from "../api";
import type { AppInfo, CloudAccount, CloudAccountStatus, FolderSummary, GlobalSettings, LogFileInfo } from "../types";

const DEFAULT_SETTINGS: GlobalSettings = {
  concurrency: 3,
  maxRetries: 5,
  fullScanIntervalMin: 15,
  largeFileThreshold: 512 * 1024 * 1024,
  language: "zh-CN",
  closeToTray: true,
  baiduAppKey: "",
  baiduSecretKey: "",
  aliyunAppId: "",
  aliyunAppSecret: "",
  logLevel: "info",
  logToFile: true,
  deletionGuard: true,
  deletionGuardThreshold: 50,
  customLogDir: null,
  activityRetentionDays: 7,
};

const LOG_LEVELS: Array<{ value: string; label: string }> = [
  { value: "error", label: "错误（error）" },
  { value: "warn", label: "警告（warn）" },
  { value: "info", label: "信息（info，推荐）" },
  { value: "debug", label: "调试（debug）" },
  { value: "trace", label: "跟踪（trace，最详细）" },
];

export default function Settings() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [deviceName, setDeviceName] = useState("");
  const [gs, setGs] = useState<GlobalSettings>(DEFAULT_SETTINGS);
  const [autostart, setAutostart] = useState(false);
  const [saved, setSaved] = useState(false);
  const timer = useRef<number | null>(null);

  // 数据目录自定义状态
  const [showDataDirModal, setShowDataDirModal] = useState(false);
  const [pendingDataDir, setPendingDataDir] = useState("");
  const [copyData, setCopyData] = useState(true);
  const [changingDataDir, setChangingDataDir] = useState(false);
  const [dataDirMsg, setDataDirMsg] = useState("");
  const [needRestart, setNeedRestart] = useState(false);

  useEffect(() => {
    loadAppInfo();
    api.getSettings().then(setGs).catch(() => {});
    api.isAutostart().then(setAutostart).catch(() => {});
  }, []);

  function loadAppInfo() {
    api.getAppInfo().then((i) => {
      setInfo(i);
      setDeviceName(i.deviceName);
    });
  }

  function patch(p: Partial<GlobalSettings>) {
    setGs((prev) => ({ ...prev, ...p }));
  }

  /** 打补丁并立即持久化（日志等级/开关等需要即时生效的设置） */
  function patchAndPersist(p: Partial<GlobalSettings>) {
    const next = { ...gs, ...p };
    setGs(next);
    api.saveSettings(deviceName, next).catch(() => {});
  }

  function persist(next?: GlobalSettings) {
    const settings = next ?? gs;
    api.saveSettings(deviceName, settings).catch(() => {});
  }

  function save() {
    api.saveSettings(deviceName, gs).then(() => {
      setSaved(true);
      if (timer.current) window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => setSaved(false), 2000);
    });
  }

  async function toggleAutostart(enabled: boolean) {
    setAutostart(enabled);
    try {
      await api.setAutostart(enabled);
    } catch (e) {
      console.error("切换开机自启动失败:", e);
      api.isAutostart().then(setAutostart).catch(() => {});
    }
  }

  async function pickNewDataDir() {
    const selected = await open({
      directory: true,
      multiple: false,
      title: "选择新的数据存储目录",
      defaultPath: info?.dataDir,
    });
    if (typeof selected === "string" && selected) {
      if (selected === info?.dataDir) {
        alert("选择的目录与当前数据目录相同，无需更改");
        return;
      }
      setPendingDataDir(selected);
      setCopyData(true);
      setDataDirMsg("");
      setShowDataDirModal(true);
    }
  }

  async function restoreDefaultDataDir() {
    if (!info?.defaultDataDir || info.dataDir === info.defaultDataDir) return;
    setPendingDataDir(info.defaultDataDir);
    setCopyData(true);
    setDataDirMsg("");
    setShowDataDirModal(true);
  }

  async function confirmChangeDataDir() {
    if (!pendingDataDir) return;
    setChangingDataDir(true);
    setDataDirMsg("正在迁移并保存数据目录…");
    try {
      await api.changeDataDir(pendingDataDir, copyData);
      setChangingDataDir(false);
      setShowDataDirModal(false);
      setNeedRestart(true);
      loadAppInfo();
    } catch (e) {
      setChangingDataDir(false);
      setDataDirMsg(`修改失败: ${e}`);
    }
  }

  const largeFileMb = Math.round(gs.largeFileThreshold / 1024 / 1024);

  return (
    <div className="page">
      <div className="page-header">
        <h1>设置</h1>
        {saved && <span className="test-ok">✓ 已保存</span>}
      </div>

      <section className="settings-section">
        <h2>系统与常规</h2>
        <label className="field checkbox">
          <input
            type="checkbox"
            checked={autostart}
            onChange={(e) => toggleAutostart(e.target.checked)}
          />
          <span>开机自启动（Windows 登录后自动在后台运行）</span>
        </label>
        <label className="field checkbox">
          <input
            type="checkbox"
            checked={gs.closeToTray}
            onChange={(e) => {
              const next = { ...gs, closeToTray: e.target.checked };
              setGs(next);
              persist(next);
            }}
          />
          <span>关闭窗口时最小化到系统托盘（后台继续同步，右键托盘图标可退出）</span>
        </label>
      </section>

      <section className="settings-section">
        <h2>设备与数据存储</h2>
        <label className="field">
          <span>设备名（出现在冲突副本文件名中）</span>
          <input value={deviceName} onChange={(e) => setDeviceName(e.target.value)} placeholder="例如：书房台式机" />
        </label>
        {info && (
          <div className="kv">
            <div>
              <span className="muted">设备 ID：</span>
              <code>{info.deviceId}</code>
            </div>
            <div>
              <span className="muted">版本：</span>v{info.version}
            </div>
            <div>
              <span className="muted">数据目录：</span>
              <code>{info.dataDir}</code>
              <button
                type="button"
                className="btn btn-sm btn-link"
                style={{ marginLeft: "8px", padding: "0 6px", fontSize: "12px", textDecoration: "underline", background: "transparent", border: "none", cursor: "pointer", color: "var(--primary)" }}
                onClick={() => api.openDataDir()}
              >
                在资源管理器中打开 ↗
              </button>
              {info.dataDir !== info.defaultDataDir ? (
                <span className="auth-badge auth-ok" style={{ marginLeft: "8px", fontSize: "11px" }}>自定义路径</span>
              ) : (
                <span className="auth-badge" style={{ marginLeft: "8px", fontSize: "11px", opacity: 0.7 }}>默认路径</span>
              )}
            </div>
          </div>
        )}

        <div className="row" style={{ marginTop: "12px", gap: "10px" }}>
          <button className="btn" onClick={() => api.openDataDir()}>
            打开数据目录
          </button>
          <button className="btn" onClick={pickNewDataDir}>
            更改数据目录…
          </button>
          {info && info.dataDir !== info.defaultDataDir && (
            <button className="btn" onClick={restoreDefaultDataDir}>
              恢复为默认目录
            </button>
          )}
        </div>

        {showDataDirModal && (
          <div
            className="box-panel"
            style={{
              marginTop: "14px",
              border: "1px solid var(--border-color)",
              padding: "16px",
              borderRadius: "8px",
              background: "var(--bg-secondary, rgba(255, 255, 255, 0.03))",
            }}
          >
            <h3 style={{ margin: "0 0 10px 0", fontSize: "15px" }}>配置数据存储目录</h3>
            <div style={{ fontSize: "13px", lineHeight: "1.6", marginBottom: "12px" }}>
              <div><span className="muted">原数据目录：</span><code>{info?.dataDir}</code></div>
              <div><span className="muted">新数据目录：</span><code>{pendingDataDir}</code></div>
            </div>
            <label className="field checkbox" style={{ marginBottom: "12px" }}>
              <input
                type="checkbox"
                checked={copyData}
                onChange={(e) => setCopyData(e.target.checked)}
              />
              <span>拷贝原数据目录中的数据到新目录（包含配置文件、已绑定账户及同步状态数据库，推荐勾选）</span>
            </label>
            <div className="row" style={{ alignItems: "center", gap: "10px" }}>
              <button className="btn btn-primary" disabled={changingDataDir} onClick={confirmChangeDataDir}>
                {changingDataDir ? "正在迁移数据…" : "确认更改"}
              </button>
              <button className="btn" disabled={changingDataDir} onClick={() => setShowDataDirModal(false)}>
                取消
              </button>
              {dataDirMsg && <span className="hint">{dataDirMsg}</span>}
            </div>
          </div>
        )}

        {needRestart && (
          <div
            className="callout"
            style={{
              marginTop: "14px",
              display: "flex",
              justifyContent: "space-between",
              alignItems: "center",
              border: "1px solid var(--color-success)",
            }}
          >
            <span>✓ 数据目录已成功更改！请重启应用以使用新目录。</span>
            <button className="btn btn-primary" onClick={() => api.restartApp()}>
              立即重启应用
            </button>
          </div>
        )}
      </section>

      <section className="settings-section">
        <h2>同步</h2>
        <div className="grid-3">
          <label className="field">
            <span>传输并发数</span>
            <input type="number" min={1} max={8} value={gs.concurrency} onChange={(e) => patch({ concurrency: Number(e.target.value) || 3 })} />
          </label>
          <label className="field">
            <span>失败重试次数</span>
            <input type="number" min={0} max={20} value={gs.maxRetries} onChange={(e) => patch({ maxRetries: Number(e.target.value) || 5 })} />
          </label>
          <label className="field">
            <span>兜底全量对比间隔（分钟）</span>
            <input type="number" min={1} value={gs.fullScanIntervalMin} onChange={(e) => patch({ fullScanIntervalMin: Number(e.target.value) || 15 })} />
          </label>
        </div>
        <label className="field">
          <span>大文件确认阈值（MB）</span>
          <input type="number" min={0} value={largeFileMb} onChange={(e) => patch({ largeFileThreshold: (Number(e.target.value) || 512) * 1024 * 1024 })} />
        </label>
        <div style={{ marginTop: "14px", padding: "12px", background: "#f8fafc", borderRadius: "8px", border: "1px solid var(--border-color)" }}>
          <label style={{ display: "flex", alignItems: "center", gap: "8px", cursor: "pointer", fontWeight: 500 }}>
            <input
              type="checkbox"
              checked={gs.deletionGuard ?? true}
              onChange={(e) => patch({ deletionGuard: e.target.checked })}
            />
            <span>🛡️ 开启大规模误删安全熔断保护（对标 GoodSync / Syncthing 核心安全机制）</span>
          </label>
          <div className="hint" style={{ marginTop: "4px", marginLeft: "24px" }}>
            当单轮检测到拟删除文件超过阈值（且占原文件总量 20% 以上）时，自动挂起删除动作并发出警报，防止硬盘误拔出或误清空目录导致云端数据被批量抹除。
          </div>
          {(gs.deletionGuard ?? true) && (
            <div style={{ marginTop: "10px", marginLeft: "24px" }}>
              <label className="field" style={{ margin: 0, maxWidth: "240px" }}>
                <span>误删拦截触发数量阈值</span>
                <input
                  type="number"
                  min={5}
                  max={1000}
                  value={gs.deletionGuardThreshold ?? 50}
                  onChange={(e) => patch({ deletionGuardThreshold: Number(e.target.value) || 50 })}
                />
              </label>
            </div>
          )}
        </div>
        <div style={{ marginTop: "14px", padding: "12px", background: "#f8fafc", borderRadius: "8px", border: "1px solid var(--border-color)" }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", flexWrap: "wrap", gap: "10px" }}>
            <div>
              <span style={{ fontWeight: 600, fontSize: "14px" }}>📋 传输历史保留周期与自动清理</span>
              <div className="hint" style={{ marginTop: "2px" }}>
                应用启动及运行时将自动清理超出保留天数的过期传输历史，防止本地数据库过度膨胀。
              </div>
            </div>
            <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
              <select
                className="input input-sm"
                value={gs.activityRetentionDays ?? 7}
                onChange={(e) => patchAndPersist({ activityRetentionDays: Number(e.target.value) })}
              >
                <option value={3}>保留最近 3 天</option>
                <option value={7}>保留最近 7 天 (推荐)</option>
                <option value={14}>保留最近 14 天</option>
                <option value={30}>保留最近 30 天</option>
                <option value={0}>永久保留 (不自动清理)</option>
              </select>
              <button
                className="btn btn-sm"
                onClick={async () => {
                  try {
                    const count = await api.cleanExpiredActivity();
                    alert(`已自动清理 ${count} 条超出保留周期的历史数据。`);
                  } catch (e) {
                    alert(`清理失败: ${e}`);
                  }
                }}
              >
                立即清理过期数据
              </button>
            </div>
          </div>
        </div>
        <p className="hint-line">
          无变化时会自动走增量快路径（不取远端锁、不下载清单、零传输）；
          并发数决定同时上传/下载的文件数。
        </p>
        <button className="btn btn-primary" onClick={save}>
          保存同步设置
        </button>
      </section>

      <CloudAccountsSection />

      <LogSection
        logLevel={gs.logLevel}
        logToFile={gs.logToFile}
        customLogDir={gs.customLogDir}
        onChange={patchAndPersist}
      />

      <section className="settings-section">
        <h2>关于</h2>
        <p className="muted">
          SyncCloud — 多后端文件同步。S3 兼容 / WebDAV / Git / 百度网盘 / 阿里云盘。
          冲突采用 Dropbox 式双方保留策略，远端 manifest + 本地基线三方对比同步。
        </p>
      </section>
    </div>
  );
}

function CloudAccountsSection() {
  const [accounts, setAccounts] = useState<CloudAccount[]>([]);
  const [folders, setFolders] = useState<FolderSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [statusMap, setStatusMap] = useState<Record<string, CloudAccountStatus | "checking">>({});

  useEffect(() => {
    loadAccounts();
    api.listFolders().then(setFolders).catch(() => {});
  }, []);

  function loadAccounts() {
    setLoading(true);
    api.listCloudAccounts()
      .then(setAccounts)
      .catch(() => {})
      .finally(() => setLoading(false));
  }

  async function probeAccount(id: string) {
    setStatusMap((prev) => ({ ...prev, [id]: "checking" }));
    try {
      const status = await api.cloudAccountStatus(id);
      setStatusMap((prev) => ({ ...prev, [id]: status }));
    } catch (e) {
      setStatusMap((prev) => ({
        ...prev,
        [id]: {
          accountId: id,
          authorized: false,
          ok: false,
          detail: String(e),
          checkedAt: new Date().toISOString(),
        },
      }));
    }
  }

  async function handleDelete(id: string, name: string) {
    if (!window.confirm(`确定要移除云盘账户「${name}」的授权记录吗？\n已配置该账户的同步文件夹可能需要重新授权。`)) {
      return;
    }
    try {
      await api.deleteCloudAccount(id);
      loadAccounts();
    } catch (e) {
      alert(`移除失败: ${e}`);
    }
  }

  function boundFolderCount(accId: string): number {
    return folders.filter((f) => f.backend.authId === accId).length;
  }

  return (
    <section className="settings-section">
      <h2>已授权网盘账户</h2>
      <p className="muted">
        SyncCloud 支持按同步文件夹分别绑定不同的百度网盘或阿里云盘账户。
        可随时检测授权的实时可用性；令牌保存在 Windows 凭据管理器中。
      </p>

      {loading ? (
        <div className="hint">正在加载账户…</div>
      ) : accounts.length === 0 ? (
        <div className="callout">
          暂无已授权的网盘账户。添加同步文件夹时，选择百度网盘或阿里云盘即可直接授权并绑定。
        </div>
      ) : (
        <div className="accounts-list" style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
          {accounts.map((acc) => {
            const status = statusMap[acc.id];
            const bound = boundFolderCount(acc.id);
            return (
              <div
                key={acc.id}
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  alignItems: "center",
                  padding: "10px 14px",
                  borderRadius: "6px",
                  border: "1px solid var(--border-color)",
                  background: "var(--bg-secondary, rgba(255, 255, 255, 0.03))",
                }}
              >
                <div style={{ minWidth: 0 }}>
                  <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                    <span style={{ fontWeight: 600 }}>{acc.name}</span>
                    <span className="auth-badge auth-ok" style={{ fontSize: "12px" }}>
                      {acc.provider === "baidu" ? "百度网盘" : "阿里云盘"}
                    </span>
                    {bound > 0 && (
                      <span className="auth-badge" style={{ fontSize: "11px", opacity: 0.8 }}>
                        绑定 {bound} 个同步任务
                      </span>
                    )}
                    {status === "checking" && (
                      <span className="auth-badge" style={{ fontSize: "11px" }}>检测中…</span>
                    )}
                    {status && status !== "checking" && (
                      <span className={`auth-badge ${status.ok ? "auth-ok" : "auth-bad"}`} style={{ fontSize: "11px" }}>
                        {status.ok ? "✓ 连接正常" : "✗ 连接异常"}
                      </span>
                    )}
                  </div>
                  <div className="muted" style={{ fontSize: "12px", marginTop: "4px" }}>
                    授权时间: {new Date(acc.createdAt).toLocaleString()}
                    {status && status !== "checking" && (
                      <> · 上次检测: {new Date(status.checkedAt).toLocaleTimeString("zh-CN", { hour12: false })}</>
                    )}
                  </div>
                  {status && status !== "checking" && !status.ok && (
                    <div style={{ fontSize: "12px", color: "var(--color-danger, #e5484d)", marginTop: "2px" }}>
                      {status.detail}
                    </div>
                  )}
                </div>
                <div style={{ display: "flex", gap: "6px", flexShrink: 0 }}>
                  <button
                    className="btn"
                    style={{ fontSize: "12px", padding: "4px 8px" }}
                    disabled={status === "checking"}
                    onClick={() => probeAccount(acc.id)}
                  >
                    检测连接
                  </button>
                  <button
                    className="btn btn-danger"
                    style={{ fontSize: "12px", padding: "4px 8px" }}
                    onClick={() => handleDelete(acc.id, acc.name)}
                  >
                    解除授权
                  </button>
                </div>
              </div>
            );
          })}
        </div>
      )}
    </section>
  );
}

function LogSection({
  logLevel,
  logToFile,
  customLogDir,
  onChange,
}: {
  logLevel: string;
  logToFile: boolean;
  customLogDir?: string | null;
  onChange: (patch: Partial<GlobalSettings>) => void;
}) {
  const [files, setFiles] = useState<LogFileInfo[]>([]);
  const [viewing, setViewing] = useState<string | null>(null);
  const [lines, setLines] = useState<string[]>([]);
  const [loadingTail, setLoadingTail] = useState(false);
  const [currentDir, setCurrentDir] = useState<string>("");

  function refreshFiles() {
    api.listLogFiles().then(setFiles).catch(() => {});
    api.getCurrentLogDir().then(setCurrentDir).catch(() => {});
  }

  useEffect(() => {
    refreshFiles();
  }, [customLogDir]);

  async function openViewer(name: string) {
    setViewing(name);
    setLoadingTail(true);
    try {
      const tail = await api.readLogTail(name, 300);
      setLines(tail);
    } catch (e) {
      setLines([`读取日志失败: ${e}`]);
    } finally {
      setLoadingTail(false);
    }
  }

  async function chooseCustomDir() {
    try {
      const selected = await open({
        directory: true,
        multiple: false,
        title: "选择日志输出目录",
      });
      if (typeof selected === "string" && selected.trim()) {
        onChange({ customLogDir: selected.trim() });
      }
    } catch (e) {
      alert(`选择目录失败: ${e}`);
    }
  }

  function formatSize(size: number): string {
    if (size >= 1024 * 1024) return `${(size / 1024 / 1024).toFixed(1)} MB`;
    if (size >= 1024) return `${(size / 1024).toFixed(1)} KB`;
    return `${size} B`;
  }

  return (
    <section className="settings-section">
      <h2>日志与诊断</h2>
      <div className="grid-3">
        <label className="field">
          <span>日志等级</span>
          <select value={logLevel} onChange={(e) => onChange({ logLevel: e.target.value })}>
            {LOG_LEVELS.map((l) => (
              <option key={l.value} value={l.value}>
                {l.label}
              </option>
            ))}
          </select>
        </label>
      </div>
      <label className="field checkbox">
        <input
          type="checkbox"
          checked={logToFile}
          onChange={(e) => onChange({ logToFile: e.target.checked })}
        />
        <span>输出文件日志（默认创建到程序所在目录 logs/，不放到数据目录中，单文件超 5MB 自动轮转）</span>
      </label>

      <div style={{ marginTop: "12px", padding: "12px 14px", background: "var(--bg-secondary, rgba(255, 255, 255, 0.03))", borderRadius: "8px", border: "1px solid var(--border-color)" }}>
        <div style={{ fontSize: "13px", fontWeight: 600, marginBottom: "8px" }}>
          📁 日志存储目录
        </div>
        <div style={{ fontSize: "13px", marginBottom: "10px", wordBreak: "break-all" }}>
          <span className="muted">当前生效路径：</span>
          <code style={{ background: "rgba(0,0,0,0.06)", padding: "2px 6px", borderRadius: "4px" }}>
            {currentDir || "检测中…"}
          </code>
        </div>
        <div style={{ display: "flex", alignItems: "center", gap: "10px", flexWrap: "wrap" }}>
          <button className="btn btn-sm" onClick={chooseCustomDir}>
            📂 自定义日志目录…
          </button>
          {customLogDir && (
            <button
              className="btn btn-sm btn-ghost"
              onClick={() => onChange({ customLogDir: null })}
              title="清除自定义目录，恢复为默认的程序所在目录"
            >
              ⟲ 恢复为默认程序目录
            </button>
          )}
          <button className="btn btn-sm" onClick={() => api.openLogDir().catch(() => {})}>
            在资源管理器中打开
          </button>
        </div>
        {customLogDir && (
          <div className="hint" style={{ marginTop: "8px" }}>
            已设置自定义日志目录：<code>{customLogDir}</code>
          </div>
        )}
      </div>

      <p className="hint-line">
        等级、文件输出开关及存储目录保存后立即生效，无需重启。日志记录所有同步轮次、传输明细、重试与错误。
      </p>

      <div className="row" style={{ marginTop: "10px", gap: "10px", alignItems: "center" }}>
        <button className="btn" onClick={refreshFiles}>
          刷新日志文件列表
        </button>
        {viewing && (
          <button className="btn" onClick={() => openViewer(viewing)} disabled={loadingTail}>
            {loadingTail ? "读取中…" : "刷新内容"}
          </button>
        )}
      </div>

      {files.length > 0 && (
        <div style={{ marginTop: "12px", display: "flex", flexDirection: "column", gap: "6px" }}>
          {files.map((f) => (
            <button
              key={f.name}
              className={`log-file-row ${viewing === f.name ? "active" : ""}`}
              onClick={() => openViewer(f.name)}
            >
              <span className="log-file-name">📄 {f.name}</span>
              <span className="muted" style={{ fontSize: "12px" }}>
                {formatSize(f.size)} · {f.modified}
              </span>
            </button>
          ))}
        </div>
      )}

      {viewing && (
        <div className="log-viewer">
          <div className="log-viewer-head">
            <span>{viewing} · 最近 300 行</span>
          </div>
          <pre className="log-viewer-body">
            {loadingTail ? "读取中…" : lines.length ? lines.join("\n") : "（空）"}
          </pre>
        </div>
      )}
    </section>
  );
}
