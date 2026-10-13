fn main() {
    // GNU 工具链：mingw 的默认 manifest（default-manifest.o）已停用，
    // tauri-build 的 app manifest 仅作用于 bin 目标（rustc-link-arg-bins），
    // 测试二进制拿不到 → comctl32 加载 v5 缺 TaskDialogIndirect → 启动即崩。
    // 这里对所有链接目标（含测试 exe）额外嵌入 common-controls v6 manifest。
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        let manifest_o = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("manifest/manifest.o");
        println!("cargo:rustc-link-arg={}", manifest_o.display());
        println!("cargo:rerun-if-changed=manifest/app.manifest");
    }
    tauri_build::build()
}
