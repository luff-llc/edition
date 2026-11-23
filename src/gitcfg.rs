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

/// Lists the chain and refuses an untrustworthy file. Purely local: it runs no helper
/// and contacts nothing.
pub fn audit(git: &Git) -> Result<Chain, String> {
    let uid = crate::config::current_uid()?;
    let mut chain = Chain::default();
    let mut seen: Vec<String> = Vec::new();
    let mut paths: Vec<(String, String)> = Vec::new();
    let settings = git.config_settings()?;
    for (scope, origin, _, _) in &settings {
        if let Some(p) = origin {
            // Git names the repository's own file relative to it (`.git/config`).
            let abs = if p.starts_with('/') { std::path::PathBuf::from(p) } else { git.dir.join(p) };
            let abs = abs.canonicalize().unwrap_or(abs).display().to_string();
            if !seen.contains(&abs) {
                seen.push(abs.clone());
                paths.push((abs, scope.clone()));
            }
        }
    }
    // Files that exist but hold nothing yet are still in the chain.
    let home = std::env::var("HOME").unwrap_or_default();
    let xdg = std::env::var("XDG_CONFIG_HOME").ok().filter(|x| x.starts_with('/')).unwrap_or(format!("{home}/.config"));
    let local = git.run(&["rev-parse", "--path-format=absolute", "--git-path", "config"]).unwrap_or_default();
    for (p, scope) in
        [(format!("{home}/.gitconfig"), "global"), (format!("{xdg}/git/config"), "global"), (local, "local")]
    {
        let p = std::path::Path::new(&p).canonicalize().map(|c| c.display().to_string()).unwrap_or(p);
        if !p.is_empty() && std::path::Path::new(&p).is_file() && !seen.contains(&p) {
            seen.push(p.clone());
            paths.push((p, scope.to_string()));
        }
    }
    for (path, scope) in paths {
        let (m, id) = identity(&path)?;
        if m.uid() != 0 && m.uid() != uid {
            return Err(format!(
                "git configuration {path} ({scope}) is owned by another user; refusing to let git contact a remote"
            ));
        }
        if m.permissions().mode() & 0o022 != 0 {
            return Err(format!(
                "git configuration {path} ({scope}) is writable by its group or others; refusing to let git contact a remote"
            ));
        }
        chain.files.push(ConfigFile { path, scope, identity: id });
    }
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
}
