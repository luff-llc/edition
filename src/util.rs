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

/// Seconds since the epoch for `YYYY-MM-DDTHH:MM:SSZ`.
pub fn parse_utc(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z'
    {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

pub fn now_secs() -> u64 {
    parse_utc(&now_utc()).unwrap_or(0)
}

/// A ref, branch or tag name as Edition accepts it: no whitespace, no shell
/// metacharacters, no leading `-`, no `..`.
pub fn valid_ref(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with('-')
        && !s.contains("..")
        && !s.contains("//")
        && !s.ends_with('/')
        && !s.ends_with(".lock")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
}

/// A build label: letters, digits and dots.
pub fn valid_build(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
}

pub fn valid_commit(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// A person or agent name recorded in the ledger: printable, one line.
pub fn valid_actor(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && !s.chars().any(|c| c.is_control())
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
        assert_eq!(parse_utc("2026-11-23T08:35:00Z"), Some(1_795_422_900));
        assert_eq!(parse_utc("2000-02-29T00:00:00Z"), Some(951_782_400));
        assert_eq!(parse_utc("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn grammars() {
        assert!(valid_ref("release/0.6"));
        assert!(valid_ref("v0.6.0-rc.1"));
        for bad in ["", "-x", "a b", "a;b", "$(x)", "a`b`", "a|b", "a..b", "a\nb", "x/", "a'b", "a\"b"] {
            assert!(!valid_ref(bad), "{bad:?}");
        }
        assert!(valid_build("51"));
        assert!(valid_build("1.2.0.7"));
        for bad in ["", "1 2", "1;2", "-1", "$1", "1/2"] {
            assert!(!valid_build(bad), "{bad:?}");
        }
        assert!(!valid_actor("a\nb"));
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("release/0.6"), "release/0.6");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
