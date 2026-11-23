//! Small helpers: UTC time, shell quoting, path expansion.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Now as `YYYY-MM-DDTHH:MM:SSZ`. There is deliberately no override: receipt expiry and
/// ledger times always come from the system clock.
pub fn now_utc() -> String {
    format_utc(now_secs())
}

/// Seconds since the epoch, from the system clock.
pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `n` bytes from the operating system's random source, hex-encoded.
pub fn random_hex(n: usize) -> Result<String, String> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| format!("cannot read the system random source: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// SHA-256 of `data`, hex-encoded (FIPS 180-4).
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
        0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
        0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
        0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] =
        [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (a, b) in h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
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

/// Removes credentials from any URL in `text`: userinfo (`user:token@`) after `://` and
/// query strings or fragments. Used for everything Edition prints, records or emits;
/// the full value is kept only for hashing and execution.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        out.push_str(&rest[..i + 3]);
        let after = &rest[i + 3..];
        let end =
            after.find(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | ')' | '>' | ',')).unwrap_or(after.len());
        let token = &after[..end];
        let auth_end = token.find('/').unwrap_or(token.len());
        let mut t = match token[..auth_end].rfind('@') {
            Some(at) => token[at + 1..].to_string(),
            None => token.to_string(),
        };
        if let Some(q) = t.find(['?', '#']) {
            t.truncate(q);
        }
        out.push_str(&t);
        rest = &after[end..];
    }
    out.push_str(rest);
    out
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
            m.insert((*k).into(), serde_json::Value::String(redact(v)));
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
    fn sha256_vectors() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let long = vec![b'a'; 1000];
        assert_eq!(sha256_hex(&long), "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3");
        let r = random_hex(16).expect("random");
        assert_eq!(r.len(), 32);
        assert_ne!(r, random_hex(16).expect("random"));
    }

    #[test]
    fn redaction() {
        assert_eq!(redact("https://user:tok@host/o/n.git"), "https://host/o/n.git");
        assert_eq!(redact("to https://tok@host/x?access_token=abc done"), "to https://host/x done");
        assert_eq!(redact("file://u:p@localhost/srv/r.git"), "file://localhost/srv/r.git");
        assert_eq!(redact("git@host:o/n.git"), "git@host:o/n.git");
        assert_eq!(redact("'https://a:b@h/p' and https://c@d/e"), "'https://h/p' and https://d/e");
        assert_eq!(redact("no urls here"), "no urls here");
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
