//! 只读观察：列出百度远端目录的当前文件数（不写任何数据）。
use synccloud_lib::storage::baidu::BaiduStore;
use synccloud_lib::storage::RemoteStore;

#[tokio::main]
async fn main() {
    let cfg_raw = std::fs::read_to_string(
        std::path::Path::new(&std::env::var("APPDATA").unwrap()).join("SyncCloud/config.json"),
    )
    .expect("读取 config.json 失败");
    let cfg: serde_json::Value = serde_json::from_str(&cfg_raw).unwrap();
    let backend = &cfg["folders"][0]["backend"];
    let store = BaiduStore::new(
        backend["authId"].as_str().unwrap_or("").into(),
        backend["appKey"].as_str().unwrap().into(),
        backend["secretKey"].as_str().unwrap().into(),
        backend["remoteDir"].as_str().unwrap_or("/apps/synccloud").into(),
    );
    match store.read_manifest().await {
        Ok(Some(m)) => println!("manifest: {} files, seq={}", m.files.len(), m.seq),
        Ok(None) => println!("manifest: 尚未创建"),
        Err(e) => println!("manifest ERR: {e:#}"),
    }
    match store.list_objects().await {
        Ok(v) => {
            println!("远端对象数: {}", v.len());
            let total: u64 = v.iter().map(|e| e.size).sum();
            println!("总字节数: {total} ({:.1} MB)", total as f64 / 1048576.0);
            for e in v.iter().take(10) {
                println!("  {} ({}B)", e.key, e.size);
            }
        }
        Err(e) => println!("list ERR: {e:#}"),
    }
}
