import { useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api } from "../api";
import type { AppInfo, CloudAccount, GlobalSettings } from "../types";

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
};

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
              {info.dataDir !== info.defaultDataDir ? (
                <span className="auth-badge auth-ok" style={{ marginLeft: "8px", fontSize: "11px" }}>自定义路径</span>
              ) : (
                <span className="auth-badge" style={{ marginLeft: "8px", fontSize: "11px", opacity: 0.7 }}>默认路径</span>
              )}
            </div>
          </div>
        )}

        <div className="row" style={{ marginTop: "12px", gap: "10px" }}>
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
        <p className="hint-line">
          无变化时会自动走增量快路径（不取远端锁、不下载清单、零传输）；
          并发数决定同时上传/下载的文件数。
        </p>
        <button className="btn btn-primary" onClick={save}>
          保存同步设置
        </button>
      </section>

      <CloudAccountsSection />

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
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    loadAccounts();
  }, []);

  function loadAccounts() {
    setLoading(true);
    api.listCloudAccounts()
      .then(setAccounts)
      .catch(() => {})
      .finally(() => setLoading(false));
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

  return (
    <section className="settings-section">
      <h2>已授权网盘账户</h2>
      <p className="muted">
        SyncCloud 支持按同步文件夹分别绑定不同的百度网盘或阿里云盘账户。
        授权步骤已移至「添加同步文件夹」向导中，无需在此处统一配置。
      </p>

      {loading ? (
        <div className="hint">正在加载账户…</div>
      ) : accounts.length === 0 ? (
        <div className="callout">
          暂无已授权的网盘账户。添加同步文件夹时，选择百度网盘或阿里云盘即可直接授权并绑定。
        </div>
      ) : (
        <div className="accounts-list" style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
          {accounts.map((acc) => (
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
              <div>
                <span style={{ fontWeight: 600, marginRight: "8px" }}>{acc.name}</span>
                <span className="auth-badge auth-ok" style={{ fontSize: "12px", marginRight: "8px" }}>
                  {acc.provider === "baidu" ? "百度网盘" : "阿里云盘"}
                </span>
                <span className="muted" style={{ fontSize: "12px" }}>
                  授权时间: {new Date(acc.createdAt).toLocaleString()}
                </span>
              </div>
              <button
                className="btn btn-danger"
                style={{ fontSize: "12px", padding: "4px 8px" }}
                onClick={() => handleDelete(acc.id, acc.name)}
              >
                解除授权
              </button>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}
