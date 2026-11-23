//! The ledger: an append-only TOML file of what went where.
//!
//! Each outward action (a deploy to a staging or prod channel, a pushed branch or tag,
//! an opened forward-merge pull request) appends one `[[entry]]`. Entries are never
//! edited; a correction is a new entry. `verify` checks that the committed ledger is a
//! prefix of the working copy.

use serde::Deserialize;
use std::io::Write;
use std::path::Path;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub at: String,
    pub by: String,
    /// `deploy`, `push-branch`, `push-tag` or `open-pr`.
    pub action: String,
    pub product: String,
    pub channel: Option<String>,
    pub tier: Option<String>,
    pub target: Option<String>,
    #[serde(rename = "ref")]
    pub ref_: String,
    pub commit: String,
    pub tree: Option<String>,
    pub version: Option<String>,
    pub platform: Option<String>,
    pub build: Option<String>,
    /// `ok` or `failed`.
    pub result: String,
    pub approved_by: Option<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    /// `name=commit (tag)` for every pin the shipped commit carried.
    #[serde(default)]
    pub pins: Vec<String>,
    pub note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    entry: Vec<Entry>,
}

pub const HEADER: &str = "# Edition ledger: what went where. Append-only; written by `edition`, never by hand.\n# A correction is a new entry. See https://github.com/luff-llc/edition docs/design.md.\n";

pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let f: File = toml::from_str(text).map_err(|e| e.to_string())?;
    Ok(f.entry)
}

pub fn load(path: &Path) -> Result<Vec<Entry>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => parse(&t).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// TOML basic string.
fn q(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn list(v: &[String]) -> String {
    if v.is_empty() {
        return "[]".into();
    }
    format!("[\n{}]", v.iter().map(|s| format!("  {},\n", q(s))).collect::<String>())
}

pub fn render(e: &Entry) -> String {
    let mut s = String::from("\n[[entry]]\n");
    let mut kv = |k: &str, v: &str| s.push_str(&format!("{k} = {}\n", q(v)));
    kv("id", &e.id);
    kv("at", &e.at);
    kv("by", &e.by);
    kv("action", &e.action);
    kv("product", &e.product);
    for (k, v) in [("channel", &e.channel), ("tier", &e.tier), ("target", &e.target)] {
        if let Some(v) = v {
            kv(k, v);
        }
    }
    kv("ref", &e.ref_);
    kv("commit", &e.commit);
    for (k, v) in [("tree", &e.tree), ("version", &e.version), ("platform", &e.platform), ("build", &e.build)] {
        if let Some(v) = v {
            kv(k, v);
        }
    }
    kv("result", &e.result);
    if let Some(v) = &e.approved_by {
        kv("approved_by", v);
    }
    if let Some(v) = &e.note {
        kv("note", v);
    }
    for (k, v) in [("command", &e.command), ("evidence", &e.evidence), ("pins", &e.pins)] {
        if !v.is_empty() {
            s.push_str(&format!("{k} = {}\n", list(v)));
        }
    }
    s
}

/// Appends one entry, creating the file (with its header) if needed. Refuses a symlink.
pub fn append(path: &Path, e: &Entry) -> Result<(), String> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(format!("refusing to write the ledger through a symlink: {}", path.display()));
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    parse(&existing).map_err(|err| format!("refusing to append to an invalid ledger {}: {err}", path.display()))?;
    let mut chunk = String::new();
    if existing.is_empty() {
        chunk.push_str(HEADER);
    } else if !existing.ends_with('\n') {
        chunk.push('\n');
    }
    chunk.push_str(&render(e));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| format!("cannot open {}: {err}", path.display()))?;
    f.write_all(chunk.as_bytes()).map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    Ok(())
}

/// The latest successful deploy to each channel.
pub fn current_by_channel(entries: &[Entry]) -> Vec<(String, &Entry)> {
    let mut out: Vec<(String, &Entry)> = Vec::new();
    for e in entries.iter().filter(|e| e.action == "deploy" && e.result == "ok") {
        if let Some(ch) = &e.channel {
            out.retain(|(c, _)| c != ch);
            out.push((ch.clone(), e));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_parse_round_trip() {
        let e = Entry {
            id: "2026-11-23T10:00:00Z/staging-home/abc".into(),
            at: "2026-11-23T10:00:00Z".into(),
            by: "agent \"sol\"".into(),
            action: "deploy".into(),
            product: "colophon".into(),
            channel: Some("staging-home".into()),
            tier: Some("staging".into()),
            target: Some("https://colophon.toolsperson.com".into()),
            ref_: "main".into(),
            commit: "abc".into(),
            tree: Some("def".into()),
            version: Some("0.6.0-dev".into()),
            platform: Some("home".into()),
            build: None,
            result: "ok".into(),
            approved_by: None,
            command: vec!["colophon-home.sh".into(), "release".into(), "abc".into()],
            evidence: vec!["check rust: exit 0".into(), "line\nbreak\\".into()],
            pins: vec![],
            note: Some("first".into()),
        };
        let text = format!("{HEADER}{}", render(&e));
        let back = parse(&text).expect("parse");
        assert_eq!(back, vec![e]);
    }
}
