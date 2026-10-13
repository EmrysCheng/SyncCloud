//! 一次性清理：删除远端 <remote_dir>/.git 目录（早期未排除时误传的孤儿文件，
//! 不在 manifest 内，引擎不可见，需手动清掉）。本地 .git 不受影响。
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
    let remote_dir = backend["remoteDir"].as_str().unwrap_or("/apps/synccloud");
    let store = BaiduStore::new(
        backend["authId"].as_str().unwrap_or("").into(),
        backend["appKey"].as_str().unwrap().into(),
        backend["secretKey"].as_str().unwrap().into(),
        remote_dir.into(),
    );
    match store.delete_file(".git").await {
        Ok(()) => println!("已删除远端 {remote_dir}/.git"),
        Err(e) => println!("删除失败: {e:#}"),
    }
}
