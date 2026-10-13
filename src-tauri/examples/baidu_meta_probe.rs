//! 诊断：查看百度网盘元数据迁移状态（默认只读；--clean 额外清理旧位置）。

use synccloud_lib::storage::baidu::BaiduStore;
use synccloud_lib::storage::RemoteStore;

#[tokio::main]
async fn main() {
    let cfg_raw = std::fs::read_to_string(
        std::path::Path::new(&std::env::var("APPDATA").unwrap()).join("SyncCloud/config.json"),
    )
    .expect("读取 config.json 失败");
    let cfg: serde_json::Value = serde_json::from_str(&cfg_raw).unwrap();
    let folder = &cfg["folders"][0];
    let backend = &folder["backend"];
    let app_key = backend["appKey"].as_str().unwrap().to_string();
    let secret = backend["secretKey"].as_str().unwrap().to_string();
    let remote_dir = backend["remoteDir"].as_str().unwrap_or("/apps/synccloud").to_string();
    println!("remote_dir = {remote_dir}");

    let store = BaiduStore::new(app_key, secret, remote_dir);

    print!("[新位置]（同步目录外元数据区）");
    match store.read_manifest().await {
        Ok(Some(m)) => println!("manifest 存在: seq={} files={} device={}", m.seq, m.files.len(), m.device),
        Ok(None) => println!("manifest 不存在"),
        Err(e) => println!("读取失败: {e:#}"),
    }
    print!("[旧位置]（同步目录内 .synccloud/）");
    match store.read_legacy_manifest().await {
        Ok(Some(m)) => println!("manifest 仍存在: files={}", m.files.len()),
        Ok(None) => println!("已无 manifest"),
        Err(e) => println!("读取失败: {e:#}"),
    }

    if std::env::args().any(|a| a == "--clean") {
        match store.cleanup_legacy_meta().await {
            Ok(()) => println!("旧位置清理已执行"),
            Err(e) => println!("清理失败: {e:#}"),
        }
    }
}
