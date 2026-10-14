# SyncCloud — 多后端文件同步桌面应用

基于 **Tauri 2 + Rust** 的 Windows 桌面文件同步工具：把本地任意文件夹与
**S3 兼容对象存储 / WebDAV / Git 仓库 / 百度网盘 / 阿里云盘** 建立双向同步，
支持多个独立同步文件夹、多机冲突自动处理（Dropbox 式冲突副本）。

## 运行环境与系统要求

- **操作系统**：Windows 10 64位（版本 1809 及以上）/ Windows 11（推荐）；
- **特别说明（关于 Windows 7 / 8）**：
  - 本程序基于 **Tauri 2.0** 与 **现代 Rust 异步运行时** 构建，界面渲染底层强依赖微软官方的 **Microsoft Edge WebView2**；
  - 微软官方已于 2023 年 1 月 10 日彻底终止对 Windows 7 / 8 的所有支持与 WebView2 更新维护。同时，现代 Rust 编译器（1.78+）与 Tauri 2 所引用的现代 Windows 内核 API 在 Windows 7 原生系统下缺失，在 Win7 原生环境下启动会直接触发 `应用程序无法正常启动（0xc0000005）` 内存访问违例报错；
  - 因此，**官方仅支持 Windows 10 及以上 64 位操作系统**，无法直接运行于原生 Windows 7 环境。

## 功能特性

- **多种存储后端**
  - **S3 兼容**：AWS S3、MinIO、阿里云 OSS（S3 兼容模式）、腾讯云 COS、Cloudflare R2、Backblaze B2
  - **WebDAV**：坚果云、Nextcloud、ownCloud、群晖 DSM 等
  - **Git**：GitHub / Gitee / Gitea 私有仓库，同步历史即提交历史
  - **百度网盘 / 阿里云盘**：百度需自建开放平台应用；阿里云盘 App 扫码即用
- **增量同步（坚果云式体验）**
  - 本地无变化 + 远端 manifest 变更戳未变 → 整轮同步零成本跳过
    （不取远端锁、不下载清单、零传输）
  - S3 用 HEAD / WebDAV 用 PROPFIND / 云盘用目录项做廉价变更探测
  - xxh3 内容哈希 + (mtime,size) 缓存，重启后扫描只 stat 不读文件
  - 远端变更日志（journal）：上传成功但 manifest 未提交时崩溃，
    下轮自动重放收敛 —— 既不丢文件，也不会全量重传
- **并行传输**：上传/下载按设置的并发数（默认 3）同时执行
- **主流同步策略**
  - 双向同步（默认）/ 仅上传 / 仅下载镜像
  - 三方对比引擎（本地 / 远端 manifest / 本地基线）
  - 实时文件监控（2 秒防抖）+ 周期兜底对比
  - 失败指数退避重试（应对 Windows 文件占用）
  - 下载走临时文件 + 原子替换
- **文件占用无感知同步（Syncthing / Resilio 同款思路）**
  - 共享读：文件正被 Office / 编辑器 / 杀毒占用时照常读取、哈希与上传
  - POSIX 语义替换：目标文件被其他程序打开时仍可原地替换下载内容
  - 待替换队列：极端独占锁定时新版本先安全停放，占用释放后下轮自动应用
    （最终一致，不报错、不丢数据；7 天未应用自动清理）
  - 健壮删除：被占用文件挂起 delete-on-close，句柄关闭即删除
- **多机冲突处理（Dropbox 方式）**
  - 同一文件被多台设备同时修改时：远端版本保留原名，
    本机修改自动另存为 `报告 (冲突副本 - 设备名 2026-10-12 1430).docx`
  - 两个版本都保留并互相同步，绝不丢数据
  - 策略可配：保留双方（默认）/ 较新者优先
  - 冲突中心页面可查看与标记处理结果
- **多同步文件夹**：每个文件夹独立配置后端、方向、冲突策略、排除规则、触发模式
- **系统托盘**：关闭窗口最小化到托盘后台常驻同步；左键单击快速唤醒；右键菜单支持：显示主界面 / 立即同步全部 / 打开数据目录 / 开机自启动切换 / 退出
- **资源管理器深度集成**：卡片路径可点击直达本地目录；每一条传输日志和动态均支持**一键在 Windows 资源管理器中高亮选中文件**（`/select,`）
- **传输历史中心**：全局按时间倒序追踪所有文件的上传、下载、删除、失败，支持按文件夹及动作多维筛选
- **全局状态仪表盘**：顶栏直观展示已同步、同步中、已暂停、异常数，支持「全部立即同步」与「全部暂停/恢复」一键批处理
- **文件夹热配置与排除规则**：支持随时热修改任务名、同步方向、冲突策略、触发模式，并支持自定义 Glob 排除黑名单
- **数据目录自定义重定向**：支持随时迁移应用数据与数据库存储路径，一键平滑复制并重启
- **单文件便携化（Zero-DLL Standalone）**：自研轻量级 Rust 启动器，内置 Deflate 压缩与 WebView2 依赖自举，免安装单文件即开即用（仅 ~6.6 MB）
- **开机自启动**：Windows 注册表自启集成，支持界面与托盘随时联动开关
- **远端多机锁**：`.synccloud/locks` + TTL，防止两台设备同时写 manifest 交叉覆盖
- **客户端加密（零知识，开发中）**：Argon2id 派生 + XChaCha20-Poly1305 流式 AEAD
- **凭据安全**：云盘 OAuth 令牌存 Windows 凭据管理器（keyring）；解除授权时真实删除凭据条目
- **授权账户健康管理**：设置页可对每个网盘账户一键「检测连接」（令牌存在性 + 实时远端探活），异常时给出原因；显示账户绑定的同步任务数
- **文件日志系统**：全量运行日志写入数据目录 `logs/`（单文件超 5MB 自动轮转、保留 14 天），等级 trace~error 设置页热切换；内置日志查看器直接查看最近 300 行，无需翻文件

## 项目结构

```
src-tauri/src/
├── sync/
│   ├── plan.rs        # 三方对比引擎（决策矩阵，表驱动测试全覆盖）
│   ├── conflict.rs    # Dropbox 式冲突副本命名
│   ├── engine.rs      # 同步循环编排（快路径→锁→重放日志→对比→并行执行→提交）
│   ├── scanner.rs     # 递归扫描 + ignore + 哈希缓存
│   ├── watcher.rs     # notify 文件监控 + 防抖
│   ├── state.rs       # SQLite 基线/哈希缓存/冲突记录/崩溃恢复日志
│   ├── transfer.rs    # 指数退避重试
│   └── manager.rs     # 多文件夹并行任务管理
├── storage/           # RemoteStore 抽象 + S3/WebDAV/Git/百度/阿里实现 + 远端锁
├── crypto.rs          # 客户端加密
├── config.rs          # 应用与文件夹配置
├── credential.rs      # keyring 凭据
└── commands.rs        # Tauri IPC 命令
src/                   # React 前端（列表/向导/事件流/冲突中心/设置）
```

## 构建与打包

```bash
npm install          # 前端依赖
npm run dev          # 前端开发调试
npm run build:app    # 生产构建：编译前端 + 编译 Tauri 核心 + 打包便携式单文件（输出至 dist-app/synccloud.exe）
```

运行测试：

```bash
cd src-tauri
cargo test           # 单元 + 端到端（内存 Mock 存储上传流 / 占用文件待替换队列 /
                     # 双设备冲突模拟 / 崩溃恢复日志 / 增量快路径 / POSIX 替换与共享读）
```

### GNU 工具链备选方案（本仓库已适配）

若本机无法安装 VS Build Tools（如代理拦截微软下载域名），可用自包含的 GNU 工具链：

1. `rustup default stable-x86_64-pc-windows-gnu`
2. 解压 [winlibs](https://github.com/brechtsanders/winlibs_mingw/releases) 到 `~/.cargo/mingw64`（提供新版 binutils，rustup 自带的 dlltool 过旧不支持 `--64`）
3. 停用 mingw 默认 manifest（避免与 tauri manifest 合并损坏）：
   `mv ~/.cargo/mingw64/x86_64-w64-mingw32/lib/default-manifest.o{,.disabled}`
4. 编译与测试：

```bash
export PATH=~/.cargo/mingw64/bin:~/.cargo/bin:$PATH
cargo test
```

若链接报 `could not find native static library WebView2LoaderStatic`（GNU ld
找不到 MSVC 命名的静态库），把 webview2-com-sys 构建输出里的
`WebView2LoaderStatic.lib` 复制为同目录 `libWebView2LoaderStatic.a` 即可
（GNU ld 可直接链接 COFF 归档）：

```bash
for d in target/*/build/webview2-com-sys-*/out/x64; do
  [ -f "$d/WebView2LoaderStatic.lib" ] && cp -n "$d/WebView2LoaderStatic.lib" "$d/libWebView2LoaderStatic.a"
done
```

`src-tauri/build.rs` 已内置 GNU 目标的 common-controls v6 manifest 嵌入
（`manifest/manifest.o`，由 `manifest/app.rc` 经 windres 编译；测试二进制
不经过 tauri-build 的 bin 链接参数，没有它会因 comctl32 v5 缺
`TaskDialogIndirect` 而启动崩溃）。

## 使用

1. 运行 **`synccloud.exe`**（见下），启动后常驻系统托盘；关闭窗口只是最小化，
   退出请用托盘右键菜单
2. 进入「添加同步文件夹」向导：选择本地目录 → 选择后端并测试连接 →
   配置同步方向/冲突策略/排除规则 → 创建
3. 文件变更会实时同步；可随时暂停/恢复/立即同步；无变化轮次自动跳过（事件流
   会显示「增量检查：无变化，已跳过」）
4. 多台电脑使用同一远端仓库即可互相同步（每台各自安装并指向同一后端）
5. 冲突发生后到「冲突中心」确认处理结果

### 生成可直接运行的 exe

```bash
npm run tauri build -- --no-bundle
# 产物：src-tauri/target/release/synccloud.exe
```

GNU 工具链下需把 `WebView2Loader.dll` 放到 exe 同目录（从
`~/.cargo/registry/src/*/webview2-com-sys-*/x64/` 复制；MSVC 工具链静态链接无需此步）。
也可直接 `npm run tauri build` 产出 NSIS 安装包。

## 各后端配置说明

| 后端 | 必填 | 说明 |
|------|------|------|
| S3 | endpoint / region / bucket / AK / SK | MinIO、R2 等需勾选 Path Style |
| WebDAV | 地址 / 用户名 / 密码 | 坚果云请使用「应用密码」而非登录密码 |
| Git | 仓库地址 / 分支 | https 需用户名+访问令牌；SSH 使用本机密钥；GitHub 单文件上限 100MB；仓库无同名分支时自动创建 |
| 百度网盘 | AppKey / SecretKey | 在 pan.baidu.com/union/main 自建应用；设置页完成设备码授权（凭据本机持久化，向导自动带入） |
| 阿里云盘 | 无需凭据 | 设置页用阿里云盘 App 扫码登录即可。授权走 AList 同款官方公共 OAuth（open.aliyundrive.com 直连，失败自动回退社区中转，可用环境变量 `ALIYUNDRIVE_OAUTH_SERVER` 自定义） |

## 已知限制（Roadmap）

- 大文件分块上传（S3 multipart / 百度分片已在后端实现，阿里云盘 10MB 分块已实现）
- 客户端加密已实现核心算法（crypto 模块 + 测试），引擎集成开关待开放
- 国际云盘（Dropbox / Google Drive / OneDrive）原生 API 后端待扩展
  （架构上只需新增 `RemoteStore` 实现）
- 远端回收站 / 版本浏览 UI

## 许可

MIT
