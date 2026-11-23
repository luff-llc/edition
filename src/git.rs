//! A thin wrapper around the `git` command. User configuration that changes output
//! (colour, signatures, pagers) is switched off for every call.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `(name, commit)` pairs.
pub type Refs = Vec<(String, String)>;

#[derive(Clone, Debug)]
pub struct Git {
    pub dir: PathBuf,
}

impl Git {
    pub fn new(dir: &Path) -> Git {
        Git { dir: dir.to_path_buf() }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new("git");
        c.arg("-C")
            .arg(&self.dir)
            .args(["-c", "color.ui=false", "-c", "log.showSignature=false", "-c", "core.pager=cat"])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C");
        c
    }

    pub fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = self.command(args).output().map_err(|e| format!("cannot run git: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
        } else {
            Err(format!(
                "git {} failed in {}: {}",
                args.join(" "),
                self.dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    pub fn ok(&self, args: &[&str]) -> bool {
        self.command(args).output().map(|o| o.status.success()).unwrap_or(false)
    }

    pub fn is_repo(&self) -> bool {
        self.ok(&["rev-parse", "--git-dir"])
    }

    /// The full commit a ref names, or `None`.
    pub fn resolve(&self, r: &str) -> Option<String> {
        if r.starts_with('-') {
            return None;
        }
        let spec = format!("{r}^{{commit}}");
        self.run(&["rev-parse", "--verify", "--quiet", "--end-of-options", &spec]).ok().filter(|s| !s.is_empty())
    }

    pub fn tree(&self, commit: &str) -> Option<String> {
        self.run(&["rev-parse", "--verify", "--quiet", &format!("{commit}^{{tree}}")]).ok()
    }

    /// True when `a` is `b` or an ancestor of it.
    pub fn is_ancestor(&self, a: &str, b: &str) -> bool {
        self.ok(&["merge-base", "--is-ancestor", a, b])
    }

    /// `(short name, commit)` of every remote-tracking branch of `remote`, without `HEAD`.
    pub fn remote_branches(&self, remote: &str) -> Vec<(String, String)> {
        let prefix = format!("refs/remotes/{remote}/");
        self.run(&["for-each-ref", "--format=%(refname) %(objectname)", &prefix])
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let (r, c) = l.split_once(' ')?;
                let name = r.strip_prefix(&prefix)?;
                (name != "HEAD").then(|| (name.to_string(), c.to_string()))
            })
            .collect()
    }

    pub fn local_branch_exists(&self, name: &str) -> bool {
        self.ok(&["show-ref", "--verify", "--quiet", &format!("refs/heads/{name}")])
    }

    /// `(tag name, peeled commit)` for every local tag.
    pub fn tags(&self) -> Vec<(String, String)> {
        self.run(&["for-each-ref", "--format=%(refname:strip=2) %(objectname) %(*objectname)", "refs/tags/"])
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut it = l.split(' ');
                let name = it.next()?.to_string();
                let obj = it.next()?.to_string();
                let peeled = it.next().filter(|s| !s.is_empty()).map(str::to_string).unwrap_or(obj);
                Some((name, peeled))
            })
            .collect()
    }

    /// File contents at a revision.
    pub fn show(&self, rev: &str, path: &str) -> Option<String> {
        self.run(&["show", &format!("{rev}:{path}")]).ok()
    }

    /// Heads and peeled tags on a remote, read with `git ls-remote` (read-only).
    pub fn ls_remote(&self, remote: &str) -> Result<(Refs, Refs), String> {
        let out = self.run(&["ls-remote", "--heads", "--tags", remote])?;
        let mut heads = Vec::new();
        let mut tags: Vec<(String, String)> = Vec::new();
        for l in out.lines() {
            let Some((c, r)) = l.split_once('\t') else { continue };
            if let Some(h) = r.strip_prefix("refs/heads/") {
                heads.push((h.to_string(), c.to_string()));
            } else if let Some(t) = r.strip_prefix("refs/tags/") {
                if let Some(t) = t.strip_suffix("^{}") {
                    tags.retain(|(n, _)| n != t);
                    tags.push((t.to_string(), c.to_string()));
                } else if !tags.iter().any(|(n, _)| n == t) {
                    tags.push((t.to_string(), c.to_string()));
                }
            }
        }
        Ok((heads, tags))
    }

    /// `short-hash subject` for each commit in a range, newest first.
    pub fn log_range(&self, range: &str) -> Vec<String> {
        self.run(&["log", "--no-show-signature", "--format=%h %s", range, "--"])
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn user_name(&self) -> Option<String> {
        self.run(&["config", "user.name"]).ok().filter(|s| !s.is_empty())
    }

    /// Git's object hash of `text` (as a blob). Used for receipts and the ledger chain.
    pub fn hash(&self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let mut child = self
            .command(&["hash-object", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run git: {e}"))?;
        child
            .stdin
            .take()
            .ok_or("no stdin for git hash-object")?
            .write_all(text.as_bytes())
            .map_err(|e| e.to_string())?;
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!("git hash-object failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// True when HEAD is not on a branch.
    pub fn is_detached(&self) -> bool {
        !self.ok(&["symbolic-ref", "-q", "HEAD"])
    }

    /// Tracked paths with uncommitted changes (staged or not).
    pub fn dirty_paths(&self) -> Vec<String> {
        self.run(&["status", "--porcelain=v1", "--untracked-files=no", "-z"])
            .unwrap_or_default()
            .split('\0')
            .filter(|l| l.len() > 3)
            .map(|l| l[3..].to_string())
            .collect()
    }

    /// True when `refs/<kind>/<name>` exists locally.
    pub fn has_ref(&self, full: &str) -> bool {
        self.ok(&["show-ref", "--verify", "--quiet", full])
    }

    pub fn is_tracked(&self, path: &str) -> bool {
        self.ok(&["ls-files", "--error-unmatch", "--", path])
    }
}
