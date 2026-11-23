//! Small helpers: UTC time, shell quoting, path expansion, and redacted output.
//!
//! Every line Edition prints goes through [`say!`] or [`warn!`], which pass the text
//! through [`redact`], so a credential in a URL never reaches the terminal, whatever
//! config value or error it came from. Execution always uses the raw values.

/// Prints a line to standard output with URL credentials removed.
#[macro_export]
macro_rules! say {
    () => { println!() };
    ($($t:tt)*) => { println!("{}", $crate::util::redact(&format!($($t)*))) };
}

/// Prints a line to standard error with URL credentials removed.
#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { eprintln!("{}", $crate::util::redact(&format!($($t)*))) };
}

/// Prints text without a newline to standard output with URL credentials removed.
#[macro_export]
macro_rules! say_raw {
    ($($t:tt)*) => { print!("{}", $crate::util::redact(&format!($($t)*))) };
}

/// Prints text without a newline to standard error with URL credentials removed.
#[macro_export]
macro_rules! warn_raw {
    ($($t:tt)*) => { eprint!("{}", $crate::util::redact(&format!($($t)*))) };
}

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

/// A receipt as a dry run prints it: exactly 32 lowercase hex characters. Checked before
/// the value is used for anything, including as part of a file name.
pub fn valid_receipt(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// SHA-1 of `data`, hex-encoded (FIPS 180-4). Used only to compute git blob ids.
pub fn sha1_hex(data: &[u8]) -> String {
    let mut h = StreamHash::new(false);
    h.update(data);
    h.finish()
}

fn sha1_block(h: &mut [u32; 8], chunk: &[u8]) {
    let mut w = [0u32; 80];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = [h[0], h[1], h[2], h[3], h[4]];
    for (i, wi) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | (!b & d), 0x5A827999),
            20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
            _ => (b ^ c ^ d, 0xCA62C1D6),
        };
        let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = t;
    }
    for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
        *x = x.wrapping_add(y);
    }
}

/// Git's blob id for `text` (what `git hash-object --stdin` prints in a SHA-1 repository).
pub fn git_blob_id(text: &str) -> String {
    let mut data = format!("blob {}\0", text.len()).into_bytes();
    data.extend_from_slice(text.as_bytes());
    sha1_hex(&data)
}

/// SHA-256 of `data`, hex-encoded (FIPS 180-4).
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = StreamHash::new(true);
    h.update(data);
    h.finish()
}

const K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
    0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
    0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
    0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
    0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
    0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2,
];

fn sha256_block(h: &mut [u32; 8], chunk: &[u8]) {
    let mut w = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }
    let mut v = *h;
    for i in 0..64 {
        let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
        let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
        let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K256[i]).wrapping_add(w[i]);
        let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
        let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
        let t2 = s0.wrapping_add(maj);
        v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
    }
    for (a, b) in h.iter_mut().zip(v) {
        *a = a.wrapping_add(b);
    }
}

/// An incremental SHA-1 or SHA-256, so large inputs are hashed without being held.
pub struct StreamHash {
    sha256: bool,
    h: [u32; 8],
    buf: Vec<u8>,
    len: u64,
}

impl StreamHash {
    pub fn new(sha256: bool) -> StreamHash {
        let h = if sha256 {
            [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19]
        } else {
            [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0, 0, 0, 0]
        };
        StreamHash { sha256, h, buf: Vec::with_capacity(64), len: 0 }
    }

    fn block(&mut self, chunk: &[u8]) {
        if self.sha256 {
            sha256_block(&mut self.h, chunk)
        } else {
            sha1_block(&mut self.h, chunk)
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        if !self.buf.is_empty() {
            let take = (64 - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() < 64 {
                return;
            }
            let full = std::mem::take(&mut self.buf);
            self.block(&full);
        }
        let (blocks, rest) = data.as_chunks::<64>();
        for c in blocks {
            self.block(c);
        }
        self.buf.extend_from_slice(rest);
    }

    pub fn finish(mut self) -> String {
        let bits = self.len.wrapping_mul(8);
        let mut tail = std::mem::take(&mut self.buf);
        tail.push(0x80);
        while tail.len() % 64 != 56 {
            tail.push(0);
        }
        tail.extend_from_slice(&bits.to_be_bytes());
        for c in tail.chunks(64) {
            self.block(c);
        }
        let n = if self.sha256 { 8 } else { 5 };
        self.h[..n].iter().map(|x| format!("{x:08x}")).collect()
    }
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
    // The events file is the caller's choice of path; it is only ever appended to, and
    // never through a link.
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(mut f) =
        std::fs::OpenOptions::new().create(true).append(true).custom_flags(crate::steps::NOFOLLOW).open(&target)
    {
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
    fn sha1_and_receipts() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(git_blob_id(""), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        assert_eq!(git_blob_id("hello\n"), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert!(valid_receipt("0123456789abcdef0123456789abcdef"));
        for bad in [
            "../../victim",
            "/etc/passwd",
            "0123456789ABCDEF0123456789ABCDEF",
            "abc",
            &"a".repeat(33),
            &"a".repeat(4096),
        ] {
            assert!(!valid_receipt(bad), "{bad}");
        }
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

    /// Known answers from tools independent of this code: `shasum -a 1`, `shasum -a 256`
    /// and `openssl dgst -sha256` (which agree), over `n` bytes of `a`. The 1 000 000-byte
    /// vectors are also the published FIPS 180 ones.
    const A_VECTORS: &[(usize, &str, &str)] = &[
        (
            0,
            "da39a3ee5e6b4b0d3255bfef95601890afd80709",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            55,
            "c1c8bbdc22796e28c0e15163d20899b65621d65a",
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318",
        ),
        (
            56,
            "c2db330f6083854c99d4b5bfb6e8f29f201be699",
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a",
        ),
        (
            63,
            "03f09f5b158a7a8cdad920bddc29b81c18a551f5",
            "7d3e74a05d7db15bce4ad9ec0658ea98e3f06eeecf16b4c6fff2da457ddc2f34",
        ),
        (
            64,
            "0098ba824b5c16427bd7a1122a5a442a25ec644d",
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb",
        ),
        (
            65,
            "11655326c708d70319be2610e8a57d9a5b959d3b",
            "635361c48bb9eab14198e76ea8ab7f1a41685d6ad62aa9146d301d4f17eb0ae0",
        ),
        (
            119,
            "ee971065aaa017e0632a8ca6c77bb3bf8b1dfc56",
            "31eba51c313a5c08226adf18d4a359cfdfd8d2e816b13f4af952f7ea6584dcfb",
        ),
        (
            120,
            "f34c1488385346a55709ba056ddd08280dd4c6d6",
            "2f3d335432c70b580af0e8e1b3674a7c020d683aa5f73aaaedfdc55af904c21c",
        ),
        (
            1_000_000,
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f",
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
        ),
    ];

    #[test]
    fn sha_known_answers_at_the_padding_boundaries() {
        for (n, sha1, sha256) in A_VECTORS {
            let data = vec![b'a'; *n];
            assert_eq!(sha1_hex(&data), *sha1, "sha1 of {n} bytes");
            assert_eq!(sha256_hex(&data), *sha256, "sha256 of {n} bytes");
        }
    }

    #[test]
    fn sha_known_answers_for_a_multi_megabyte_input() {
        // (i * 7 + 3) mod 256 for 3 MiB + 17 bytes; digests from shasum.
        let data: Vec<u8> = (0..(3 * 1024 * 1024 + 17)).map(|i: usize| ((i * 7 + 3) % 256) as u8).collect();
        assert_eq!(sha1_hex(&data), "61889c7cfef045a8caf53e696db34a5054f2f168");
        assert_eq!(sha256_hex(&data), "af0420698203db612d2363019e5daab6c288a059aad9a9ee0270be2cd0f5bd4d");
        for sha256 in [false, true] {
            let mut h = StreamHash::new(sha256);
            for c in data.chunks(65_537) {
                h.update(c);
            }
            let want = if sha256 {
                "af0420698203db612d2363019e5daab6c288a059aad9a9ee0270be2cd0f5bd4d"
            } else {
                "61889c7cfef045a8caf53e696db34a5054f2f168"
            };
            assert_eq!(h.finish(), want);
        }
    }

    #[test]
    fn sha_updates_split_at_every_offset_around_the_block_boundaries() {
        // (i * 31 + 5) mod 256 for 200 bytes; digests from shasum.
        let data: Vec<u8> = (0..200usize).map(|i| ((i * 31 + 5) % 256) as u8).collect();
        let want = [
            (false, "4f346e1d048d0b60e1bff62d7a8a608bbf9a26fe"),
            (true, "650ff77e1121cdbf6069827a5da6a7a1d80ab96bdeb01b64da8a5b22a3be900a"),
        ];
        for (sha256, digest) in want {
            for i in 0..=data.len() {
                let mut h = StreamHash::new(sha256);
                h.update(&data[..i]);
                h.update(&data[i..]);
                assert_eq!(h.finish(), digest, "split at {i}");
            }
            // Three pieces with both cuts near the first two block boundaries.
            for i in 50..=70 {
                for j in i..=135.min(data.len()) {
                    if j < 115 && j != i {
                        continue;
                    }
                    let mut h = StreamHash::new(sha256);
                    h.update(&data[..i]);
                    h.update(&data[i..j]);
                    h.update(&data[j..]);
                    assert_eq!(h.finish(), digest, "split at {i} and {j}");
                }
            }
            let mut h = StreamHash::new(sha256);
            for b in &data {
                h.update(std::slice::from_ref(b));
            }
            assert_eq!(h.finish(), digest, "byte by byte");
        }
    }
}
