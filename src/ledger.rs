//! The ledger: an append-only TOML file of what went where.
//!
//! Each outward action (a deploy to a staging or prod channel, a pushed branch or tag,
//! an opened forward-merge pull request) appends one `[[entry]]`. Entries are never
//! edited; a correction is a new entry. Each entry carries `prev`, the hash of the
//! previous entry's canonical text, so an edit, removal or reordering breaks the chain.
//! `verify` checks the chain and that every committed version of the ledger on the main
//! branch's first-parent history is a prefix of the next.

use crate::git::Git;
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
    /// `ok`: every step succeeded. `partial`: the outward step succeeded and a later
    /// step failed. `failed`: the outward step failed or never ran.
    pub result: String,
    /// The hash of the previous entry's canonical text (`GENESIS` for the first).
    #[serde(default)]
    pub prev: String,
    /// The receipt this run was confirmed with.
    pub receipt: Option<String>,
    pub approved_by: Option<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    /// `name=commit (tag)` for every pin the shipped commit carried.
    #[serde(default)]
    pub pins: Vec<String>,
    /// Each step that ran, with its exit code.
    #[serde(default)]
    pub steps: Vec<String>,
    /// Content-check findings an operator explicitly allowed for this run.
    #[serde(default)]
    pub overrides: Vec<String>,
    pub note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    entry: Vec<Entry>,
}

pub const GENESIS: &str = "0000000000000000000000000000000000000000";

pub const HEADER: &str = "# Edition ledger: what went where. Append-only; written by `edition`, never by hand.\n# A correction is a new entry; see Edition's docs/design.md.\n";

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
    kv("prev", &e.prev);
    if let Some(v) = &e.receipt {
        kv("receipt", v);
    }
    if let Some(v) = &e.approved_by {
        kv("approved_by", v);
    }
    if let Some(v) = &e.note {
        kv("note", v);
    }
    for (k, v) in [
        ("command", &e.command),
        ("evidence", &e.evidence),
        ("pins", &e.pins),
        ("steps", &e.steps),
        ("overrides", &e.overrides),
    ] {
        if !v.is_empty() {
            s.push_str(&format!("{k} = {}\n", list(v)));
        }
    }
    s
}

/// The chain hash of an entry: Git's blob hash of its canonical rendering.
pub fn entry_hash(git: &Git, e: &Entry) -> Result<String, String> {
    let _ = git;
    Ok(crate::util::git_blob_id(&render(e)))
}

/// Checks the chain. Returns the first broken link as `(index, message)`.
pub fn check_chain(git: &Git, entries: &[Entry]) -> Result<(), (usize, String)> {
    let mut prev = GENESIS.to_string();
    for (i, e) in entries.iter().enumerate() {
        if e.prev != prev {
            return Err((i, format!("entry {} ({}) does not follow the entry before it", i + 1, e.id)));
        }
        prev = entry_hash(git, e).map_err(|m| (i, m))?;
    }
    Ok(())
}

/// The entry with URL credentials removed from every free-text field: nothing recorded
/// may carry them.
pub fn redacted(e: &Entry) -> Entry {
    let r = crate::util::redact;
    let o = |v: &Option<String>| v.as_deref().map(r);
    let l = |v: &[String]| v.iter().map(|x| r(x)).collect::<Vec<_>>();
    Entry {
        id: r(&e.id),
        at: e.at.clone(),
        by: r(&e.by),
        action: r(&e.action),
        product: r(&e.product),
        channel: o(&e.channel),
        tier: o(&e.tier),
        target: o(&e.target),
        ref_: r(&e.ref_),
        commit: e.commit.clone(),
        tree: e.tree.clone(),
        version: o(&e.version),
        platform: o(&e.platform),
        build: o(&e.build),
        result: e.result.clone(),
        prev: e.prev.clone(),
        receipt: e.receipt.clone(),
        approved_by: o(&e.approved_by),
        command: l(&e.command),
        evidence: l(&e.evidence),
        pins: l(&e.pins),
        steps: l(&e.steps),
        overrides: l(&e.overrides),
        note: o(&e.note),
    }
}

/// Appends one entry under a lock, setting its `prev` to the hash of the last entry.
/// Creates the file (with its header) if needed. Refuses a symlink or an invalid ledger.
pub fn append(git: &Git, path: &Path, e: &Entry) -> Result<Entry, String> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(format!("refusing to write the ledger through a symlink: {}", path.display()));
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let lock_path = path.with_extension("toml.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|err| format!("cannot open {}: {err}", lock_path.display()))?;
    lock.lock().map_err(|err| format!("cannot lock {}: {err}", lock_path.display()))?;
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let entries =
        parse(&existing).map_err(|err| format!("refusing to append to an invalid ledger {}: {err}", path.display()))?;
    check_chain(git, &entries).map_err(|(_, m)| format!("refusing to append to a broken ledger chain: {m}"))?;
    let mut e = redacted(e);
    e.prev = match entries.last() {
        Some(last) => entry_hash(git, last)?,
        None => GENESIS.to_string(),
    };
    let mut chunk = String::new();
    if existing.is_empty() {
        chunk.push_str(HEADER);
    } else if !existing.ends_with('\n') {
        chunk.push('\n');
    }
    chunk.push_str(&render(&e));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| format!("cannot open {}: {err}", path.display()))?;
    f.write_all(chunk.as_bytes()).map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    f.sync_all().map_err(|err| err.to_string())?;
    drop(lock);
    Ok(e)
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
            product: "inkwell".into(),
            channel: Some("staging-home".into()),
            tier: Some("staging".into()),
            target: Some("https://staging.example.com".into()),
            ref_: "main".into(),
            commit: "abc".into(),
            tree: Some("def".into()),
            version: Some("0.6.0-dev".into()),
            platform: Some("home".into()),
            build: None,
            result: "ok".into(),
            prev: GENESIS.into(),
            receipt: Some("0123456789abcdef".into()),
            approved_by: None,
            command: vec!["deploy.sh".into(), "release".into(), "abc".into()],
            evidence: vec!["check rust: exit 0".into(), "line\nbreak\\".into()],
            pins: vec![],
            steps: vec!["deploy: exit 0".into()],
            overrides: vec![],
            note: Some("first".into()),
        };
        let text = format!("{HEADER}{}", render(&e));
        let back = parse(&text).expect("parse");
        assert_eq!(back, vec![e]);
    }
}
