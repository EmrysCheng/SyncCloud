#!/usr/bin/env bash
# SyncCloud 开发/构建脚本（GNU 工具链环境一键设置）
# 用法：bash scripts/dev.sh          →  启动 tauri dev（热重载开发模式）
#       bash scripts/dev.sh test     →  运行全部测试
#       bash scripts/dev.sh build    →  产出 release exe（含 WebView2Loader.dll）
#       bash scripts/dev.sh run      →  直接运行 release exe
set -e
cd "$(dirname "$0")/.."

# winlibs MinGW（GNU 工具链需要的新版 binutils/gcc）+ cargo
export PATH="$HOME/.cargo/mingw64/bin:$HOME/.cargo/bin:$PATH"

# 运行主程序需要 WebView2Loader.dll 在输出目录
WVL=$(find "$HOME/.cargo/registry/src" -path "*webview2-com-sys*/x64/WebView2Loader.dll" 2>/dev/null | head -1)
copy_loader() {
  if [ -n "$WVL" ]; then
    cp -u "$WVL" "$1"/ 2>/dev/null || true
  fi
}

case "${1:-dev}" in
  test)
    cd src-tauri && exec cargo test
    ;;
  build)
    npm run tauri build -- --no-bundle
    copy_loader src-tauri/target/release
    copy_loader src-tauri/target/release/deps
    echo "产物：src-tauri/target/release/synccloud.exe"
    ;;
  run)
    copy_loader src-tauri/target/release
    exec src-tauri/target/release/synccloud.exe
    ;;
  *)
    if [ -n "$WVL" ]; then
      mkdir -p src-tauri/target/debug src-tauri/target/debug/deps
      cp -u "$WVL" src-tauri/target/debug/ 2>/dev/null || true
      cp -u "$WVL" src-tauri/target/debug/deps/ 2>/dev/null || true
    fi
    exec npm run tauri dev
    ;;
esac
