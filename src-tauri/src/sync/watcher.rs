//! 文件监控：notify 递归监听 + 有界防抖 → 合并后的触发信号。
//! 事件可能丢失（OS 限制），引擎另有周期兜底全量扫描。

use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

use notify::{EventKind, RecursiveMode, Watcher};

const QUIET_PERIOD: Duration = Duration::from_millis(500);
const MAX_BATCH_WAIT: Duration = Duration::from_secs(2);
const MIN_SIGNAL_GAP: Duration = Duration::from_secs(1);

pub struct WatchHandle {
    /// 防抖后触发信号（一轮同步建议）
    pub trigger: tokio::sync::mpsc::Receiver<()>,
    /// 持有 watcher 保活（drop 即停止监控）
    #[allow(dead_code)]
    watcher: notify::RecommendedWatcher,
}

/// 启动目录监控。返回触发信号接收端。
pub fn watch(root: PathBuf) -> anyhow::Result<WatchHandle> {
    let (event_tx, event_rx) = mpsc::channel(1);
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(ev) = res {
                // 只关心内容变化与创建/删除/改名；忽略普通元数据变化
                let relevant = match ev.kind {
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => true,
                    _ => false,
                };
                if relevant {
                    let _ = event_tx.try_send(());
                }
            }
        })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    // 一轮正在同步时只保留一次后续检查，不能积压过时的完整扫描。
    let (trigger_tx, trigger_rx) = mpsc::channel(1);
    tokio::spawn(debounce_events(event_rx, trigger_tx));

    Ok(WatchHandle {
        trigger: trigger_rx,
        watcher,
    })
}

async fn debounce_events(mut events: mpsc::Receiver<()>, trigger: mpsc::Sender<()>) {
    let mut next_allowed = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = trigger.closed() => return,
            event = events.recv() => if event.is_none() { return; },
        }
        let first = tokio::time::Instant::now();
        let max_deadline = first + MAX_BATCH_WAIT;
        let mut deadline = (first + QUIET_PERIOD).max(next_allowed).min(max_deadline);
        loop {
            // 定时器优先：持续就绪的事件不能饿死截止时间。
            tokio::select! { biased;
                _ = trigger.closed() => return,
                _ = tokio::time::sleep_until(deadline) => break,
                event = events.recv() => {
                    if event.is_none() { return; }
                    deadline = (tokio::time::Instant::now() + QUIET_PERIOD)
                        .max(next_allowed).min(max_deadline);
                }
            }
        }
        match trigger.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {
                // Full 表示下一轮已排队；其中一次扫描会包含当前全部变更。
                next_allowed = tokio::time::Instant::now() + MIN_SIGNAL_GAP;
            }
            Err(mpsc::error::TrySendError::Closed(())) => return,
        }
    }
}

impl WatchHandle {
    /// 暂停监控（同步进行中忽略触发由引擎负责，这里提供显式暂停能力）。
    pub fn _unused(&self) {}
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn slow_consumer_keeps_one_followup_and_drop_stops_debounce() {
        let (events, event_rx) = tokio::sync::mpsc::channel(1);
        let (trigger, trigger_rx) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(super::debounce_events(event_rx, trigger));
        events.send(()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(650)).await;
        assert_eq!(trigger_rx.len(), 1);
        events.send(()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert_eq!(trigger_rx.len(), 1, "queued followups must coalesce");
        assert!(
            !task.is_finished(),
            "full trigger queue must not stop monitoring"
        );
        drop(trigger_rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(events.is_closed());
    }

    #[tokio::test]
    async fn continuous_writes_have_bounded_trigger_latency() {
        let root =
            std::env::temp_dir().join(format!("synccloud-watch-busy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut handle = super::watch(root.clone()).unwrap();
        let written = root.join("background.tmp");
        let writer = tokio::spawn(async move {
            for n in 0..100 {
                tokio::fs::write(&written, n.to_string()).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });
        let triggered =
            tokio::time::timeout(std::time::Duration::from_secs(3), handle.trigger.recv()).await;
        writer.abort();
        let _ = writer.await;
        drop(handle);
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            matches!(triggered, Ok(Some(()))),
            "continuous local writes postponed realtime sync beyond three seconds"
        );
    }

    #[test]
    fn idle_watcher_does_not_block_runtime() {
        let root = std::env::temp_dir().join(format!("synccloud-watch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let watch = {
            let _guard = rt.enter();
            super::watch(root.clone()).unwrap()
        };
        let (tx, rx) = std::sync::mpsc::channel();
        rt.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            tx.send(()).unwrap();
        });
        let responsive = rx.recv_timeout(std::time::Duration::from_secs(2)).is_ok();
        drop(watch);
        rt.shutdown_timeout(std::time::Duration::from_secs(2));
        std::fs::remove_dir_all(root).unwrap();
        assert!(responsive, "idle file watcher starved a one-worker runtime");
    }
}
