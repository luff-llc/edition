//! The content check: internal coordination text (notes addressed to a person, local
//! home-directory paths, private-key headers) should not reach a public remote. It
//! reports; it never rewrites history.

use crate::config::{glob_match, Loaded};
use crate::git::Git;
use crate::verify::Report;
use std::io::{BufRead, IsTerminal};

/// A deny pattern: literal text where `<word>` matches one run of `[A-Za-z0-9._-]`.
#[derive(Clone, Debug)]
pub struct Pattern {
    source: String,
    parts: Vec<Option<String>>,
}

impl Pattern {
    pub fn parse(s: &str) -> Pattern {
        let mut parts = Vec::new();
        let mut rest = s;
        while let Some(i) = rest.find('<') {
            match rest[i..].find('>') {
                Some(j) if j > 1 && rest[i + 1..i + j].chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                    if i > 0 {
                        parts.push(Some(rest[..i].to_string()));
                    }
                    parts.push(None);
                    rest = &rest[i + j + 1..];
                }
                _ => {
                    parts.push(Some(rest[..=i].to_string()));
                    rest = &rest[i + 1..];
                }
            }
        }
        if !rest.is_empty() {
            parts.push(Some(rest.to_string()));
        }
        Pattern { source: s.to_string(), parts }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    fn word(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
    }

    fn match_at(&self, s: &str) -> bool {
        let mut pos = 0;
        for p in &self.parts {
            match p {
                Some(lit) => {
                    if !s[pos..].starts_with(lit.as_str()) {
                        return false;
                    }
                    pos += lit.len();
                }
                None => {
                    let n: usize = s[pos..].chars().take_while(|c| Self::word(*c)).map(char::len_utf8).sum();
                    if n == 0 {
                        return false;
                    }
                    pos += n;
                }
            }
        }
        true
    }

    /// A pattern that starts with `/` is a path: it must start a token (not continue a
    /// URL or a relative path such as `data/home/...`).
    pub fn is_match(&self, line: &str) -> bool {
        let path_like = matches!(self.parts.first(), Some(Some(l)) if l.starts_with('/'));
        let mut prev: Option<char> = None;
        for (i, c) in line.char_indices() {
            let boundary =
                !path_like || prev.is_none_or(|p| !(Self::word(p) || matches!(p, '/' | ':' | '}' | '$' | '~')));
            if boundary && self.match_at(&line[i..]) {
                return true;
            }
            prev = Some(c);
        }
        false
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub place: String,
    pub pattern: String,
    pub excerpt: String,
    /// A short stable id, for `--allow-finding`.
    pub id: String,
}

/// Gives each finding its id and drops exact repeats.
pub fn identify(git: &Git, found: Vec<Finding>) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    for mut f in found {
        if out.iter().any(|o| o.place == f.place && o.pattern == f.pattern && o.excerpt == f.excerpt) {
            continue;
        }
        let _ = git;
        let h = crate::util::sha256_hex(format!("{}\0{}\0{}", f.place, f.pattern, f.excerpt).as_bytes());
        f.id = h.chars().take(10).collect();
        out.push(f);
    }
    out
}

pub struct Scanner {
    patterns: Vec<Pattern>,
    allow: Vec<String>,
}

impl Scanner {
    pub fn new(l: &Loaded) -> Scanner {
        Scanner {
            patterns: l.cfg.content.deny.iter().map(|p| Pattern::parse(p)).collect(),
            allow: l.cfg.content.allow_paths.clone(),
        }
    }

    pub fn allowed(&self, path: &str) -> bool {
        self.allow.iter().any(|g| glob_match(g, path))
    }

    pub fn scan_line(&self, place: &str, line: &str, out: &mut Vec<Finding>) {
        for p in &self.patterns {
            if p.is_match(line) {
                let excerpt: String = line.trim().chars().take(100).collect();
                out.push(Finding {
                    place: place.to_string(),
                    pattern: p.source().to_string(),
                    excerpt,
                    id: String::new(),
                });
            }
        }
    }

    /// A finding for content that could not be read as text. It blocks like any other
    /// finding unless its path is exempt or it is allowed by id.
    fn unscanned(&self, place: String, why: &str, out: &mut Vec<Finding>) {
        out.push(Finding { place, pattern: UNSCANNED.into(), excerpt: why.into(), id: String::new() });
    }

    /// Commits in `range` (git log arguments): their messages and every line they add.
    /// Renames are shown as a deletion and an addition, so a file moved out of an allowed
    /// path is scanned in full. Merges are shown as combined diffs, and a line counts as
    /// added by the merge only when it is new against every parent (merge-only changes).
    /// Binary files are reported as unscanned. Any error fails the scan.
    pub fn scan_commits(&self, git: &Git, range: &[String]) -> Result<Vec<Finding>, String> {
        let mut args = vec![
            "log",
            "--no-show-signature",
            "-p",
            "--cc",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--format=%x1e%H%x1f%B%x1f",
        ];
        args.extend(range.iter().map(String::as_str));
        args.push("--");
        let out = git.run(&args).map_err(|e| format!("content scan failed: {e}"))?;
        let mut found = Vec::new();
        for rec in out.split('\u{1e}') {
            let mut f = rec.splitn(3, '\u{1f}');
            let (Some(hash), Some(body), diff) = (f.next(), f.next(), f.next()) else { continue };
            let hash = crate::util::short(hash.trim()).to_string();
            for line in body.lines() {
                self.scan_line(&format!("commit {hash} message"), line, &mut found);
            }
            let mut path = String::new();
            let mut parents = 1usize;
            for line in diff.unwrap_or("").lines() {
                if let Some(rest) = line.strip_prefix("diff --git ") {
                    path = rest.rsplit(" b/").next().unwrap_or(rest).to_string();
                } else if let Some(rest) =
                    line.strip_prefix("diff --cc ").or_else(|| line.strip_prefix("diff --combined "))
                {
                    path = rest.to_string();
                } else if let Some(p) = line.strip_prefix("+++ ") {
                    if p != "/dev/null" {
                        path = p.strip_prefix("b/").unwrap_or(p).to_string();
                    }
                } else if line.starts_with("Binary files ") && line.ends_with(" differ") {
                    if !line.ends_with("and /dev/null differ") && !self.allowed(&path) {
                        self.unscanned(format!("commit {hash} {path}"), "binary file", &mut found);
                    }
                } else if line.starts_with("@@") {
                    parents = line.chars().take_while(|c| *c == '@').count().saturating_sub(1).max(1);
                } else if line.len() >= parents
                    && line.is_char_boundary(parents)
                    && line[..parents].chars().all(|c| c == '+')
                    && !line.starts_with("+++ ")
                    && !self.allowed(&path)
                {
                    self.scan_line(&format!("commit {hash} {path}"), &line[parents..], &mut found);
                }
            }
        }
        Ok(identify(git, found))
    }

    /// Exactly what a push would publish: every commit reachable from `tips` and from
    /// none of the commits the remote already has.
    pub fn scan_publish(&self, git: &Git, tips: &[String], remote_has: &[String]) -> Result<Vec<Finding>, String> {
        let mut range: Vec<String> = tips.to_vec();
        range.push("--not".into());
        range.extend(remote_has.iter().filter(|c| git.resolve(c).is_some()).cloned());
        self.scan_commits(git, &range)
    }

    /// Every tracked file in the working tree. Binary and oversize files are reported as
    /// unscanned; any error fails the scan.
    pub fn scan_tree(&self, l: &Loaded, git: &Git) -> Result<Vec<Finding>, String> {
        let mut found = Vec::new();
        let list = git.run(&["ls-files", "-z"]).map_err(|e| format!("content scan failed: {e}"))?;
        for path in list.split('\0').filter(|p| !p.is_empty()) {
            if self.allowed(path) {
                continue;
            }
            let bytes = match std::fs::read(l.root.join(path)) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("content scan failed: cannot read {path}: {e}")),
            };
            if bytes.len() > MAX_SCAN_BYTES {
                self.unscanned(path.to_string(), "larger than the scan limit", &mut found);
                continue;
            }
            if bytes.contains(&0) {
                self.unscanned(path.to_string(), "binary file", &mut found);
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            for (n, line) in text.lines().enumerate() {
                self.scan_line(&format!("{path}:{}", n + 1), line, &mut found);
            }
        }
        Ok(identify(git, found))
    }
}

/// The pattern name of findings for content that could not be scanned.
pub const UNSCANNED: &str = "unscanned";

/// Files above this size are reported as unscanned rather than read.
pub const MAX_SCAN_BYTES: usize = 4_000_000;

fn summarize(found: &[Finding], limit: usize) -> String {
    found
        .iter()
        .take(limit)
        .map(|f| format!("{} [{}] {}", f.place, f.pattern, f.excerpt))
        .collect::<Vec<_>>()
        .join("\n        ")
}

/// `verify`: the tracked tree, and commits not yet on each public remote.
pub fn verify(l: &Loaded, git: &Git, r: &mut Report) {
    let public: Vec<&String> = l.cfg.remotes.iter().filter(|(_, c)| c.public).map(|(n, _)| n).collect();
    if public.is_empty() {
        return;
    }
    let s = Scanner::new(l);
    let block = l.cfg.content.action == "block";
    let mut push = |msg: String| if block { r.errors.push(msg) } else { r.warnings.push(msg) };
    let tree = match s.scan_tree(l, git) {
        Ok(t) => t,
        Err(e) => {
            r.errors.push(e);
            return;
        }
    };
    let mut clean = tree.is_empty();
    if !tree.is_empty() {
        push(format!(
            "content: {} line(s) in tracked files match the deny-list:\n        {}",
            tree.len(),
            summarize(&tree, 10)
        ));
    }
    for remote in public {
        let range = vec!["HEAD".to_string(), "--not".to_string(), format!("--remotes={remote}")];
        let commits = match s.scan_commits(git, &range) {
            Ok(c) => c,
            Err(e) => {
                clean = false;
                push(e);
                continue;
            }
        };
        if !commits.is_empty() {
            clean = false;
            push(format!(
                "content: {} finding(s) in commits not yet on public remote {remote}:\n        {}",
                commits.len(),
                summarize(&commits, 10)
            ));
        }
    }
    if clean {
        r.ok.push("content: nothing on the deny-list in the tracked tree or unpushed commits".into());
    }
}

/// `check-push <remote>`: a git pre-push hook. Reads `<local ref> <local sha> <remote ref>
/// <remote sha>` lines from standard input (or checks HEAD when there are none).
pub fn check_push(l: &Loaded, git: &Git, remote: &str) -> Result<i32, String> {
    if !l.is_public(remote) {
        println!("edition: {remote} is not marked public; content not checked");
        if let Err(e) = crate::policy::agreed_version(l, git, Some("HEAD")) {
            eprintln!("edition: versions (reported, not blocking): {e}");
        }
        return Ok(0);
    }
    let zero = |s: &str| s.chars().all(|c| c == '0');
    let mut ranges: Vec<Vec<String>> = Vec::new();
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        for line in stdin.lock().lines() {
            let line = line.map_err(|e| e.to_string())?;
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() != 4 || zero(f[1]) {
                continue;
            }
            if zero(f[3]) || git.resolve(f[3]).is_none() {
                ranges.push(vec![f[1].to_string(), "--not".into(), format!("--remotes={remote}")]);
            } else {
                ranges.push(vec![format!("{}..{}", f[3], f[1])]);
            }
        }
    }
    if ranges.is_empty() {
        ranges.push(vec!["HEAD".into(), "--not".into(), format!("--remotes={remote}")]);
    }
    let s = Scanner::new(l);
    let mut found = Vec::new();
    for r in &ranges {
        match s.scan_commits(git, r) {
            Ok(f) => found.extend(f),
            Err(e) => {
                eprintln!("edition: {e}; push blocked (the scan fails closed)");
                return Ok(1);
            }
        }
    }
    // Version agreement and pins at what is being pushed (HEAD when run by hand).
    let mut other = Vec::new();
    if let Err(e) = crate::policy::agreed_version(l, git, Some("HEAD")) {
        other.push(format!("versions: {e}"));
    }
    for p in crate::policy::pins(l, git, Some("HEAD"), false) {
        if let Some(v) = p.prod_violation() {
            other.push(v);
        }
    }
    if found.is_empty() && other.is_empty() {
        println!("edition: content, version and pin checks passed for {remote}");
        return Ok(0);
    }
    for o in &other {
        eprintln!("edition: {o} (reported, not blocking)");
    }
    if found.is_empty() {
        return Ok(0);
    }
    eprintln!("edition: {} line(s) about to go to public remote {remote} match the deny-list:", found.len());
    for f in &found {
        eprintln!("  {} {} [{}] {}", f.id, f.place, f.pattern, f.excerpt);
    }
    if l.cfg.content.action == "block" {
        eprintln!("edition: push blocked (content.action = \"block\"); edit the commits or the deny-list");
        Ok(1)
    } else {
        eprintln!("edition: reported only (content.action = \"report\")");
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::Pattern;

    #[test]
    fn patterns() {
        let p = Pattern::parse("Decisions for <name>");
        assert!(p.is_match(&format!("## 12. Decisions {} Alex", "for")));
        assert!(!p.is_match("Decisions for "));
        let u = Pattern::parse("/Users/<name>/");
        assert!(u.is_match(&format!("see /{}/someone/projects/x", "Users")));
        assert!(!u.is_match("/Users/"));
        assert!(!Pattern::parse("/home/<name>/").is_match("let p = root.join(\"data/home/records/x\")"));
        assert!(!Pattern::parse("/home/<name>/").is_match("http://h/v1/libraries/home/records/batch"));
        assert!(Pattern::parse("/home/<name>/").is_match(&format!("cd /{}/ci/build", "home")));
        let lit = Pattern::parse("a <b c");
        assert!(lit.is_match("x a <b c y"));
    }
}
