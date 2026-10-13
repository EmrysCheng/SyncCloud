//! 客户端加密（零知识）：passphrase → Argon2id 派生密钥 → XChaCha20-Poly1305 流式 AEAD（STREAM LE31）。
//!
//! 文件格式：
//! ```text
//! [magic "SCENC1\0" 7B][salt 16B][nonce 20B][密文块*（每块 = 64KiB 明文 + 16B tag）]
//! ```
//! 非末块用 `encrypt_next`，末块（不足 64KiB）用 `encrypt_last` 标记流结束。

use std::io::{Read, Write};
use std::path::Path;

use chacha20poly1305::aead::stream::{DecryptorLE31, EncryptorLE31};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{Key, XChaCha20Poly1305};
use generic_array::GenericArray;
use rand::RngCore;
use zeroize::Zeroize;

pub const MAGIC: &[u8; 7] = b"SCENC1\0";
const CHUNK: usize = 64 * 1024;
const TAG: usize = 16;
/// LE31 流 nonce = XChaCha 24B - 4B 计数器前缀
const STREAM_NONCE: usize = 20;
/// 空关联数据（Payload 要求 &[u8] 切片，数组引用不满足 From）
const AD: &[u8] = b"";

/// Argon2id 派生 32 字节密钥。
pub fn derive_key(passphrase: &str, salt: &[u8]) -> anyhow::Result<[u8; 32]> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(64 * 1024, 3, 4, Some(32))
        .map_err(|e| anyhow::anyhow!("Argon2 参数错误: {e}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow::anyhow!("密钥派生失败: {e}"))?;
    Ok(key)
}

/// 生成随机盐。
pub fn random_salt() -> [u8; 16] {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    salt
}

fn read_chunk<R: Read>(r: &mut R, max: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(max.min(64 * 1024));
    let mut chunk = vec![0u8; max.min(64 * 1024)];
    while buf.len() < max {
        let want = (max - buf.len()).min(chunk.len());
        let n = r.read(&mut chunk[..want])?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

/// 加密文件（流式，64KiB 块 AEAD）。
pub fn encrypt_file(src: &Path, dest: &Path, key: &[u8; 32], salt: &[u8]) -> anyhow::Result<()> {
    let mut nonce_bytes = [0u8; STREAM_NONCE];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = GenericArray::from_slice(&nonce_bytes);
    let mut stream: EncryptorLE31<XChaCha20Poly1305> =
        EncryptorLE31::new(GenericArray::from_slice(key), nonce);

    let mut reader = std::fs::File::open(src)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut writer = std::fs::File::create(dest)?;
    writer.write_all(MAGIC)?;
    writer.write_all(salt)?;
    writer.write_all(&nonce_bytes)?;

    loop {
        let mut chunk = read_chunk(&mut reader, CHUNK)?;
        if chunk.len() < CHUNK {
            // 末块：encrypt_last_in_place 消费 self（流终结语义），随后即退出
            stream
                .encrypt_last_in_place(AD, &mut chunk)
                .map_err(|_| anyhow::anyhow!("加密块失败"))?;
            writer.write_all(&chunk)?;
            break;
        }
        stream
            .encrypt_next_in_place(AD, &mut chunk)
            .map_err(|_| anyhow::anyhow!("加密块失败"))?;
        writer.write_all(&chunk)?;
    }
    writer.flush()?;
    Ok(())
}

/// 解密文件（流式）。
pub fn decrypt_file(src: &Path, dest: &Path, key: &[u8; 32]) -> anyhow::Result<()> {
    let mut reader = std::fs::File::open(src)?;
    let mut magic = [0u8; 7];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        anyhow::bail!("文件不是 SyncCloud 加密格式");
    }
    let mut salt = [0u8; 16];
    reader.read_exact(&mut salt)?;
    let mut nonce_bytes = [0u8; STREAM_NONCE];
    reader.read_exact(&mut nonce_bytes)?;

    let nonce = GenericArray::from_slice(&nonce_bytes);
    let mut stream: DecryptorLE31<XChaCha20Poly1305> =
        DecryptorLE31::new(GenericArray::from_slice(key), nonce);

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut writer = std::fs::File::create(dest)?;
    loop {
        let mut chunk = read_chunk(&mut reader, CHUNK + TAG)?;
        if chunk.is_empty() {
            anyhow::bail!("加密文件损坏：缺少结束块");
        }
        if chunk.len() < CHUNK + TAG {
            // 末块：decrypt_last_in_place 消费 self，随后即退出
            stream
                .decrypt_last_in_place(AD, &mut chunk)
                .map_err(|_| anyhow::anyhow!("解密失败（密码错误或文件损坏）"))?;
            writer.write_all(&chunk)?;
            break;
        }
        stream
            .decrypt_next_in_place(AD, &mut chunk)
            .map_err(|_| anyhow::anyhow!("解密失败（密码错误或文件损坏）"))?;
        writer.write_all(&chunk)?;
    }
    writer.flush()?;
    Ok(())
}

/// 安全清除密钥。
pub fn zero(key: &mut [u8; 32]) {
    key.zeroize();
}

/// 文件名混淆：相对路径 → 远端对象名（base64url(nonce || 加密路径)）。
pub fn encrypt_name(rel: &str, key: &[u8; 32]) -> String {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let mut pt = rel.as_bytes().to_vec();
    let ct = cipher.encrypt(&nonce, pt.as_ref()).expect("加密路径");
    pt.zeroize();
    let mut blob = nonce.to_vec();
    blob.extend_from_slice(&ct);
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(blob)
}

/// 文件名还原。
pub fn decrypt_name(enc: &str, key: &[u8; 32]) -> Option<String> {
    use base64::Engine;
    let blob = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(enc).ok()?;
    if blob.len() < 24 + TAG {
        return None;
    }
    let (nonce, ct) = blob.split_at(24);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let pt = cipher.decrypt(chacha20poly1305::XNonce::from_slice(nonce), ct).ok()?;
    String::from_utf8(pt).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let dir = std::env::temp_dir().join("synccloud-test-crypto");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("plain.bin");
        // 200KB：跨多个 64KiB 块
        let data: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &data).unwrap();

        let salt = random_salt();
        let key = derive_key("正确的马蹄钉", &salt).unwrap();
        let enc = dir.join("plain.bin.enc");
        encrypt_file(&src, &enc, &key, &salt).unwrap();
        assert_ne!(std::fs::read(&enc).unwrap(), data);

        let dec = dir.join("plain.bin.dec");
        decrypt_file(&enc, &dec, &key).unwrap();
        assert_eq!(std::fs::read(&dec).unwrap(), data);
    }

    #[test]
    fn wrong_key_fails() {
        let dir = std::env::temp_dir().join("synccloud-test-crypto2");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("p.txt");
        std::fs::write(&src, b"secret").unwrap();
        let salt = random_salt();
        let k1 = derive_key("pass1", &salt).unwrap();
        let k2 = derive_key("pass2", &salt).unwrap();
        let enc = dir.join("p.enc");
        encrypt_file(&src, &enc, &k1, &salt).unwrap();
        let dec = dir.join("p.dec");
        assert!(decrypt_file(&enc, &dec, &k2).is_err());
    }

    #[test]
    fn empty_file_roundtrip() {
        let dir = std::env::temp_dir().join("synccloud-test-crypto3");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("empty.txt");
        std::fs::write(&src, b"").unwrap();
        let salt = random_salt();
        let key = derive_key("p", &salt).unwrap();
        let enc = dir.join("e.enc");
        encrypt_file(&src, &enc, &key, &salt).unwrap();
        let dec = dir.join("e.dec");
        decrypt_file(&enc, &dec, &key).unwrap();
        assert_eq!(std::fs::read(&dec).unwrap(), b"");
    }

    #[test]
    fn exact_chunk_boundary_roundtrip() {
        // 恰好 64KiB 整数倍：多块全满 + 空 last 块场景
        let dir = std::env::temp_dir().join("synccloud-test-crypto4");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("exact.bin");
        let data = vec![7u8; CHUNK * 2];
        std::fs::write(&src, &data).unwrap();
        let salt = random_salt();
        let key = derive_key("p", &salt).unwrap();
        let enc = dir.join("e.enc");
        encrypt_file(&src, &enc, &key, &salt).unwrap();
        let dec = dir.join("e.dec");
        decrypt_file(&enc, &dec, &key).unwrap();
        assert_eq!(std::fs::read(&dec).unwrap(), data);
    }

    #[test]
    fn name_obfuscation_roundtrip() {
        let salt = random_salt();
        let key = derive_key("p", &salt).unwrap();
        let orig = "文档/2026 报告.docx";
        let enc = encrypt_name(orig, &key);
        assert!(!enc.contains("报告"));
        assert!(!enc.contains('/'));
        assert_eq!(decrypt_name(&enc, &key).unwrap(), orig);
    }
}
