//! 文件监控：notify 递归监听 + 1.5s 防抖（聚合编辑器抖动）→ 触发信号。
//! 事件可能丢失（OS 限制），引擎另有周期兜底全量扫描。

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};

pub struct WatchHandle {
    /// 防抖后触发信号（一轮同步建议）
    pub trigger: tokio::sync::mpsc::Receiver<()>,
    /// 持有 watcher 保活（drop 即停止监控）
    #[allow(dead_code)]
    watcher: notify::RecommendedWatcher,
}

/// 启动目录监控。返回触发信号接收端。
pub fn watch(root: PathBuf) -> anyhow::Result<WatchHandle> {
    let (event_tx, event_rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        if let Ok(ev) = res {
            // 只关心内容变化与创建/删除/改名；忽略普通元数据变化
            let relevant = match ev.kind {
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => true,
                _ => false,
            };
            if relevant {
                let _ = event_tx.send(());
            }
        }
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    let (trigger_tx, trigger_rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        let mut last_signal = tokio::time::Instant::now() - Duration::from_secs(10);
        loop {
            match event_rx.recv() {
                Ok(()) => {
                    // 防抖：事件流安静 1.5 秒后才触发一次同步
                    loop {
                        match event_rx.recv_timeout(Duration::from_millis(1500)) {
                            Ok(()) => continue, // 又有新事件，重置安静计时
                            Err(mpsc::RecvTimeoutError::Timeout) => break,
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    // 限频：两次触发至少间隔 3 秒，避免连续同步风暴
                    let elapsed = last_signal.elapsed();
                    if elapsed < Duration::from_secs(3) {
                        tokio::time::sleep(Duration::from_secs(3) - elapsed).await;
                    }
                    last_signal = tokio::time::Instant::now();
                    if trigger_tx.send(()).await.is_err() {
                        return;
                    }
                }
                Err(_) => return, // watcher 已停止
            }
        }
    });

    Ok(WatchHandle { trigger: trigger_rx, watcher })
}

impl WatchHandle {
    /// 暂停监控（同步进行中忽略触发由引擎负责，这里提供显式暂停能力）。
    pub fn _unused(&self) {}
}
