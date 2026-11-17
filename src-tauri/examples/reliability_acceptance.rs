//! Explicit, isolated live acceptance: cargo run --example reliability_acceptance -- --live
//! Reads existing configuration and vault tokens; never changes configured sync tasks.
use anyhow::Context;
use synccloud_lib::{
    config::{AppConfig, BackendConfig},
    storage::{aliyun::AliyunStore, baidu::BaiduStore, RemoteStore},
};

async fn check_files(store: &dyn RemoteStore, dir: &std::path::Path) -> anyhow::Result<()> {
    let source = dir.join("source");
    let downloaded = dir.join("downloaded");
    // Cross the Aliyun part boundary; deterministic content enables byte/hash checks.
    let bytes: Vec<u8> = (0..10 * 1024 * 1024 + 97)
        .map(|n| (n % 251) as u8)
        .collect();
    tokio::fs::write(&source, &bytes).await?;
    store
        .put_file("nested/acceptance.bin", &source)
        .await
        .context("首次分块上传")?;
    store
        .get_file("nested/acceptance.bin", &downloaded)
        .await
        .context("首次下载")?;
    anyhow::ensure!(
        synccloud_lib::sync::scanner::hash_file(&source)?
            == synccloud_lib::sync::scanner::hash_file(&downloaded)?,
        "首次内容哈希不同"
    );
    tokio::fs::write(&source, b"replacement content").await?;
    store
        .put_file("nested/acceptance.bin", &source)
        .await
        .context("覆盖上传")?;
    store
        .get_file("nested/acceptance.bin", &downloaded)
        .await
        .context("覆盖后下载")?;
    anyhow::ensure!(
        tokio::fs::read(&downloaded).await? == b"replacement content",
        "覆盖内容不同"
    );
    let mut manifest = synccloud_lib::sync::types::Manifest::new("acceptance");
    manifest.seq = 1;
    store.write_manifest(&manifest).await?;
    anyhow::ensure!(store.read_manifest().await?.is_some(), "清单回读缺失");
    let stamp = store.manifest_stamp().await?;
    anyhow::ensure!(stamp.is_some(), "变更戳缺失");
    store
        .delete_file("nested/acceptance.bin")
        .await
        .context("删除文件")?;
    anyhow::ensure!(
        !store
            .list_objects()
            .await?
            .iter()
            .any(|v| v.key == "nested/acceptance.bin"),
        "删除后文件仍存在"
    );
    Ok(())
}

async fn check(
    store: &dyn RemoteStore,
    dir: &std::path::Path,
    hold: std::time::Duration,
) -> anyhow::Result<()> {
    store.begin_cycle().await?;
    let lease = synccloud_lib::storage::acquire_remote_lock(
        store,
        "acceptance",
        std::time::Duration::from_secs(5),
    )
    .await
    .context("验收获取远端锁")?;
    let result = {
        let work = async {
            check_files(store, dir)
                .await
                .context("验收上传/下载/清单操作")?;
            if !hold.is_zero() {
                tokio::time::sleep(hold).await;
            }
            lease
                .verify(store.raw_io())
                .await
                .context("验收持锁结束复核")
        };
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => result,
            _ = lease.heartbeat(store.raw_io(), "acceptance") => unreachable!(),
        }
    };
    lease.release(store.raw_io()).await;
    result
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::args().any(|s| s == "--live"),
        "必须显式传入 --live，仅操作新建验收目录"
    );
    let config: AppConfig = serde_json::from_slice(&std::fs::read(
        synccloud_lib::config::app_data_dir().join("config.json"),
    )?)?;
    let cleanup_only =
        std::env::args().find_map(|s| s.strip_prefix("--cleanup-root=").map(str::to_owned));
    let hold_seconds = std::env::args()
        .find_map(|s| s.strip_prefix("--hold-lease-secs=").map(str::to_owned))
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(0);
    anyhow::ensure!(hold_seconds <= 300, "验收持锁等待上限为 300 秒");
    let run = uuid::Uuid::new_v4();
    let root = cleanup_only
        .clone()
        .unwrap_or_else(|| format!("/SyncCloud-acceptance-{run}"));
    let local = std::env::temp_dir().join(format!("synccloud-acceptance-{run}"));
    std::fs::create_dir_all(&local)?;
    println!("Dedicated remote root: {root}");
    let mut failures = Vec::new();
    for provider in ["aliyun", "baidu"] {
        let Some(mut backend) = config
            .folders
            .iter()
            .find_map(|f| match (&f.backend, provider) {
                (b @ BackendConfig::AliyunDrive { .. }, "aliyun")
                | (b @ BackendConfig::BaiduPan { .. }, "baidu") => Some(b.clone()),
                _ => None,
            })
        else {
            continue;
        };
        backend.resolve_credentials()?;
        let (store, cleanup): (Box<dyn RemoteStore>, BackendConfig) = match backend {
            BackendConfig::AliyunDrive { auth_id, .. } => {
                let cfg = BackendConfig::AliyunDrive {
                    auth_id: auth_id.clone(),
                    account_name: "acceptance".into(),
                    app_id: "".into(),
                    app_secret: "".into(),
                    remote_dir: root.clone(),
                };
                (Box::new(AliyunStore::new(auth_id, root.clone())), cfg)
            }
            BackendConfig::BaiduPan {
                auth_id,
                app_key,
                secret_key,
                ..
            } => {
                let cfg = BackendConfig::BaiduPan {
                    auth_id: auth_id.clone(),
                    account_name: "acceptance".into(),
                    app_key: app_key.clone(),
                    secret_key: secret_key.clone(),
                    remote_dir: root.clone(),
                };
                (
                    Box::new(BaiduStore::new(auth_id, app_key, secret_key, root.clone())),
                    cfg,
                )
            }
            _ => unreachable!(),
        };
        println!("{provider}: starting isolated acceptance");
        let result = if cleanup_only.is_some() {
            Ok(Ok(()))
        } else {
            tokio::time::timeout(
                std::time::Duration::from_secs(600),
                check(
                    store.as_ref(),
                    &local,
                    std::time::Duration::from_secs(hold_seconds),
                ),
            )
            .await
        };
        match result {
            Ok(Ok(())) if cleanup_only.is_some() => {}
            Ok(Ok(())) => {
                println!("{provider}: upload/download/overwrite/delete/hash/manifest/lock PASS (extra lease hold: {hold_seconds}s)")
            }
            other => {
                let message = format!("{provider}: {other:?}");
                println!("{message}");
                failures.push(message);
            }
        }
        let cleaned = match cleanup {
            BackendConfig::AliyunDrive { auth_id, .. } => {
                AliyunStore::new(auth_id, root.clone())
                    .cleanup_acceptance_root()
                    .await
            }
            BackendConfig::BaiduPan {
                auth_id,
                app_key,
                secret_key,
                ..
            } => {
                BaiduStore::new(auth_id, app_key, secret_key, root.clone())
                    .cleanup_acceptance_root()
                    .await
            }
            _ => unreachable!(),
        };
        match cleaned {
            Ok(()) => println!("{provider}: dedicated test root cleaned"),
            Err(e) => {
                println!("{provider}: cleanup failed: {e:#}");
                failures.push(format!("{provider} cleanup: {e:#}"));
            }
        }
    }
    std::fs::remove_dir_all(local)?;
    anyhow::ensure!(
        failures.is_empty(),
        "{} 验收或清理操作未通过，见上方无凭据日志",
        failures.len()
    );
    Ok(())
}
