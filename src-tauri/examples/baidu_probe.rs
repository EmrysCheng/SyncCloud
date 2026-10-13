//! 百度网盘后端真实链路探针：用 config.json 里保存的凭据依次执行
//! 连接测试 → 锁写入/读取/删除 → manifest 读写 → 对象列举，逐步打印结果，
//! 用于定位引擎同步失败的具体环节。

use synccloud_lib::storage::baidu::BaiduStore;
use synccloud_lib::storage::{RawIo, RemoteStore};

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
    println!("remote_dir = {remote_dir}, app_key = {}***", &app_key[..4.min(app_key.len())]);

    let store = BaiduStore::new(app_key, secret, remote_dir);

    println!("\n== 1. test() ==");
    match store.test().await {
        Ok(()) => println!("OK"),
        Err(e) => println!("ERR: {e:#}"),
    }

    println!("\n== 2. lock 原语 put/read/delete ==");
    let raw = store.raw_io();
    match raw.put_text(".synccloud/probe-lock.txt", "probe|9999999999999").await {
        Ok(()) => println!("put OK"),
        Err(e) => println!("put ERR: {e:#}"),
    }
    match raw.read_text(".synccloud/probe-lock.txt").await {
        Ok(Some(s)) => println!("read OK: {s}"),
        Ok(None) => println!("read OK: None"),
        Err(e) => println!("read ERR: {e:#}"),
    }
    match raw.list_prefix(".synccloud").await {
        Ok(v) => println!("list_prefix OK: {v:?}"),
        Err(e) => println!("list_prefix ERR: {e:#}"),
    }
    match raw.delete(".synccloud/probe-lock.txt").await {
        Ok(()) => println!("delete OK"),
        Err(e) => println!("delete ERR: {e:#}"),
    }

    println!("\n== 3. read_manifest ==");
    match store.read_manifest().await {
        Ok(Some(m)) => println!("OK: {} files, device={}", m.files.len(), m.device),
        Ok(None) => println!("OK: None（远端尚未创建）"),
        Err(e) => println!("ERR: {e:#}"),
    }

    println!("\n== 4. list_objects ==");
    match store.list_objects().await {
        Ok(v) => println!("OK: {} 个对象: {:?}", v.len(), v.iter().take(5).map(|e| &e.key).collect::<Vec<_>>()),
        Err(e) => println!("ERR: {e:#}"),
    }

    println!("\n== 5. 小文件上传/下载往返 ==");
    let tmp = std::env::temp_dir().join("synccloud-probe-up.txt");
    std::fs::write(&tmp, b"probe content").unwrap();
    match store.put_file("probe/up-test.txt", &tmp).await {
        Ok(()) => println!("put_file OK"),
        Err(e) => println!("put_file ERR: {e:#}"),
    }
    let down = std::env::temp_dir().join("synccloud-probe-down.txt");
    match store.get_file("probe/up-test.txt", &down).await {
        Ok(()) => println!("get_file OK: {:?}", std::fs::read(&down).unwrap()),
        Err(e) => println!("get_file ERR: {e:#}"),
    }
    match store.delete_file("probe/up-test.txt").await {
        Ok(()) => println!("delete_file OK"),
        Err(e) => println!("delete_file ERR: {e:#}"),
    }

    println!("\n== 6. 删除接口变体探测 ==");
    {
        use std::time::Duration;
        let app_key: String = backend["appKey"].as_str().unwrap().into();
        let token = match synccloud_lib::credential::load_secret(&format!("baidu:{app_key}")) {
            Ok(Some(t)) => {
                let st: serde_json::Value = serde_json::from_str(&t).unwrap();
                st["access_token"].as_str().unwrap().to_string()
            }
            _ => {
                println!("（keyring 无 token，跳过变体探测）");
                String::new()
            }
        };
        if !token.is_empty() {
            let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
            let path = "/Sync/probe/up-test.txt";
            // 取 fs_id
            let fs_id: String = {
                let r: serde_json::Value = http
                    .get("https://pan.baidu.com/rest/2.0/xpan/file")
                    .query(&[
                        ("method", "list"),
                        ("access_token", token.as_str()),
                        ("dir", "/Sync/probe"),
                        ("web", "1"),
                    ])
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                r["list"][0]["fs_id"].to_string()
            };
            println!("fs_id = {fs_id}");

            // a) 旧版 filemanager
            let a = http
                .post("https://pan.baidu.com/rest/2.0/xpan/file")
                .query(&[("method", "filemanager"), ("access_token", token.as_str()), ("opera", "delete")])
                .form(&[("filelist", format!("[\"{path}\"]"))])
                .send()
                .await
                .unwrap();
            println!("legacy filemanager: HTTP {} {}", a.status(), a.text().await.unwrap_or_default());

            // b) fs_id 形式
            let b = http
                .post("https://pan.baidu.com/rest/2.0/xpan/file_manager")
                .query(&[("method", "delete"), ("access_token", token.as_str())])
                .form(&[("filelist", format!("[{fs_id}]"))])
                .send()
                .await
                .unwrap();
            println!("fsid filelist: HTTP {} {}", b.status(), b.text().await.unwrap_or_default());

            // c) 未转义 raw body
            let c = http
                .post("https://pan.baidu.com/rest/2.0/xpan/file_manager")
                .query(&[("method", "delete"), ("access_token", token.as_str())])
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(format!("filelist=[\"{path}\"]"))
                .send()
                .await
                .unwrap();
            println!("raw body: HTTP {} {}", c.status(), c.text().await.unwrap_or_default());
        }
    }
}
