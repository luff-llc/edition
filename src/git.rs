//! A thin wrapper around the `git` command.
//!
//! Every git (and gh) invocation runs with a cleared environment plus a small allowlist,
//! so variables such as `GIT_DIR`, `GIT_CONFIG_*`, `GIT_SSH_COMMAND` or `GH_HOST` in the
//! caller's environment cannot redirect or alter it. Repository hooks, fsmonitor and the
//! `ext::` transport are switched off on the command line. The user's global git
//! configuration (credential helpers, ssh settings) remains the trusted base.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Receives each blob from `Git::cat_blobs`: `(object id, size, a reader of exactly its
/// raw bytes)`, so a blob is never held whole.
pub type BlobSink<'a> = dyn FnMut(&str, u64, &mut dyn std::io::Read) -> Result<(), String> + 'a;

/// `(name, commit)` pairs.
pub type Refs = Vec<(String, String)>;

/// Inherited variables git and gh keep: what authentication and identity need.
pub const TOOL_VARS: &[&str] = &[
    "HOME",
    "LANG",
    "TMPDIR",
    "SSH_AUTH_SOCK",
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
];

/// Configuration forced on every git call.
pub const SAFE_CONFIG: &[&str] =
    &["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", "-c", "protocol.ext.allow=never"];

/// One setting as git reports it: `(scope, origin file if any, key, value)`.
pub type ConfigSetting = (String, Option<String>, String, String);

/// Names that may never be passed through to git or gh, even when declared.
pub fn forbidden_tool_var(name: &str) -> bool {
    name.starts_with("GIT_") || matches!(name, "GH_HOST" | "GH_REPO" | "GH_CONFIG_DIR" | "PATH")
}

/// The environment git and gh run with: `PATH`, the allowlist, the declared token
/// variables, and fixed settings. Nothing else is inherited.
pub fn tool_env(path: &str, declared: &[String]) -> Vec<(String, String)> {
    let mut env = vec![("PATH".to_string(), path.to_string())];
    for k in TOOL_VARS.iter().map(|k| k.to_string()).chain(declared.iter().filter(|k| !forbidden_tool_var(k)).cloned())
    {
        if let Ok(v) = std::env::var(&k) {
            env.push((k, v));
        }
    }
    env.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
    env.push(("LC_ALL".into(), "C".into()));
    env
}

/// The repository a gh command should target, from a remote URL:
/// `(host, owner/name)` for `https://host/owner/name(.git)`, `git@host:owner/name(.git)`
/// and `ssh://[user@]host[:port]/owner/name(.git)`.
pub fn forge_repo(url: &str) -> Option<(String, String)> {
    let (host, path) = if let Some(rest) = url.strip_prefix("https://") {
        let (h, p) = rest.split_once('/')?;
        (h.rsplit('@').next()?, p)
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let (h, p) = rest.split_once('/')?;
        (h.rsplit('@').next()?.split(':').next()?, p)
    } else if let Some((user_host, p)) = url.split_once(':') {
        if user_host.contains('/') || !user_host.contains('@') {
            return None;
        }
        (user_host.rsplit('@').next()?, p)
    } else {
        return None;
    };
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = path.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if parts.next().is_some() || !ok(owner) || !ok(name) || !ok(host) {
        return None;
    }
    Some((host.to_string(), format!("{owner}/{name}")))
}

#[derive(Clone, Debug)]
pub struct Git {
    pub dir: PathBuf,
    /// The environment every call runs with.
    pub env: Vec<(String, String)>,
}

impl Git {
    pub fn new(dir: &Path) -> Git {
        Git { dir: dir.to_path_buf(), env: tool_env(crate::steps::DEFAULT_PATH, &[]) }
    }

    pub fn with_env(dir: &Path, path: &str, declared: &[String]) -> Git {
        Git { dir: dir.to_path_buf(), env: tool_env(path, declared) }
    }

    fn command(&self, args: &[&str]) -> Command {
        self.command_with(SAFE_CONFIG, args)
    }

    /// Where git runs hooks for this repository, as the user configured it (the one
    /// query made without Edition's forced `core.hooksPath`).
    pub fn hooks_dir(&self) -> Result<String, String> {
        let out = self.command_with(&[], &["rev-parse", "--git-path", "hooks"]).output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn command_with(&self, forced: &[&str], args: &[&str]) -> Command {
        let mut c = Command::new("git");
        c.env_clear()
            .envs(self.env.iter().cloned())
            .arg("-C")
            .arg(&self.dir)
            .args(forced)
            .args(["-c", "color.ui=false", "-c", "log.showSignature=false", "-c", "core.pager=cat"])
            .args(args);
        c
    }

    /// Every effective setting with its scope and origin
    /// (`git config --list --show-scope --show-origin -z`).
    pub fn config_settings(&self) -> Result<Vec<ConfigSetting>, String> {
        let out = self.run(&["config", "--list", "--show-scope", "--show-origin", "-z"])?;
        let mut v = Vec::new();
        let fields: Vec<&str> = out.split('\0').collect();
        let mut i = 0;
        while i + 2 < fields.len() {
            let (scope, origin, kv) = (fields[i], fields[i + 1], fields[i + 2]);
            i += 3;
            if scope.is_empty() {
                break;
            }
            let (k, val) = kv.split_once('\n').unwrap_or((kv, ""));
            v.push((
                scope.to_string(),
                origin.strip_prefix("file:").map(str::to_string),
                k.to_string(),
                val.to_string(),
            ));
        }
        Ok(v)
    }

    /// `(fetch URL, push URL, configured URL)` of a remote, after `insteadOf`,
    /// `pushInsteadOf` and `pushurl`.
    pub fn remote_urls(&self, remote: &str) -> Result<(String, String, String), String> {
        let fetch = self.run(&["remote", "get-url", "--", remote])?;
        let push = self.run(&["remote", "get-url", "--push", "--", remote])?;
        let raw = self.run(&["config", "--get", &format!("remote.{remote}.url")]).unwrap_or_else(|_| fetch.clone());
        if [&fetch, &push, &raw].iter().any(|u| u.is_empty() || u.starts_with('-') || u.chars().any(char::is_control)) {
            return Err(format!("remote {remote} has an unusable URL ({})", crate::util::redact(&fetch)));
        }
        Ok((fetch, push, raw))
    }

    pub fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = self.command(args).output().map_err(|e| format!("cannot run git: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
        } else {
            Err(crate::util::redact(&format!(
                "git {} failed in {}: {}",
                args.join(" "),
                self.dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }

    /// Raw stdout of a read-only command that must never fetch a missing object.
    pub fn run_bytes(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let out =
            self.command(args).env("GIT_NO_LAZY_FETCH", "1").output().map_err(|e| format!("cannot run git: {e}"))?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(crate::util::redact(&format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }

    /// Streams the raw bytes of each blob, in order, through `each(oid, size, reader)`, with
    /// `git cat-file --batch`: no filters or conversions, and never a lazy fetch.
    pub fn cat_blobs(&self, oids: &[String], each: &mut BlobSink<'_>) -> Result<(), String> {
        use std::io::{BufRead, Read, Write};
        use std::process::Stdio;
        if oids.is_empty() {
            return Ok(());
        }
        if let Some(bad) = oids.iter().find(|o| !crate::util::valid_commit(o)) {
            return Err(format!("{bad:?} is not an object id"));
        }
        let mut child = self
            .command(&["cat-file", "--batch"])
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run git: {e}"))?;
        let mut stdin = child.stdin.take().ok_or("no stdin for git cat-file")?;
        let input: String = oids.iter().map(|o| format!("{o}\n")).collect();
        let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
        let mut out = std::io::BufReader::new(child.stdout.take().ok_or("no stdout for git cat-file")?);
        let mut result = Ok(());
        for want in oids {
            let mut header = String::new();
            if out.read_line(&mut header).map_err(|e| e.to_string())? == 0 {
                result = Err("git cat-file ended early".to_string());
                break;
            }
            let parts: Vec<&str> = header.trim_end().split(' ').collect();
            let [oid, "blob", size] = parts.as_slice() else {
                result = Err(format!("object {want} is not a blob here ({})", header.trim_end()));
                break;
            };
            let size: u64 = size.parse().map_err(|_| "unexpected git cat-file output".to_string())?;
            let mut body = (&mut out).take(size);
            let r = each(oid, size, &mut body);
            // Whatever the sink did not read is skipped, never buffered.
            std::io::copy(&mut body, &mut std::io::sink()).map_err(|e| e.to_string())?;
            if let Err(e) = r {
                result = Err(e);
                break;
            }
            let mut nl = [0u8; 1];
            out.read_exact(&mut nl).map_err(|e| e.to_string())?;
        }
        drop(out);
        let _ = writer.join();
        let _ = child.wait();
        result
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

    /// Runs git with `input` on standard input.
    pub fn run_input(&self, args: &[&str], input: &str) -> Result<String, String> {
        use std::io::Write;
        let mut child = self
            .command(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run git: {e}"))?;
        child.stdin.take().ok_or("no stdin for git")?.write_all(input.as_bytes()).map_err(|e| e.to_string())?;
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(crate::util::redact(&format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forge_urls() {
        let gh = Some(("github.com".to_string(), "o/n".to_string()));
        assert_eq!(forge_repo("https://github.com/o/n.git"), gh);
        assert_eq!(forge_repo("https://user:tok@github.com/o/n"), gh);
        assert_eq!(forge_repo("git@github.com:o/n.git"), gh);
        assert_eq!(forge_repo("ssh://git@github.com:22/o/n.git"), gh);
        assert_eq!(forge_repo("/srv/git/n.git"), None);
        assert_eq!(forge_repo("gh:o/n"), None);
        assert_eq!(forge_repo("https://github.com/o/n/extra"), None);
    }

    #[test]
    fn tool_variables() {
        assert!(forbidden_tool_var("GIT_DIR"));
        assert!(forbidden_tool_var("GH_HOST"));
        assert!(forbidden_tool_var("GH_REPO"));
        assert!(!forbidden_tool_var("GH_ENTERPRISE_TOKEN"));
        assert!(!forbidden_tool_var("GH_TOKEN"));
        assert!(!forbidden_tool_var("GNUPGHOME"));
    }
}
