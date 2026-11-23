//! The git configuration chain: audited before git may contact a remote, and bound in
//! every outward plan.
//!
//! Git reads the system file, the global file (chosen by `HOME`), the repository's own
//! file and anything they include. Before any operation that can talk to a remote or
//! run a helper, Edition lists that chain (a purely local `git config --list`) and
//! refuses if any file in it is writable by its group or others or owned by a user
//! other than root or you. It does not try to judge which settings in such a file are
//! harmless. The identity of every file (path, owner, mode, size, modification time and
//! a SHA-256 of its contents) and the settings that change how git talks to a remote or
//! signs are part of the plan hash, so an edit between dry run and apply invalidates
//! the receipt.

use crate::git::Git;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// One file in the chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFile {
    pub path: String,
    pub scope: String,
    /// `uid mode size mtime sha256`.
    pub identity: String,
}

/// The audited chain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chain {
    pub files: Vec<ConfigFile>,
    /// `(scope, key, value)` for settings that change how git reaches a remote or signs.
    pub sensitive: Vec<(String, String, String)>,
}

/// Settings that change how git reaches a remote, authenticates, runs helpers or signs.
pub fn sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    let (section, rest) = k.split_once('.').unwrap_or((k.as_str(), ""));
    let last = rest.rsplit('.').next().unwrap_or("");
    match section {
        "url" | "credential" | "http" | "protocol" | "include" => true,
        "includeif" => last == "path",
        "gpg" => last == "program" || rest == "format",
        "tag" | "commit" | "push" => last == "gpgsign",
        "user" => rest == "signingkey",
        "core" => matches!(rest, "sshcommand" | "gitproxy" | "askpass"),
        "remote" => matches!(last, "url" | "pushurl" | "proxy" | "receivepack" | "uploadpack"),
        _ => false,
    }
}

fn identity(path: &str) -> Result<(std::fs::Metadata, String), String> {
    let m = std::fs::metadata(path).map_err(|e| format!("git configuration {path}: {e}"))?;
    let bytes = std::fs::read(path).map_err(|e| format!("git configuration {path}: {e}"))?;
    let id = format!(
        "{} {:o} {} {} {}",
        m.uid(),
        m.permissions().mode() & 0o7777,
        m.len(),
        m.mtime(),
        crate::util::sha256_hex(&bytes)
    );
    Ok((m, id))
}

/// Git's limit on nested includes; deeper chains fail closed.
const MAX_DEPTH: usize = 10;

/// `include.path` and `includeIf.*.path` values in a config file, parsed by Edition
/// itself so that a file contributing nothing is still followed.
pub fn include_targets(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_include = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            let header = line.trim_start_matches('[').split(']').next().unwrap_or("");
            let name = header.split(|c: char| c.is_whitespace() || c == '"').next().unwrap_or("").to_ascii_lowercase();
            // `[include]`, `[includeIf "..."]`, and the `[include.x]`-style forms.
            in_include = name == "include"
                || name == "includeif"
                || name.starts_with("include.")
                || name.starts_with("includeif.");
            // A key on the same line as the header: `[include] path = x`.
            let rest = line.split_once(']').map(|(_, r)| r.trim()).unwrap_or("");
            if in_include && !rest.is_empty() {
                if let Some(v) = path_value(rest) {
                    out.push(v);
                }
            }
            continue;
        }
        if in_include {
            if let Some(v) = path_value(line) {
                out.push(v);
            }
        }
    }
    out
}

/// The value of a `path = value` line, with git's quoting and comments.
fn path_value(line: &str) -> Option<String> {
    let (k, v) = line.split_once('=')?;
    if !k.trim().eq_ignore_ascii_case("path") {
        return None;
    }
    let mut out = String::new();
    let mut quoted = false;
    let mut chars = v.trim().chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(match n {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                }
            }
            '#' | ';' if !quoted => break,
            c => out.push(c),
        }
    }
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

/// Resolves an include target the way git does: `~/` from `HOME`, a relative path from
/// the including file's directory. `%(prefix)/` is not resolved and fails closed.
fn resolve_include(target: &str, including: &Path, home: &str) -> Result<PathBuf, String> {
    if let Some(rest) = target.strip_prefix("~/") {
        if home.is_empty() {
            return Err(format!("include {target} needs HOME"));
        }
        return Ok(Path::new(home).join(rest));
    }
    if target.starts_with('~') || target.starts_with("%(") {
        return Err(format!("include {target} cannot be resolved; refusing"));
    }
    let p = Path::new(target);
    Ok(if p.is_absolute() { p.to_path_buf() } else { including.parent().unwrap_or(Path::new("/")).join(p) })
}

/// For a file that does not exist yet: whether the nearest existing directory above it
/// lets someone else create it.
fn missing_is_writable_by_others(path: &Path, uid: u32) -> Option<String> {
    let mut cur = path.parent();
    while let Some(d) = cur {
        if let Ok(m) = std::fs::metadata(d) {
            let mode = m.permissions().mode();
            if (m.uid() != 0 && m.uid() != uid) || (mode & 0o022 != 0 && mode & 0o1000 == 0) {
                return Some(d.display().to_string());
            }
            return None;
        }
        cur = d.parent();
    }
    None
}

/// Lists the chain explicitly and refuses an untrustworthy file. Purely local: it runs
/// no helper and contacts nothing.
///
/// The chain is: the system file and the global and XDG files as `git var
/// GIT_CONFIG_SYSTEM` / `GIT_CONFIG_GLOBAL` report them for the environment Edition gives
/// git (they are reported whether or not the files exist); the repository's `config`,
/// and `config.worktree` when `extensions.worktreeConfig` is set; every file
/// `git config --list --show-origin` names (which includes files such as Apple git's
/// built-in configuration); and, recursively, every `include.path` and `includeIf.*.path`
/// target, whatever its condition, parsed from the files themselves.
pub fn audit(git: &Git) -> Result<Chain, String> {
    let uid = crate::config::current_uid()?;
    let home = git.env.iter().find(|(k, _)| k == "HOME").map(|(_, v)| v.clone()).unwrap_or_default();
    let mut chain = Chain::default();
    let settings = git.config_settings()?;
    let mut queue: Vec<(PathBuf, String, usize)> = Vec::new();
    let system = git
        .run(&["var", "GIT_CONFIG_SYSTEM"])
        .map_err(|e| format!("cannot find git's system configuration ({e}); refusing"))?;
    let global = git
        .run(&["var", "GIT_CONFIG_GLOBAL"])
        .map_err(|e| format!("cannot find git's global configuration ({e}); refusing"))?;
    for p in system.lines().filter(|l| !l.is_empty()) {
        queue.push((PathBuf::from(p), "system".into(), 0));
    }
    for p in global.lines().filter(|l| !l.is_empty()) {
        queue.push((PathBuf::from(p), "global".into(), 0));
    }
    let local = git.run(&["rev-parse", "--path-format=absolute", "--git-path", "config"])?;
    queue.push((PathBuf::from(local), "local".into(), 0));
    if git.run(&["config", "--bool", "extensions.worktreeConfig"]).is_ok_and(|v| v == "true") {
        let wt = git.run(&["rev-parse", "--path-format=absolute", "--git-path", "config.worktree"])?;
        queue.push((PathBuf::from(wt), "worktree".into(), 0));
    }
    for (scope, origin, _, _) in &settings {
        if let Some(p) = origin {
            let abs = if p.starts_with('/') { PathBuf::from(p) } else { git.dir.join(p) };
            queue.push((abs, scope.clone(), 0));
        }
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    while let Some((path, scope, depth)) = queue.pop() {
        if depth > MAX_DEPTH {
            return Err(format!(
                "git configuration includes nest deeper than {MAX_DEPTH} at {}; refusing",
                path.display()
            ));
        }
        let key = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key.clone());
        let shown = key.display().to_string();
        if !key.exists() {
            if let Some(dir) = missing_is_writable_by_others(&key, uid) {
                return Err(format!(
                    "git configuration {shown} ({scope}) does not exist, and {dir} lets others create it; refusing to let git contact a remote"
                ));
            }
            chain.files.push(ConfigFile { path: shown, scope, identity: "missing".into() });
            continue;
        }
        let (m, id) = identity(&shown)?;
        if m.uid() != 0 && m.uid() != uid {
            return Err(format!(
                "git configuration {shown} ({scope}) is owned by another user; refusing to let git contact a remote"
            ));
        }
        if m.permissions().mode() & 0o022 != 0 {
            return Err(format!(
                "git configuration {shown} ({scope}) is writable by its group or others; refusing to let git contact a remote"
            ));
        }
        let text = std::fs::read_to_string(&key).map_err(|e| format!("git configuration {shown}: {e}; refusing"))?;
        for t in include_targets(&text) {
            let target = resolve_include(&t, &key, &home)?;
            queue.push((target, format!("{scope} include"), depth + 1));
        }
        chain.files.push(ConfigFile { path: shown, scope, identity: id });
    }
    chain.files.sort_by(|a, b| a.path.cmp(&b.path));
    for (scope, _, key, value) in settings {
        if scope != "command" && sensitive_key(&key) {
            chain.sensitive.push((scope, key, value));
        }
    }
    Ok(chain)
}

#[cfg(test)]
mod tests {
    use super::sensitive_key;

    #[test]
    fn sensitive_keys() {
        for k in [
            "gpg.program",
            "gpg.ssh.program",
            "gpg.format",
            "tag.gpgSign",
            "commit.gpgsign",
            "user.signingKey",
            "protocol.file.allow",
            "http.proxy",
            "http.https://h/.proxy",
            "http.curloptResolve",
            "core.askPass",
            "core.gitProxy",
            "core.sshCommand",
            "credential.helper",
            "credential.https://h.helper",
            "url.x.insteadOf",
            "remote.origin.url",
            "remote.origin.pushurl",
            "remote.origin.proxy",
            "include.path",
            "includeIf.gitdir:/x/.path",
        ] {
            assert!(sensitive_key(k), "{k}");
        }
        for k in ["user.name", "color.ui", "core.editor", "remote.origin.fetch"] {
            assert!(!sensitive_key(k), "{k}");
        }
    }

    #[test]
    fn includes_are_parsed() {
        let text = "[include]\n\tpath = a.cfg ; comment\n[includeIf \"gitdir:~/w/\"]\n  PATH = \"~/b c.cfg\"\n[user]\n\tpath = not-an-include\n[include] path = d.cfg\n";
        assert_eq!(super::include_targets(text), vec!["a.cfg", "~/b c.cfg", "d.cfg"]);
    }
}
