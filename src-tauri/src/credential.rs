//! 凭据安全存储：敏感字段（密码、密钥）写入操作系统凭据管理器（Windows Credential Manager）。
//! account 命名规则：`<folder_id>:<field>`。

use keyring::Entry;

const SERVICE: &str = "SyncCloud";

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
