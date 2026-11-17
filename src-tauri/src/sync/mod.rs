pub mod conflict;
pub mod engine;
pub mod fslock;
pub mod history;
pub mod manager;
pub mod plan;
pub mod recycle;
pub mod scanner;
pub mod state;
pub mod storage_mode;
pub mod transfer;
pub mod types;
pub mod watcher;

/// All local mutations for a folder share this gate, including shell hydration.
pub async fn operation_lock(id: &uuid::Uuid) -> anyhow::Result<tokio::sync::OwnedMutexGuard<()>> {
    if MAINTENANCE.load(std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("数据目录正在迁移或等待重启，请重启后操作");
    }
    let guard = drain_operation_lock(id).await;
    if MAINTENANCE.load(std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("数据目录正在迁移或等待重启，请重启后操作");
    }
    Ok(guard)
}
/// Migration keeps the gate closed while draining operations already in flight.
pub(crate) async fn drain_operation_lock(id: &uuid::Uuid) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        static LOCKS: std::sync::OnceLock<
            std::sync::Mutex<
                std::collections::HashMap<uuid::Uuid, std::sync::Arc<tokio::sync::Mutex<()>>>,
            >,
        > = std::sync::OnceLock::new();
        LOCKS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .entry(*id)
            .or_default()
            .clone()
    };
    lock.lock_owned().await
}
pub static MAINTENANCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn data_gate() -> &'static std::sync::Arc<tokio::sync::RwLock<()>> {
    static GATE: std::sync::OnceLock<std::sync::Arc<tokio::sync::RwLock<()>>> =
        std::sync::OnceLock::new();
    GATE.get_or_init(Default::default)
}
/// IPC writes retain shared access through persistence, including queued config
/// changes and OAuth completion. Migration takes exclusive access first.
pub(crate) async fn data_write_guard() -> anyhow::Result<tokio::sync::OwnedRwLockReadGuard<()>> {
    anyhow::ensure!(
        !MAINTENANCE.load(std::sync::atomic::Ordering::SeqCst),
        "迁移后请重启程序再修改数据"
    );
    let guard = data_gate().clone().read_owned().await;
    anyhow::ensure!(
        !MAINTENANCE.load(std::sync::atomic::Ordering::SeqCst),
        "迁移后请重启程序再修改数据"
    );
    Ok(guard)
}
pub(crate) async fn data_migration_guard() -> tokio::sync::OwnedRwLockWriteGuard<()> {
    data_gate().clone().write_owned().await
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    #[tokio::test]
    async fn migration_drains_writes_and_rejects_queued_or_late_writes() {
        let active = data_write_guard().await.unwrap();
        let mut migration = Box::pin(data_migration_guard());
        assert!(futures::poll!(migration.as_mut()).is_pending());
        drop(active);
        let exclusive = migration.await;
        let mut queued = Box::pin(data_write_guard());
        assert!(futures::poll!(queued.as_mut()).is_pending());
        MAINTENANCE.store(true, std::sync::atomic::Ordering::SeqCst);
        drop(exclusive);
        let result = queued.await;
        let late = data_write_guard().await;
        MAINTENANCE.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(result.is_err() && late.is_err());
    }
}
