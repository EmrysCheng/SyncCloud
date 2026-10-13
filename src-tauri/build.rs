fn main() {
    // GNU 工具链：mingw 的默认 manifest（default-manifest.o）已停用，
    // 需要显式嵌入 common-controls v6 manifest，否则 comctl32 加载 v5，
    // 缺 TaskDialogIndirect 导致对话框路径启动即崩。
    //
    // 注意：tauri-build 默认会经 embed-resource 给 bin 目标再嵌一份 app manifest
    // （rustc-link-arg-bins），与本处「全目标」manifest 叠加会让 bin 拿到两个
    // manifest，链接报 `.rsrc merge failure: multiple non-default manifests`。
    // 因此这里让 tauri-build 跳过自带的 app manifest，由本 manifest 统一覆盖
    // bin 与测试二进制（内容为 Common-Controls v6 + PerMonitorV2 DPI 感知，
    // 是 tauri 默认 manifest 的超集）。
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("manifest");
        let manifest_rc = manifest_dir.join("app.rc");
        let manifest_o = manifest_dir.join("manifest.o");
        if manifest_rc.exists() {
            let _ = std::process::Command::new("windres")
                .arg("-i")
                .arg(&manifest_rc)
                .arg("-o")
                .arg(&manifest_o)
                .current_dir(&manifest_dir)
                .status();
        }
        if manifest_o.exists() {
            println!("cargo:rustc-link-arg={}", manifest_o.display());
        }
        println!("cargo:rerun-if-changed=manifest/app.manifest");
        println!("cargo:rerun-if-changed=manifest/app.rc");
        tauri_build::try_build(
            tauri_build::Attributes::default()
                .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest()),
        )
        .expect("tauri-build 失败");
        return;
    }
    tauri_build::build()
}
