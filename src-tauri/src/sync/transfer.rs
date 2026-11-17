//! 传输控制与限速工具：指数退避 + 令牌桶带宽限速（对标 GoodSync / Syncthing / Resilio Sync / PanguFlow）。
//! 支持全局与单通道实时流量整形，防止同步跑满带宽导致网络卡顿。

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;

/// 异步令牌桶带宽限速器（对标 Syncthing 与 GoodSync 流量整形）。
#[derive(Clone)]
pub struct BandwidthLimiter {
    rate_bytes_per_sec: Arc<AtomicU64>,
    state: Arc<TokioMutex<LimiterState>>,
}

struct LimiterState {
    tokens: f64,
    last_check: Instant,
}

impl BandwidthLimiter {
    pub fn new(kbps: u32) -> Self {
        let bytes_per_sec = (kbps as u64) * 1024;
        Self {
            rate_bytes_per_sec: Arc::new(AtomicU64::new(bytes_per_sec)),
            state: Arc::new(TokioMutex::new(LimiterState {
                tokens: bytes_per_sec as f64,
                last_check: Instant::now(),
            })),
        }
    }

    /// 动态热更新限速速率（KB/s，0 为不限速，实时生效）
    pub fn set_limit_kbps(&self, kbps: u32) {
        let bytes_per_sec = (kbps as u64) * 1024;
        self.rate_bytes_per_sec
            .store(bytes_per_sec, Ordering::Relaxed);
    }

    /// 消费指定字节。如果超出速率则平滑异步休眠等待，0 限速时零开销直通。
    pub async fn consume(&self, bytes: usize) {
        let mut remaining = bytes as f64;
        while remaining > 0.0 {
            let rate = self.rate_bytes_per_sec.load(Ordering::Relaxed);
            if rate == 0 {
                return;
            }
            let mut st = self.state.lock().await;
            let now = Instant::now();
            st.tokens = (st.tokens + now.duration_since(st.last_check).as_secs_f64() * rate as f64)
                .min(rate as f64);
            st.last_check = now;
            let taken = remaining.min(st.tokens);
            remaining -= taken;
            st.tokens -= taken;
            drop(st);
            if remaining > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64((remaining / rate as f64).min(0.1)))
                    .await;
            }
        }
    }
}

static GLOBAL_UP_LIMITER: OnceLock<BandwidthLimiter> = OnceLock::new();
static GLOBAL_DOWN_LIMITER: OnceLock<BandwidthLimiter> = OnceLock::new();

pub fn upload_limiter() -> &'static BandwidthLimiter {
    GLOBAL_UP_LIMITER.get_or_init(|| BandwidthLimiter::new(0))
}

pub fn download_limiter() -> &'static BandwidthLimiter {
    GLOBAL_DOWN_LIMITER.get_or_init(|| BandwidthLimiter::new(0))
}

/// 全局热应用上传与下载限速（设置变更时即刻生效，无需重启）。
pub fn apply_bandwidth_limits(up_kbps: u32, down_kbps: u32) {
    upload_limiter().set_limit_kbps(up_kbps);
    download_limiter().set_limit_kbps(down_kbps);
}

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
                let delay = retry_delay(attempt);
                on_fail(&e, attempt + 1, delay);
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

pub fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(1u64.checked_shl(attempt).unwrap_or(60).min(60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retries_back_off_exponentially() {
        let mut delays = Vec::new();
        let _ = with_retry(
            2,
            || async { Err::<(), _>(anyhow::anyhow!("transient")) },
            |_, _, delay| delays.push(delay.as_secs()),
        )
        .await;
        assert_eq!(delays, vec![1, 2]);
    }
}
