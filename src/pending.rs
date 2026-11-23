//! Pending outcomes: what a running plan will record, kept in `.edition/pending/` until
//! it is published on the ledger branch.
//!
//! Before the first step of a recording plan runs, Edition writes the prospective
//! entries here, rewrites them after every step, and removes the file once the ledger
//! push succeeds. If Edition is killed, or the remote cannot be reached afterwards, the
//! outcome survives and `edition ledger reconcile` publishes it without running
//! anything again. While a pending outcome exists, new outward plans are refused.

use crate::ledger::{self, Entry};
use std::path::{Path, PathBuf};

pub struct Pending {
    dir: PathBuf,
}

impl Pending {
    pub fn at(root: &Path) -> Pending {
        Pending { dir: root.join(".edition").join("pending") }
    }

    fn ensure(&self) -> Result<(), String> {
        let parent = self.dir.parent().ok_or("no state directory")?;
        crate::steps::ensure_private_dir(parent)?;
        let ignore = parent.join(".gitignore");
        if std::fs::symlink_metadata(&ignore).is_err() {
            let _ = std::fs::write(&ignore, "*\n");
        }
        crate::steps::ensure_private_dir(&self.dir)
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.toml"))
    }

    /// Writes (or rewrites) the pending entries for one run.
    pub fn write(&self, key: &str, entries: &[Entry]) -> Result<(), String> {
        self.ensure()?;
        let mut text = String::from(
            "# Pending Edition ledger entries: not yet published. Publish with `edition ledger reconcile`.\n",
        );
        for e in entries {
            text.push_str(&ledger::render(&ledger::redacted(e)));
        }
        crate::steps::write_private(&self.path(key), &text)
    }

    pub fn remove(&self, key: &str) -> Result<(), String> {
        match std::fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove a pending record: {e}")),
        }
    }

    /// Every pending run, oldest first: `(key, raw text, entries)`.
    pub fn list(&self) -> Result<Vec<(String, String, Vec<Entry>)>, String> {
        if std::fs::symlink_metadata(&self.dir).is_err() {
            return Ok(Vec::new());
        }
        self.ensure()?;
        let mut out = Vec::new();
        let mut names: Vec<_> = std::fs::read_dir(&self.dir)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|d| d.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        names.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        for p in names {
            let key = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let entries = ledger::parse(&text).map_err(|e| format!("{}: {e}", p.display()))?;
            out.push((key, text, entries));
        }
        Ok(out)
    }
}
