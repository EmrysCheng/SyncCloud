use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let release_dir = manifest_dir.join("../src-tauri/target/release");

    let exe_path = release_dir.join("synccloud.exe");
    let dll_path = release_dir.join("WebView2Loader.dll");

    println!("cargo:rerun-if-changed={}", exe_path.display());
    println!("cargo:rerun-if-changed={}", dll_path.display());

    let mut exe_bytes = Vec::new();
    File::open(&exe_path)
        .expect("Failed to open synccloud.exe from target/release")
        .read_to_end(&mut exe_bytes)
        .expect("Failed to read synccloud.exe");

    let mut dll_bytes = Vec::new();
    File::open(&dll_path)
        .expect("Failed to open WebView2Loader.dll from target/release")
        .read_to_end(&mut dll_bytes)
        .expect("Failed to read WebView2Loader.dll");

    // 计算哈希
    let mut hasher = Sha256::new();
    hasher.update(&exe_bytes);
    hasher.update(&dll_bytes);
    let hash_str = hex::encode(hasher.finalize());

    fs::write(
        out_dir.join("version_hash.rs"),
        format!("pub const VERSION_HASH: &str = \"{}\";\n", hash_str),
    ).unwrap();

    // 压缩 exe
    let exe_out = out_dir.join("synccloud.deflate");
    let mut encoder = DeflateEncoder::new(File::create(&exe_out).unwrap(), Compression::best());
    encoder.write_all(&exe_bytes).unwrap();
    encoder.finish().unwrap();

    // 压缩 dll
    let dll_out = out_dir.join("webview2.deflate");
    let mut encoder = DeflateEncoder::new(File::create(&dll_out).unwrap(), Compression::best());
    encoder.write_all(&dll_bytes).unwrap();
    encoder.finish().unwrap();

    // 编译图标
    let icon_path = manifest_dir.join("../src-tauri/icons/icon.ico");
    if icon_path.exists() {
        let rc_content = format!("1 ICON \"{}\"", icon_path.display().to_string().replace('\\', "/"));
        let rc_file = out_dir.join("icon.rc");
        fs::write(&rc_file, rc_content).unwrap();
        let obj_file = out_dir.join("icon.o");
        let status = std::process::Command::new("windres")
            .arg("-i")
            .arg(&rc_file)
            .arg("-o")
            .arg(&obj_file)
            .status();
        if let Ok(st) = status {
            if st.success() {
                println!("cargo:rustc-link-arg={}", obj_file.display());
            }
        }
    }
}
