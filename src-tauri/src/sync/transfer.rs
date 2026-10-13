//! 传输重试工具：指数退避。Windows 上文件被占用（编辑器/杀毒扫描）是常态，
//! 上传下载与本地替换都可能瞬时失败，必须重试。

use std::future::Future;
use std::time::Duration;

/// 指数退避重试。每次失败调用 on_fail（用于事件上报），全部失败后返回最后一次错误。
/// 生命周期 'a 允许闭包返回借用外部状态的 future。
pub async fn with_retry<'a, F, Fut, T>(
    max_retries: u32,
    mut f: F,
    mut on_fail: impl FnMut(&anyhow::Error, u32, Duration),
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut + 'a,
    Fut: Future<Output = anyhow::Result<T>> + 'a,
{
    let mut attempt: u32 = 0;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt >= max_retries {
                    return Err(e);
                }
                let delay = Duration::from_secs(1u64.saturating_pow(attempt + 1).min(60));
                on_fail(&e, attempt + 1, delay);
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}
