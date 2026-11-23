//! Small helpers: UTC time, shell quoting, path expansion.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Now as `YYYY-MM-DDTHH:MM:SSZ`. `EDITION_NOW` overrides it (tests, replays).
pub fn now_utc() -> String {
    if let Ok(v) = std::env::var("EDITION_NOW") {
        if !v.is_empty() {
            return v;
        }
    }
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format_utc(secs)
}

pub fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Quotes an argument for display so a printed command can be pasted into a shell.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,^{}~".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

pub fn shell_line(argv: &[String]) -> String {
    argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

/// Expands a leading `~/` and resolves a relative path against `base`.
pub fn resolve_path(base: &Path, p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Appends one JSON line to `$EDITION_EVENTS` (a file path, or `-` for standard error):
/// the worker contract. Without the variable nothing is written.
pub fn emit_event(kind: &str, fields: &[(&str, &str)]) {
    let Ok(target) = std::env::var("EDITION_EVENTS") else { return };
    if target.is_empty() {
        return;
    }
    let mut m = serde_json::Map::new();
    m.insert("event".into(), serde_json::Value::String(kind.into()));
    m.insert("at".into(), serde_json::Value::String(now_utc()));
    for (k, v) in fields {
        if !v.is_empty() {
            m.insert((*k).into(), serde_json::Value::String((*v).into()));
        }
    }
    let line = format!("{}\n", serde_json::Value::Object(m));
    if target == "-" {
        eprint!("{line}");
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&target) {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn short(commit: &str) -> &str {
    &commit[..commit.len().min(10)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_795_422_900), "2026-11-23T08:35:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("release/0.6"), "release/0.6");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
