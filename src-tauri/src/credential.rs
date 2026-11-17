//! 凭据安全存储：敏感字段（密码、密钥）写入操作系统凭据管理器（Windows Credential Manager）。
//! account 命名规则：`<folder_id>:<field>`。

use keyring::Entry;

const SERVICE: &str = "SyncCloud";

/// 同账户的刷新、保存及注销共用互斥锁，避免轮换令牌被旧实例覆盖。
pub fn account_lock(account: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(account.into())
        .or_default()
        .clone()
}

fn entry(account: &str) -> anyhow::Result<Entry> {
    Ok(Entry::new(SERVICE, account)?)
}

pub fn save_secret(account: &str, value: &str) -> anyhow::Result<()> {
    entry(account)?.set_password(value)?;
    Ok(())
}

pub fn load_secret(account: &str) -> anyhow::Result<Option<String>> {
    let e = entry(account)?;
    match e.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete_secret(account: &str) -> anyhow::Result<()> {
    match entry(account)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn resolve(value: &str) -> anyhow::Result<String> {
    if let Some(key) = value.strip_prefix("keyring:") {
        load_secret(key)?.ok_or_else(|| anyhow::anyhow!("凭据不存在，请重新输入或授权: {key}"))
    } else {
        Ok(value.to_string())
    }
}

pub fn protect(value: &mut String, key: &str) -> anyhow::Result<()> {
    if !value.is_empty() && !value.starts_with("keyring:") {
        let key = format!("{key}:{}", uuid::Uuid::new_v4());
        save_secret(&key, value)?;
        anyhow::ensure!(
            load_secret(&key)?.as_deref() == Some(value.as_str()),
            "凭据回读校验失败"
        );
        *value = format!("keyring:{key}");
    }
    Ok(())
}
