//! Dropbox 风格冲突副本命名：`报告 (冲突副本 - MachineA 2026-10-12 1430).docx`

use chrono::{DateTime, Local};

/// 生成冲突副本文件名（仅文件名部分，不含目录）。
/// - `报告.docx` → `报告 (冲突副本 - MachineA 2026-10-12 1430).docx`
/// - `报告` → `报告 (冲突副本 - MachineA 2026-10-12 1430)`
/// 同名冲突副本已存在时追加序号。
pub fn conflict_copy_name(orig_name: &str, device: &str, at: DateTime<Local>) -> String {
    let safe_device = sanitize_component(device);
    let stamp = at.format("%Y-%m-%d %H%M").to_string();
    let (stem, ext) = split_ext(orig_name);
    let base = if ext.is_empty() {
        format!("{stem} (冲突副本 - {safe_device} {stamp})")
    } else {
        format!("{stem} (冲突副本 - {safe_device} {stamp}).{ext}")
    };
    base
}

/// 带去重：目标目录中已存在同名副本时追加 ` (2)`、` (3)`……
pub fn unique_conflict_copy(
    dir: &std::path::Path,
    orig_name: &str,
    device: &str,
    at: DateTime<Local>,
) -> String {
    let base = conflict_copy_name(orig_name, device, at);
    if !dir.join(&base).exists() {
        return base;
    }
    let (stem, ext) = split_ext(&base);
    for i in 2..1000 {
        let cand = if ext.is_empty() {
            format!("{stem} ({i})")
        } else {
            format!("{stem} ({i}).{ext}")
        };
        if !dir.join(&cand).exists() {
            return cand;
        }
    }
    format!("{base}.{}", at.timestamp_millis())
}

/// 分离扩展名：`a.tar.gz` → (`a.tar`, `gz`)；隐藏文件 `.gitignore` → (`.gitignore`, ``)。
fn split_ext(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(pos) if pos > 0 => (name[..pos].to_string(), name[pos + 1..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

/// 设备名里的文件系统非法字符替换（Windows: \ / : * ? " < > |）。
fn sanitize_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let bad = matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|');
        if bad {
            out.push('_');
        } else {
            out.push(c);
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        "device".into()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt() -> DateTime<Local> {
        // 固定时间避免测试脆弱（解析带偏移的 RFC3339 再转本地时区）
        "2026-10-12T14:30:00+08:00"
            .parse::<DateTime<chrono::FixedOffset>>()
            .unwrap()
            .with_timezone(&Local)
    }

    #[test]
    fn with_extension() {
        assert_eq!(
            conflict_copy_name("报告.docx", "MachineA", dt()),
            "报告 (冲突副本 - MachineA 2026-10-12 1430).docx"
        );
    }

    #[test]
    fn without_extension() {
        assert_eq!(
            conflict_copy_name("README", "PC", dt()),
            "README (冲突副本 - PC 2026-10-12 1430)"
        );
    }

    #[test]
    fn dotfile_keeps_name() {
        assert_eq!(
            conflict_copy_name(".gitignore", "PC", dt()),
            ".gitignore (冲突副本 - PC 2026-10-12 1430)"
        );
    }

    #[test]
    fn device_name_sanitized() {
        assert_eq!(
            conflict_copy_name("a.txt", "WIN:C*2", dt()),
            "a (冲突副本 - WIN_C_2 2026-10-12 1430).txt"
        );
    }

    #[test]
    fn unique_dedup() {
        let dir = std::env::temp_dir().join("synccloud-test-conflict");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a (冲突副本 - PC 2026-10-12 1430).txt"), b"x").unwrap();
        let name = unique_conflict_copy(&dir, "a.txt", "PC", dt());
        assert_eq!(name, "a (冲突副本 - PC 2026-10-12 1430) (2).txt");
    }
}
