//! 时间、时长与 CSV 的小工具。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Local, SecondsFormat};
use anyhow::Result;

pub fn rfc3339(t: DateTime<Local>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, false)
}

pub fn unix(t: DateTime<Local>) -> i64 {
    t.timestamp()
}

/// 人读时长：`6d17h` / `3h07m` / `12m` / `45s`
pub fn dur_text(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 86_400 {
        format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600)
    } else if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// 追加一行到 CSV，顺带确保父目录存在。
pub fn append_line(path: &Path, line: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// 读一个 CSV 文件的所有行（跳过表头与空行）。
pub fn read_rows(path: &Path) -> Vec<Vec<String>> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split(',').map(|c| c.trim().to_string()).collect())
        .collect()
}

/// 写 JSON 状态文件（先写 .tmp 再替换）。
pub fn write_json_atomic(path: &Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    // Windows 上 rename 到已存在的文件会失败，先删再改名。
    // （等价于 MoveFileExW 的 MOVEFILE_REPLACE_EXISTING）
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&tmp, path)?;
    Ok(())
}
