//! The ledger lives on its own orphan branch (default `edition/ledger`), never in the
//! product's history, so recording a deploy does not move `main`.
//!
//! The branch holds one file, `ledger.toml`; each recorded action is one commit that
//! appends one entry. Edition writes it with plumbing (`hash-object`, `mktree`,
//! `commit-tree`) without touching the user's checkout, and publishes it with a
//! compare-and-swap push. A local copy of the last remote tip it saw is kept in
//! `refs/edition/ledger`; a remote tip that does not descend from it means the remote
//! branch was rewritten.

use crate::git::Git;
use crate::ledger::{self, Entry};

pub const FILE: &str = "ledger.toml";

/// The local copy of the remote ledger branch's tip.
pub const CACHE: &str = "refs/edition/ledger";

const INCOMING: &str = "refs/edition/ledger-incoming";

/// How many times a recorder re-reads, re-chains and retries when another recorder
/// won the race to push.
pub const ATTEMPTS: usize = 8;

pub struct Store<'a> {
    pub git: &'a Git,
    pub branch: String,
}

impl<'a> Store<'a> {
    pub fn new(git: &'a Git, branch: &str) -> Store<'a> {
        Store { git, branch: branch.to_string() }
    }

    fn remote_ref(&self) -> String {
        format!("refs/heads/{}", self.branch)
    }

    /// The ledger text at a commit (empty when the commit has no ledger file).
    pub fn text_at(&self, commit: &str) -> String {
        self.git.show(commit, FILE).unwrap_or_default()
    }

    /// The local copy's tip, if any.
    pub fn cached(&self) -> Option<String> {
        self.git.resolve(CACHE)
    }

    /// Entries as last seen (offline: unverified against the remote).
    pub fn load_local(&self) -> Result<Vec<Entry>, String> {
        match self.cached() {
            Some(c) => ledger::parse_ledger(&self.text_at(&c)).map_err(|e| format!("{}:{FILE}: {e}", self.branch)),
            None => Ok(Vec::new()),
        }
    }

    /// The remote tip of the ledger branch, fetched so its objects are local.
    pub fn fetch(&self, url: &str) -> Result<Option<String>, String> {
        let (heads, _) = self.git.ls_remote(url)?;
        let Some(tip) = heads.into_iter().find(|(n, _)| *n == self.branch).map(|(_, c)| c) else {
            return Ok(None);
        };
        if self.git.resolve(&tip).is_none() {
            self.git.run(&["fetch", "-q", "--no-tags", url, &format!("+{}:{INCOMING}", self.remote_ref())])?;
        }
        if self.git.resolve(&tip).is_none() {
            return Err(format!("the remote {} tip {} could not be fetched", self.branch, crate::util::short(&tip)));
        }
        Ok(Some(tip))
    }

    /// Reads the remote branch and moves the local copy to it, refusing a remote that
    /// was rewritten (its tip does not descend from what was seen before) or deleted.
    pub fn sync(&self, url: &str) -> Result<Option<String>, String> {
        let tip = self.fetch(url)?;
        match (self.cached(), &tip) {
            (Some(seen), Some(t)) if !self.git.is_ancestor(&seen, t) => {
                return Err(format!(
                    "the remote {} branch was rewritten: its tip {} does not contain {} seen before",
                    self.branch,
                    crate::util::short(t),
                    crate::util::short(&seen)
                ))
            }
            (Some(seen), None) => {
                return Err(format!(
                    "the remote {} branch is gone, but {} was seen before",
                    self.branch,
                    crate::util::short(&seen)
                ))
            }
            _ => {}
        }
        if let Some(t) = &tip {
            if self.cached().as_deref() != Some(t.as_str()) {
                self.validate_new(self.cached().as_deref(), t)?;
            }
            self.git.run(&["update-ref", CACHE, t])?;
        }
        Ok(tip)
    }

    /// The text a valid commit must hold: its parent's ledger (or the header, for the
    /// first commit) followed by exactly one rendered entry.
    fn expected_text(parent_text: &str, e: &Entry) -> String {
        let mut t = if parent_text.is_empty() { ledger::HEADER.to_string() } else { parent_text.to_string() };
        if !t.ends_with('\n') {
            t.push('\n');
        }
        t.push_str(&ledger::render(e));
        t
    }

    /// Checks every first-parent commit after `seen` (all of them, for a branch seen for
    /// the first time) up to `tip`: each must be an exact one-entry extension of its
    /// parent's ledger, chained correctly, with nothing else in its tree. Fails closed.
    ///
    /// It reads one `git log -p` stream and checks each commit's diff (the appended
    /// lines) against a running chain state, so the cost grows with the number of
    /// entries, not with their square.
    pub fn validate_new(&self, seen: Option<&str>, tip: &str) -> Result<(), String> {
        let bad = |c: &str, why: &str| {
            format!("the {} branch has an invalid commit {}: {why}", self.branch, crate::util::short(c))
        };
        // The state after `seen`, which was validated when it was first seen.
        let (mut last_hash, mut ids, mut lines) = match seen {
            Some(s) => {
                let text = self.text_at(s);
                let entries =
                    ledger::parse_ledger(&text).map_err(|e| bad(s, &format!("its ledger does not parse: {e}")))?;
                let last = match entries.last() {
                    Some(e) => ledger::entry_hash(self.git, e)?,
                    None => ledger::GENESIS.to_string(),
                };
                let ids: std::collections::HashSet<String> = entries.into_iter().map(|e| e.id).collect();
                (last, ids, text.lines().count())
            }
            None => (ledger::GENESIS.to_string(), std::collections::HashSet::new(), 0usize),
        };
        let range = match seen {
            Some(s) => format!("{s}..{tip}"),
            None => tip.to_string(),
        };
        let out = self.git.run(&[
            "log",
            "--first-parent",
            "--reverse",
            "--root",
            "-p",
            "-U0",
            "--no-renames",
            "--no-ext-diff",
            "--no-color",
            "--format=%x1e%H %P",
            &range,
        ])?;
        let mut first = true;
        for rec in out.split('\u{1e}').filter(|r| !r.trim().is_empty()) {
            let (head, diff) = rec.split_once('\n').unwrap_or((rec, ""));
            let mut ids_in_head = head.split(' ').filter(|x| !x.is_empty());
            let c = ids_in_head.next().unwrap_or("").to_string();
            let parents: Vec<&str> = ids_in_head.collect();
            match parents.len() {
                0 if first && seen.is_none() => {}
                0 => return Err(bad(&c, "a second root commit")),
                1 => {}
                _ => return Err(bad(&c, "a merge commit")),
            }
            first = false;
            let mut added: Vec<&str> = Vec::new();
            let mut files = 0;
            for line in diff.lines() {
                if let Some(rest) = line.strip_prefix("diff --git ") {
                    files += 1;
                    if rest != format!("a/{FILE} b/{FILE}") {
                        return Err(bad(&c, &format!("it changes something other than {FILE}")));
                    }
                } else if let Some(idx) = line.strip_prefix("index ") {
                    // `index a..b 100644`, or `index a..b` when the mode is unchanged.
                    if idx.split(' ').nth(1).is_some_and(|m| m != "100644") {
                        return Err(bad(&c, &format!("{FILE} must have mode 100644")));
                    }
                } else if let Some(m) = line.strip_prefix("new file mode ") {
                    if m != "100644" || lines != 0 {
                        return Err(bad(&c, &format!("{FILE} must be created once, with mode 100644")));
                    }
                } else if line.starts_with("--- ") || line.starts_with("+++ ") {
                } else if line.starts_with("old mode")
                    || line.starts_with("new mode")
                    || line.starts_with("deleted file")
                    || line.starts_with("Binary files")
                    || line.starts_with("similarity")
                    || line.starts_with("rename ")
                    || line.starts_with("copy ")
                    || line.starts_with("dissimilarity")
                {
                    return Err(bad(&c, &format!("unexpected change to {FILE}: {line}")));
                } else if let Some(h) = line.strip_prefix("@@ -") {
                    // `@@ -N,0 +N+1,M @@`: an insertion after the last line only.
                    let old = h.split(' ').next().unwrap_or("");
                    let (start, count) = old.split_once(',').unwrap_or((old, "1"));
                    if count != "0" || start.parse::<usize>().ok() != Some(lines) {
                        return Err(bad(&c, "it changes existing lines instead of appending"));
                    }
                } else if let Some(a) = line.strip_prefix('+') {
                    added.push(a);
                } else if line.starts_with('-') || line.starts_with('\\') {
                    return Err(bad(&c, "it removes or rewrites existing lines"));
                } else if !line.is_empty() {
                    return Err(bad(&c, &format!("unexpected diff metadata: {line}")));
                }
            }
            if files != 1 || added.is_empty() {
                return Err(bad(&c, "it does not append exactly one entry"));
            }
            let mut body: Vec<&str> = added.clone();
            if lines == 0 {
                let header: Vec<&str> = ledger::HEADER.lines().collect();
                if body.len() < header.len() || body[..header.len()] != header[..] {
                    return Err(bad(&c, "the first commit does not start with the ledger header"));
                }
                body.drain(..header.len());
            }
            let text = format!("{}\n", body.join("\n"));
            let parsed = ledger::parse(&text).map_err(|e| bad(&c, &format!("its entry does not parse: {e}")))?;
            let [e] = parsed.as_slice() else {
                return Err(bad(&c, "it does not append exactly one entry"));
            };
            if ledger::render(e).lines().collect::<Vec<_>>() != body {
                return Err(bad(&c, "its appended text is not exactly the rendered entry"));
            }
            if e.prev != last_hash {
                return Err(bad(&c, "its entry does not chain to the entry before it"));
            }
            if !ids.insert(e.id.clone()) {
                return Err(bad(&c, &format!("its entry repeats an id already recorded ({})", e.id)));
            }
            last_hash = ledger::entry_hash(self.git, e)?;
            lines += added.len();
        }
        Ok(())
    }

    /// The entry already recorded with this entry's id, if any. Ids embed the run's
    /// receipt, so one run with several records (a tag and its forward merge) keeps them
    /// apart while a retried push of the same record is recognised.
    fn recorded(&self, entry: &Entry) -> Result<Option<Entry>, String> {
        Ok(self.load_local()?.into_iter().find(|e| e.id == entry.id))
    }

    /// Builds a commit that appends `entry` on top of `base` (or a root commit).
    fn commit_on(&self, base: Option<&str>, entry: &Entry) -> Result<(String, Entry), String> {
        let text = base.map(|b| self.text_at(b)).unwrap_or_default();
        let entries = ledger::parse_ledger(&text).map_err(|e| format!("the ledger branch is invalid: {e}"))?;
        ledger::check_chain(self.git, &entries).map_err(|(_, m)| format!("the ledger chain is broken: {m}"))?;
        let mut e = ledger::redacted(entry);
        e.prev = match entries.last() {
            Some(last) => ledger::entry_hash(self.git, last)?,
            None => ledger::GENESIS.to_string(),
        };
        let new_text = Self::expected_text(&text, &e);
        let blob = self.git.run_input(&["hash-object", "-w", "--stdin"], &new_text)?;
        let tree = self.git.run_input(&["mktree"], &format!("100644 blob {blob}\t{FILE}\n"))?;
        let msg = format!("Record {} {} {}", e.action, e.ref_, e.result);
        let mut args = vec!["commit-tree", tree.as_str(), "-m", msg.as_str()];
        if let Some(b) = base {
            args.extend(["-p", b]);
        }
        let commit = self.git.run(&args)?;
        Ok((commit, e))
    }

    /// Appends one entry and publishes it with a compare-and-swap push. If another
    /// recorder pushed first, re-reads the branch, re-chains and tries again.
    pub fn append(&self, fetch_url: &str, push_url: &str, entry: &Entry) -> Result<Entry, String> {
        let mut last_err = String::new();
        for _ in 0..ATTEMPTS {
            let base = self.sync(fetch_url)?;
            // An entry already on the branch (a push whose answer was lost, or a retry)
            // is a success, never a second append.
            if let Some(done) = self.recorded(entry)? {
                return Ok(done);
            }
            let (commit, written) = self.commit_on(base.as_deref(), entry)?;
            let lease = format!("--force-with-lease={}:{}", self.remote_ref(), base.clone().unwrap_or_default());
            let refspec = format!("{commit}:{}", self.remote_ref());
            match self.git.run(&["push", "-q", "--atomic", &lease, push_url, &refspec]) {
                Ok(_) => {
                    self.git.run(&["update-ref", CACHE, &commit])?;
                    return Ok(written);
                }
                Err(e) => last_err = e,
            }
        }
        Err(format!("could not publish the ledger entry after {ATTEMPTS} attempts: {last_err}"))
    }

    /// Writes `entries` as a new branch history (one commit per entry) under `target`,
    /// for migration. Returns the tip.
    pub fn build_history(&self, entries: &[Entry], target: &str) -> Result<String, String> {
        let mut base: Option<String> = None;
        let mut chained: Vec<Entry> = Vec::new();
        for e in entries {
            // The entries arrive chained for format 2.
            let mut text = if chained.is_empty() {
                ledger::HEADER.to_string()
            } else {
                self.text_at(base.as_deref().unwrap_or(""))
            };
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&ledger::render(e));
            let blob = self.git.run_input(&["hash-object", "-w", "--stdin"], &text)?;
            let tree = self.git.run_input(&["mktree"], &format!("100644 blob {blob}\t{FILE}\n"))?;
            let msg = format!("Record {} {} {} (migrated)", e.action, e.ref_, e.result);
            let mut args = vec!["commit-tree".to_string(), tree, "-m".into(), msg];
            if let Some(b) = &base {
                args.extend(["-p".to_string(), b.clone()]);
            }
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            base = Some(self.git.run(&a)?);
            chained.push(e.clone());
        }
        let tip = base.ok_or("there are no entries to migrate")?;
        self.git.run(&["update-ref", target, &tip])?;
        Ok(tip)
    }

    /// `(commit, text)` along the first-parent history of `tip`, oldest first.
    pub fn history(&self, tip: &str) -> Vec<(String, String)> {
        self.git
            .run(&["log", "--first-parent", "--reverse", "--format=%H", tip])
            .unwrap_or_default()
            .lines()
            .map(|c| (c.to_string(), self.text_at(c)))
            .collect()
    }
}
