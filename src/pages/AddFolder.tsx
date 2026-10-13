import { useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import QRCode from "qrcode";
import { api } from "../api";
import type { BackendConfig, CloudAccount, BaiduLoginInfo, AliyunQrInfo } from "../types";

type BackendKind = "s3" | "webdav" | "git" | "baidu" | "aliyun";

const BACKENDS: { kind: BackendKind; icon: string; title: string; desc: string }[] = [
  { kind: "s3", icon: "🪣", title: "S3 兼容存储", desc: "AWS S3 / MinIO / 阿里 OSS / 腾讯 COS / Cloudflare R2 / B2" },
  { kind: "webdav", icon: "📁", title: "WebDAV", desc: "坚果云 / Nextcloud / ownCloud / 群晖 DSM" },
  { kind: "git", icon: "🔀", title: "Git 仓库", desc: "GitHub / Gitee / Gitea 私有仓库，自带版本历史" },
  { kind: "baidu", icon: "🅑", title: "百度网盘", desc: "开放平台设备码授权，支持不同文件夹绑定不同账户" },
  { kind: "aliyun", icon: "🅐", title: "阿里云盘", desc: "官方 App 扫码免密登录，支持不同文件夹绑定不同账户" },
];

function QrCanvas({ url }: { url: string }) {
  const ref = useRef<HTMLCanvasElement>(null);
  useEffect(() => {
    if (ref.current && url) {
      QRCode.toCanvas(ref.current, url, { width: 180, margin: 1 }).catch(() => {});
    }
  }, [url]);
  if (!url) return null;
  return <canvas ref={ref} className="qr-canvas" />;
}

export default function AddFolder({ onDone }: { onDone: () => void }) {
  const [step, setStep] = useState(1);
  const [name, setName] = useState("");
  const [localPath, setLocalPath] = useState("");
  const [backendKind, setBackendKind] = useState<BackendKind>("s3");
  const [backend, setBackend] = useState<Partial<BackendConfig>>({ type: "s3" });
  const [direction, setDirection] = useState<"bidirectional" | "upload_only" | "download_only">("bidirectional");
  const [conflictPolicy, setConflictPolicy] = useState<"both_copies" | "newest_wins">("both_copies");
  const [syncMode, setSyncMode] = useState<"realtime" | "interval" | "manual">("realtime");
  const [intervalSecs, setIntervalSecs] = useState(300);
  const [ignoreText, setIgnoreText] = useState("");
  const [testState, setTestState] = useState<"idle" | "testing" | "ok" | "fail">("idle");
  const [testMsg, setTestMsg] = useState("");
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState("");

  // 云盘多账号状态
  const [cloudAccounts, setCloudAccounts] = useState<CloudAccount[]>([]);
  const [selectedAccountId, setSelectedAccountId] = useState<string>("__new__");
  const [newAccountName, setNewAccountName] = useState("");

  // 百度授权状态
  const [baiduAppKey, setBaiduAppKey] = useState("");
  const [baiduSecretKey, setBaiduSecretKey] = useState("");
  const [baiduInfo, setBaiduInfo] = useState<BaiduLoginInfo | null>(null);
  const [baiduStatus, setBaiduStatus] = useState("");
  const [baiduAuthorizing, setBaiduAuthorizing] = useState(false);

  // 阿里授权状态
  const [aliyunInfo, setAliyunInfo] = useState<AliyunQrInfo | null>(null);
  const [aliyunStatus, setAliyunStatus] = useState("");
  const [aliyunAuthorizing, setAliyunAuthorizing] = useState(false);
  const aliyunPollGen = useRef(0);

  // 切换后端时加载对应账号列表
  useEffect(() => {
    if (backendKind === "baidu" || backendKind === "aliyun") {
      loadAccounts(backendKind);
    }
  }, [backendKind]);

  function loadAccounts(kind: "baidu" | "aliyun") {
    api.listCloudAccounts(kind).then((list) => {
      setCloudAccounts(list);
      if (list.length > 0) {
        // 默认选中第一个已有账号
        applyAccount(list[0]);
      } else {
        setSelectedAccountId("__new__");
        setNewAccountName(kind === "baidu" ? "百度账号 1" : "阿里云盘账号 1");
      }
    }).catch(() => {});
  }

  function applyAccount(acc: CloudAccount) {
    setSelectedAccountId(acc.id);
    setBackend((b) => ({
      ...b,
      authId: acc.id,
      accountName: acc.name,
      appKey: acc.appKey ?? b.appKey,
      secretKey: acc.secretKey ?? b.secretKey,
      remoteDir: b.remoteDir || "/apps/synccloud",
    }));
  }

  function handleAccountSelect(val: string) {
    setSelectedAccountId(val);
    if (val === "__new__") {
      setNewAccountName(backendKind === "baidu" ? `百度账号 ${cloudAccounts.length + 1}` : `阿里云盘账号 ${cloudAccounts.length + 1}`);
      setBackend((b) => ({ ...b, authId: undefined, accountName: undefined }));
    } else {
      const found = cloudAccounts.find((a) => a.id === val);
      if (found) {
        applyAccount(found);
      }
    }
  }

  function switchBackend(kind: BackendKind) {
    setBackendKind(kind);
    setTestState("idle");
    setTestMsg("");
    setBaiduInfo(null);
    setBaiduStatus("");
    setAliyunInfo(null);
    setAliyunStatus("");
    if (kind === "baidu" || kind === "aliyun") {
      setBackend({ type: kind, remoteDir: "/apps/synccloud" });
    } else {
      setBackend({ type: kind });
    }
  }

  async function pickDir() {
    const dir = await open({ directory: true, multiple: false, title: "选择要同步的本地文件夹" });
    if (typeof dir === "string") {
      setLocalPath(dir);
      if (!name) setName(dir.split(/[\\/]/).pop() || "同步文件夹");
    }
  }

  // ---- 百度网盘就地授权 ----
  async function startBaiduAuth() {
    if (!baiduAppKey.trim() || !baiduSecretKey.trim()) return;
    setBaiduAuthorizing(true);
    setBaiduStatus("正在发起设备码授权…");
    try {
      const info = await api.baiduLoginStart(baiduAppKey.trim(), baiduSecretKey.trim());
      setBaiduInfo(info);
      setBaiduStatus(
        info.qrcodeUrl
          ? "请使用百度网盘 App 扫描下方二维码，或在浏览器打开验证页输入用户码"
          : `请在浏览器打开 ${info.verificationUrl} 并输入用户码 ${info.userCode}`
      );
      pollBaidu(info, baiduAppKey.trim(), baiduSecretKey.trim());
    } catch (e) {
      setBaiduAuthorizing(false);
      setBaiduStatus(`发起失败: ${e}`);
    }
  }

  async function pollBaidu(info: BaiduLoginInfo, ak: string, sk: string, attempt = 0, errs = 0) {
    if (attempt > 90) {
      setBaiduAuthorizing(false);
      return setBaiduStatus("等待授权超时，请重新发起");
    }
    try {
      const authId = crypto.randomUUID();
      const accName = newAccountName.trim() || `百度账号 ${cloudAccounts.length + 1}`;
      const account = await api.baiduLoginPoll(ak, sk, info.deviceCode, authId, accName);
      if (account) {
        setBaiduAuthorizing(false);
        setBaiduStatus(`✓ 百度网盘授权成功: ${account.name}`);
        setBaiduInfo(null);
        // 更新账号列表并选中新账号
        const nextList = [...cloudAccounts.filter((a) => a.id !== account.id), account];
        setCloudAccounts(nextList);
        applyAccount(account);
      } else {
        setTimeout(() => pollBaidu(info, ak, sk, attempt + 1), (info.interval || 5) * 1000);
      }
    } catch (e) {
      if (errs >= 3) {
        setBaiduAuthorizing(false);
        setBaiduStatus(String(e));
      } else {
        setTimeout(() => pollBaidu(info, ak, sk, attempt + 1, errs + 1), 5000);
      }
    }
  }

  // ---- 阿里云盘就地扫码授权 ----
  async function startAliyunAuth() {
    const gen = ++aliyunPollGen.current;
    setAliyunAuthorizing(true);
    setAliyunStatus("正在生成登录二维码…");
    try {
      const info = await api.aliyunQrStart();
      if (gen !== aliyunPollGen.current) return;
      setAliyunInfo(info);
      setAliyunStatus("请使用阿里云盘 App 扫描下方二维码并确认登录");
      pollAliyun(info.sid, gen);
    } catch (e) {
      setAliyunAuthorizing(false);
      setAliyunStatus(`生成二维码失败: ${e}`);
    }
  }

  async function pollAliyun(sid: string, gen: number, attempt = 0, errs = 0) {
    if (gen !== aliyunPollGen.current) return;
    if (attempt > 100) {
      setAliyunAuthorizing(false);
      return setAliyunStatus("等待扫码超时，请重新生成二维码");
    }
    try {
      const authId = crypto.randomUUID();
      const accName = newAccountName.trim() || `阿里云盘账号 ${cloudAccounts.length + 1}`;
      const account = await api.aliyunQrPoll(sid, authId, accName);
      if (gen !== aliyunPollGen.current) return;
      if (account) {
        setAliyunAuthorizing(false);
        setAliyunStatus(`✓ 阿里云盘授权成功: ${account.name}`);
        setAliyunInfo(null);
        const nextList = [...cloudAccounts.filter((a) => a.id !== account.id), account];
        setCloudAccounts(nextList);
        applyAccount(account);
      } else {
        setTimeout(() => pollAliyun(sid, gen, attempt + 1), 3000);
      }
    } catch (e) {
      if (gen !== aliyunPollGen.current) return;
      const msg = String(e);
      if (msg.includes("过期")) {
        setAliyunAuthorizing(false);
        setAliyunStatus(`${msg}（请重新生成二维码）`);
      } else if (errs >= 3) {
        setAliyunAuthorizing(false);
        setAliyunStatus(msg);
      } else {
        setTimeout(() => pollAliyun(sid, gen, attempt + 1, errs + 1), 4000);
      }
    }
  }

  async function testConn() {
    setTestState("testing");
    setTestMsg("");
    try {
      const msg = await api.testBackend(buildBackend());
      setTestState("ok");
      setTestMsg(msg);
    } catch (e) {
      setTestState("fail");
      setTestMsg(String(e));
    }
  }

  function buildBackend(): BackendConfig {
    const b: BackendConfig = { type: backendKind, ...backend } as BackendConfig;
    b.type = backendKind;
    return b;
  }

  async function create() {
    setCreating(true);
    setError("");
    try {
      await api.addFolder({
        id: crypto.randomUUID(),
        name: name.trim() || "未命名",
        localPath,
        backend: buildBackend(),
        direction,
        conflictPolicy,
        ignoreRules: ignoreText
          .split(/\r?\n/)
          .map((s) => s.trim())
          .filter(Boolean),
        syncMode,
        intervalSecs: Math.max(30, intervalSecs),
        encryption: { mode: "off" },
        enabled: true,
      });
      onDone();
    } catch (e) {
      setError(String(e));
    } finally {
      setCreating(false);
    }
  }

  const canNext1 = name.trim() && localPath.trim();
  const backendRequiredOk =
    backendKind === "s3"
      ? backend.endpoint && backend.bucket && backend.accessKeyId && backend.secretAccessKey
      : backendKind === "webdav"
        ? backend.url && backend.username && backend.password
        : backendKind === "git"
          ? backend.remoteUrl
          : backendKind === "baidu"
            ? (selectedAccountId !== "__new__" || (backend.authId && backend.appKey))
            : backendKind === "aliyun"
              ? (selectedAccountId !== "__new__" || backend.authId)
              : true;

  return (
    <div className="page">
      <div className="page-header">
        <h1>添加同步文件夹</h1>
        <div className="steps">
          <span className={step >= 1 ? "step active" : "step"}>1 基本信息</span>
          <span className="step-arrow">→</span>
          <span className={step >= 2 ? "step active" : "step"}>2 存储后端</span>
          <span className="step-arrow">→</span>
          <span className={step >= 3 ? "step active" : "step"}>3 同步选项</span>
          <span className="step-arrow">→</span>
          <span className={step >= 4 ? "step active" : "step"}>4 确认</span>
        </div>
      </div>

      {step === 1 && (
        <div className="wizard-body">
          <label className="field">
            <span>名称</span>
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="例如：工作文档" />
          </label>
          <label className="field">
            <span>本地文件夹</span>
            <div className="row">
              <input value={localPath} onChange={(e) => setLocalPath(e.target.value)} placeholder="D:\Docs" />
              <button className="btn" onClick={pickDir}>
                浏览…
              </button>
            </div>
            <span className="hint">该文件夹的内容将与远端存储保持同步，不会在其中生成任何非用户文件</span>
          </label>
          <div className="wizard-actions">
            <button className="btn btn-primary" disabled={!canNext1} onClick={() => setStep(2)}>
              下一步
            </button>
          </div>
        </div>
      )}

      {step === 2 && (
        <div className="wizard-body">
          <div className="backend-grid">
            {BACKENDS.map((b) => (
              <button
                key={b.kind}
                className={`backend-card ${backendKind === b.kind ? "selected" : ""}`}
                onClick={() => switchBackend(b.kind)}
              >
                <div className="backend-icon">{b.icon}</div>
                <div className="backend-title">{b.title}</div>
                <div className="backend-desc">{b.desc}</div>
              </button>
            ))}
          </div>

          <div className="backend-form">
            {backendKind === "s3" && (
              <>
                <div className="grid-2">
                  <label className="field">
                    <span>Endpoint</span>
                    <input
                      value={backend.endpoint ?? ""}
                      onChange={(e) => setBackend({ ...backend, endpoint: e.target.value })}
                      placeholder="https://s3.amazonaws.com 或 http://127.0.0.1:9000"
                    />
                  </label>
                  <label className="field">
                    <span>Region</span>
                    <input
                      value={backend.region ?? ""}
                      onChange={(e) => setBackend({ ...backend, region: e.target.value })}
                      placeholder="us-east-1"
                    />
                  </label>
                </div>
                <div className="grid-2">
                  <label className="field">
                    <span>Bucket</span>
                    <input
                      value={backend.bucket ?? ""}
                      onChange={(e) => setBackend({ ...backend, bucket: e.target.value })}
                    />
                  </label>
                  <label className="field">
                    <span>仓库前缀（可选）</span>
                    <input
                      value={backend.rootPrefix ?? ""}
                      onChange={(e) => setBackend({ ...backend, rootPrefix: e.target.value })}
                      placeholder="synccloud/docs"
                    />
                  </label>
                </div>
                <div className="grid-2">
                  <label className="field">
                    <span>Access Key ID</span>
                    <input
                      value={backend.accessKeyId ?? ""}
                      onChange={(e) => setBackend({ ...backend, accessKeyId: e.target.value })}
                    />
                  </label>
                  <label className="field">
                    <span>Secret Access Key</span>
                    <input
                      type="password"
                      value={backend.secretAccessKey ?? ""}
                      onChange={(e) => setBackend({ ...backend, secretAccessKey: e.target.value })}
                    />
                  </label>
                </div>
                <label className="field checkbox">
                  <input
                    type="checkbox"
                    checked={backend.pathStyle ?? true}
                    onChange={(e) => setBackend({ ...backend, pathStyle: e.target.checked })}
                  />
                  <span>Path Style（MinIO / R2 / 多数私有部署需勾选）</span>
                </label>
              </>
            )}

            {backendKind === "webdav" && (
              <>
                <label className="field">
                  <span>服务器地址</span>
                  <input
                    value={backend.url ?? ""}
                    onChange={(e) => setBackend({ ...backend, url: e.target.value })}
                    placeholder="https://dav.jianguoyun.com/dav/"
                  />
                </label>
                <div className="grid-2">
                  <label className="field">
                    <span>用户名</span>
                    <input
                      value={backend.username ?? ""}
                      onChange={(e) => setBackend({ ...backend, username: e.target.value })}
                    />
                  </label>
                  <label className="field">
                    <span>密码 / 应用密码</span>
                    <input
                      type="password"
                      value={backend.password ?? ""}
                      onChange={(e) => setBackend({ ...backend, password: e.target.value })}
                      placeholder="坚果云请使用应用密码"
                    />
                  </label>
                </div>
                <label className="field">
                  <span>远端目录（可选）</span>
                  <input
                    value={backend.rootPath ?? ""}
                    onChange={(e) => setBackend({ ...backend, rootPath: e.target.value })}
                    placeholder="留空则使用 /synccloud"
                  />
                </label>
              </>
            )}

            {backendKind === "git" && (
              <>
                <label className="field">
                  <span>远端仓库地址</span>
                  <input
                    value={backend.remoteUrl ?? ""}
                    onChange={(e) => setBackend({ ...backend, remoteUrl: e.target.value })}
                    placeholder="https://github.com/you/private-sync-repo.git"
                  />
                </label>
                <div className="grid-2">
                  <label className="field">
                    <span>分支</span>
                    <input
                      value={backend.branch ?? "synccloud"}
                      onChange={(e) => setBackend({ ...backend, branch: e.target.value })}
                    />
                  </label>
                  <label className="field">
                    <span>提交大小限制提示</span>
                    <input value="GitHub 单文件上限 100MB" disabled />
                  </label>
                </div>
                <div className="grid-2">
                  <label className="field">
                    <span>用户名（https 认证）</span>
                    <input
                      value={backend.username ?? ""}
                      onChange={(e) => setBackend({ ...backend, username: e.target.value })}
                      placeholder="GitHub 用户名"
                    />
                  </label>
                  <label className="field">
                    <span>访问令牌（PAT）</span>
                    <input
                      type="password"
                      value={backend.password ?? ""}
                      onChange={(e) => setBackend({ ...backend, password: e.target.value })}
                      placeholder="ghp_… / SSH 仓库可留空"
                    />
                  </label>
                </div>
              </>
            )}

            {backendKind === "baidu" && (
              <>
                <div className="callout">
                  支持多百度网盘账号独立同步。你可以在此步骤直接绑定已有百度账户，或授权新账户。
                </div>

                <label className="field">
                  <span>选择百度网盘账户</span>
                  <select value={selectedAccountId} onChange={(e) => handleAccountSelect(e.target.value)}>
                    {cloudAccounts.map((a) => (
                      <option key={a.id} value={a.id}>
                        {a.name}（已授权）
                      </option>
                    ))}
                    <option value="__new__">+ 授权新百度网盘账户…</option>
                  </select>
                </label>

                {selectedAccountId === "__new__" && (
                  <div className="box-panel" style={{ border: "1px dashed var(--border-color)", padding: "12px", borderRadius: "8px", marginBottom: "16px" }}>
                    <div className="hint" style={{ marginBottom: "8px" }}>
                      授权新百度账户：输入开放平台应用 AppKey/SecretKey 发起设备码授权
                    </div>
                    <label className="field">
                      <span>账户备注名称</span>
                      <input
                        value={newAccountName}
                        onChange={(e) => setNewAccountName(e.target.value)}
                        placeholder="例如：个人网盘 / 工作网盘"
                      />
                    </label>
                    <div className="grid-2">
                      <label className="field">
                        <span>AppKey</span>
                        <input
                          value={baiduAppKey}
                          onChange={(e) => setBaiduAppKey(e.target.value.trim())}
                          placeholder="开放平台 AppKey"
                        />
                      </label>
                      <label className="field">
                        <span>SecretKey</span>
                        <input
                          type="password"
                          value={baiduSecretKey}
                          onChange={(e) => setBaiduSecretKey(e.target.value.trim())}
                          placeholder="开放平台 SecretKey"
                        />
                      </label>
                    </div>
                    <div className="row" style={{ marginTop: "8px", alignItems: "center" }}>
                      <button
                        className="btn btn-primary"
                        disabled={!baiduAppKey || !baiduSecretKey || baiduAuthorizing}
                        onClick={startBaiduAuth}
                      >
                        {baiduAuthorizing ? "授权中…" : "发起设备码授权"}
                      </button>
                      {baiduStatus && <span className="hint" style={{ marginLeft: "8px" }}>{baiduStatus}</span>}
                    </div>

                    {baiduInfo && (
                      <div style={{ marginTop: "12px" }}>
                        <div className="kv">
                          <div>
                            <span className="muted">用户码：</span>
                            <code style={{ fontSize: "16px", fontWeight: "bold" }}>{baiduInfo.userCode}</code>
                          </div>
                          <div>
                            <button className="btn-link" onClick={() => api.openExternal(baiduInfo.verificationUrl)}>
                              打开验证页输入用户码 →
                            </button>
                          </div>
                        </div>
                        {baiduInfo.qrcodeUrl && (
                          <div style={{ marginTop: "8px" }}>
                            <QrCanvas url={baiduInfo.qrcodeUrl} />
                            <div className="hint">或使用百度网盘 App 扫码确认</div>
                          </div>
                        )}
                      </div>
                    )}
                  </div>
                )}

                {selectedAccountId !== "__new__" && (
                  <div className="hint" style={{ color: "var(--color-success)", marginBottom: "12px" }}>
                    ✓ 已绑定所选百度账户，该同步文件夹将独立使用此账户同步
                  </div>
                )}

                <label className="field">
                  <span>远端同步目录</span>
                  <input
                    value={backend.remoteDir ?? "/apps/synccloud"}
                    onChange={(e) => setBackend({ ...backend, remoteDir: e.target.value })}
                  />
                  <span className="hint">云端存放文件的目标目录（不会在此目录中生成任何 .synccloud 文件夹）</span>
                </label>
              </>
            )}

            {backendKind === "aliyun" && (
              <>
                <div className="callout">
                  支持多阿里云盘账号独立同步。你可以在此步骤直接绑定已有阿里云盘账户，或扫码授权新账户。
                </div>

                <label className="field">
                  <span>选择阿里云盘账户</span>
                  <select value={selectedAccountId} onChange={(e) => handleAccountSelect(e.target.value)}>
                    {cloudAccounts.map((a) => (
                      <option key={a.id} value={a.id}>
                        {a.name}（已授权）
                      </option>
                    ))}
                    <option value="__new__">+ 扫码授权新阿里云盘账户…</option>
                  </select>
                </label>

                {selectedAccountId === "__new__" && (
                  <div className="box-panel" style={{ border: "1px dashed var(--border-color)", padding: "12px", borderRadius: "8px", marginBottom: "16px" }}>
                    <label className="field">
                      <span>账户备注名称</span>
                      <input
                        value={newAccountName}
                        onChange={(e) => setNewAccountName(e.target.value)}
                        placeholder="例如：个人阿里云盘 / 备份账户"
                      />
                    </label>
                    <div className="row" style={{ marginTop: "8px", alignItems: "center" }}>
                      <button
                        className="btn btn-primary"
                        disabled={aliyunAuthorizing}
                        onClick={startAliyunAuth}
                      >
                        {aliyunAuthorizing ? "等待扫码中…" : "生成登录二维码"}
                      </button>
                      {aliyunStatus && <span className="hint" style={{ marginLeft: "8px" }}>{aliyunStatus}</span>}
                    </div>

                    {aliyunInfo && (
                      <div style={{ marginTop: "12px" }}>
                        <QrCanvas url={aliyunInfo.qrUrl} />
                        <div className="hint">请使用阿里云盘 App 扫描上方二维码并确认登录</div>
                      </div>
                    )}
                  </div>
                )}

                {selectedAccountId !== "__new__" && (
                  <div className="hint" style={{ color: "var(--color-success)", marginBottom: "12px" }}>
                    ✓ 已绑定所选阿里云盘账户，该同步文件夹将独立使用此账户同步
                  </div>
                )}

                <label className="field">
                  <span>远端同步目录</span>
                  <input
                    value={backend.remoteDir ?? "/apps/synccloud"}
                    onChange={(e) => setBackend({ ...backend, remoteDir: e.target.value })}
                  />
                  <span className="hint">云端存放文件的目标目录（不会在此目录中生成任何 .synccloud 文件夹）</span>
                </label>
              </>
            )}

            <div className="row test-row">
              <button className="btn" disabled={!backendRequiredOk || testState === "testing"} onClick={testConn}>
                {testState === "testing" ? "测试中…" : "测试连接"}
              </button>
              {testState === "ok" && <span className="test-ok">✓ {testMsg}</span>}
              {testState === "fail" && <span className="test-fail">✗ {testMsg}</span>}
            </div>
          </div>

          <div className="wizard-actions">
            <button className="btn" onClick={() => setStep(1)}>
              上一步
            </button>
            <button className="btn btn-primary" disabled={!backendRequiredOk} onClick={() => setStep(3)}>
              下一步
            </button>
          </div>
        </div>
      )}

      {step === 3 && (
        <div className="wizard-body">
          <label className="field">
            <span>同步方向</span>
            <select value={direction} onChange={(e) => setDirection(e.target.value as typeof direction)}>
              <option value="bidirectional">双向同步（推荐，本机与云端互相同步）</option>
              <option value="upload_only">仅上传（本机变更推送到云端，不拉取）</option>
              <option value="download_only">仅下载（云端变更拉到本机，不上传）</option>
            </select>
          </label>
          <label className="field">
            <span>冲突处理策略</span>
            <select value={conflictPolicy} onChange={(e) => setConflictPolicy(e.target.value as typeof conflictPolicy)}>
              <option value="both_copies">保留双方（Dropbox 式冲突副本，最安全）</option>
              <option value="newest_wins">较新者优先（败者仍存为冲突副本）</option>
            </select>
            <span className="hint">
              多台设备同时修改同一文件时：远端版本保留原名，另一版本自动存为
              「文件名 (冲突副本 - 设备名 日期).扩展名」
            </span>
          </label>
          <label className="field">
            <span>同步触发</span>
            <select value={syncMode} onChange={(e) => setSyncMode(e.target.value as typeof syncMode)}>
              <option value="realtime">实时（文件变更即同步 + 周期兜底扫描）</option>
              <option value="interval">定时（按间隔轮询）</option>
              <option value="manual">仅手动</option>
            </select>
          </label>
          {syncMode === "interval" && (
            <label className="field">
              <span>间隔（秒）</span>
              <input
                type="number"
                min={30}
                value={intervalSecs}
                onChange={(e) => setIntervalSecs(Number(e.target.value) || 300)}
              />
            </label>
          )}
          <label className="field">
            <span>排除规则（每行一条，glob 语法）</span>
            <textarea
              rows={4}
              value={ignoreText}
              onChange={(e) => setIgnoreText(e.target.value)}
              placeholder={"*.iso\nnode_modules\n.virtualenvs"}
            />
            <span className="hint">已内置排除：临时文件（~$*、*.tmp）、系统垃圾文件（Thumbs.db 等）</span>
          </label>
          <div className="wizard-actions">
            <button className="btn" onClick={() => setStep(2)}>
              上一步
            </button>
            <button className="btn btn-primary" onClick={() => setStep(4)}>
              下一步
            </button>
          </div>
        </div>
      )}

      {step === 4 && (
        <div className="wizard-body">
          <div className="confirm-list">
            <div>
              <b>名称：</b>
              {name}
            </div>
            <div>
              <b>本地文件夹：</b>
              {localPath}
            </div>
            <div>
              <b>后端：</b>
              {BACKENDS.find((b) => b.kind === backendKind)?.title}
              {backend.accountName ? `（账户: ${backend.accountName}）` : ""}
            </div>
            <div>
              <b>方向：</b>
              {direction === "bidirectional" ? "双向" : direction === "upload_only" ? "仅上传" : "仅下载"}
            </div>
            <div>
              <b>冲突策略：</b>
              {conflictPolicy === "both_copies" ? "保留双方" : "较新者优先"}
            </div>
            <div>
              <b>触发：</b>
              {syncMode === "realtime" ? "实时" : syncMode === "interval" ? `每 ${intervalSecs}s` : "手动"}
            </div>
          </div>
          {error && <div className="form-error">{error}</div>}
          <div className="wizard-actions">
            <button className="btn" onClick={() => setStep(3)}>
              上一步
            </button>
            <button className="btn btn-primary" disabled={creating} onClick={create}>
              {creating ? "创建中…" : "✓ 创建并开始同步"}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
