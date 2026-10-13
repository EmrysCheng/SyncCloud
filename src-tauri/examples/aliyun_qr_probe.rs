//! 阿里云盘免 AppId 扫码链路探针：生成二维码 → 轮询一次（应为 WaitLogin）。
//! 真正的扫码确认需要用户用 App 完成，此处只验证代码路径可用。
#[tokio::main]
async fn main() {
    println!("== start_qr_login ==");
    match synccloud_lib::storage::aliyun::start_qr_login().await {
        Ok((qr, link, sid)) => {
            println!("二维码内容: {qr}");
            println!("浏览器兜底: {link}");
            println!("sid: {sid}");
            println!("\n== poll 一次 ==");
            match synccloud_lib::storage::aliyun::poll_qr_login(&sid, None).await {
                Ok(false) => println!("OK: 等待扫码（WaitLogin）"),
                Ok(true) => println!("OK: 已登录（不应发生——没人扫码）"),
                Err(e) => println!("ERR: {e:#}"),
            }
        }
        Err(e) => println!("ERR: {e:#}"),
    }
}
