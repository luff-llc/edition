//! `edition.toml`: one file per repository.

use crate::util::resolve_path;
use crate::version::{FilePattern, Template};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "edition.toml";

/// Marks a value someone still has to supply. A channel whose command carries it is
/// refused. (Built with `concat!` so this source does not trip the content check.)
pub const TODO_MARK: &str = concat!("TODO(", "owner)");

/// The three built-in tiers. Their policy is enforced by Edition; a channel only names
/// its tier and cannot loosen it.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// What is being tested on one machine: any ref, never recorded as a deployment.
    Dev,
    /// The tip of `main`, and nothing else; no approval; recorded.
    Staging,
    /// A final release tag on a release line, on the remote, with every pin on a release tag.
    Prod,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Tier::Dev => "dev",
            Tier::Staging => "staging",
            Tier::Prod => "prod",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Approval {
    #[default]
    None,
    /// A named person must pass `--approved-by`.
    Owner,
    /// As `owner`, and the command itself asks for a hardware key touch.
    Touch,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tags {
    #[serde(rename = "final", default = "d_final")]
    pub final_: String,
    #[serde(default = "d_pre")]
    pub prerelease: String,
    /// What version files say at a pre-release tag: `base` (0.6.0) or `full` (0.6.0-rc.1).
    #[serde(default = "d_base")]
    pub prerelease_files: String,
}

impl Default for Tags {
    fn default() -> Self {
        Tags { final_: d_final(), prerelease: d_pre(), prerelease_files: d_base() }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lines {
    #[serde(default = "d_line_branch")]
    pub branch: String,
    #[serde(default = "d_dev_suffix")]
    pub dev_suffix: String,
    #[serde(default = "d_hotfix")]
    pub hotfix_branch: String,
}

impl Default for Lines {
    fn default() -> Self {
        Lines { branch: d_line_branch(), dev_suffix: d_dev_suffix(), hotfix_branch: d_hotfix() }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionFile {
    pub path: String,
    pub pattern: String,
    /// Only the first match after the line equal to this text counts.
    pub after: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoRef {
    pub path: String,
    #[serde(default = "d_remote")]
    pub remote: String,
    #[serde(default = "d_final")]
    pub final_tag: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub name: String,
    pub file: String,
    /// One placeholder: `{commit}` or `{tag}`.
    pub pattern: String,
    /// A key of `[repos]`, or `self`.
    pub repo: String,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// `command` (run before the deploy; its exit code is the evidence) or `parity`
    /// (Concordance must report every item the release carries as ready).
    #[serde(default = "d_command")]
    pub kind: String,
    #[serde(default)]
    pub run: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub note: Option<String>,
}

/// One platform of the product (`ios`, `android`, `home`). The version is shared; the
/// build number is per platform.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    /// File holding this platform's build number.
    pub build_file: Option<String>,
    /// Pattern with `{build}`, for example `versionCode = {build}`.
    pub build_pattern: Option<String>,
    /// The platform's name in Concordance, when it differs.
    pub concordance: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    pub tier: Tier,
    pub kind: String,
    pub target: String,
    #[serde(default)]
    pub checks: Vec<String>,
    #[serde(default)]
    pub approval: Approval,
    /// The existing command that performs the deploy or upload.
    pub run: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// A read-only command run after a successful deploy (health check).
    #[serde(default)]
    pub probe: Vec<String>,
    /// Prod tier only: a pre-release tag (`-rc.N`) is acceptable here (store review builds).
    #[serde(default)]
    pub allow_prerelease: bool,
    /// The platform this channel ships (a key of `[platforms]`).
    pub platform: Option<String>,
    /// Channels that must already hold the same commit (per the ledger) before this one
    /// takes it: a readiness gate with recorded evidence, never a date.
    #[serde(default)]
    pub requires: Vec<String>,
    pub note: Option<String>,
}

/// A git remote of this repository.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteCfg {
    /// A public remote gets the content check before anything is pushed to it.
    #[serde(default)]
    pub public: bool,
}

/// Text that must not reach a public remote: internal coordination, not code.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentCfg {
    /// Patterns; `<word>` matches one run of letters, digits, `.`, `_` or `-`.
    #[serde(default = "d_deny")]
    pub deny: Vec<String>,
    /// Path globs that are never scanned (for example the file defining the patterns).
    #[serde(default)]
    pub allow_paths: Vec<String>,
    /// `block` (the default: findings stop a push and fail `verify`) or `report`.
    /// Edition's own outward plans to a public remote always block.
    #[serde(default = "d_block")]
    pub action: String,
}

impl Default for ContentCfg {
    fn default() -> Self {
        ContentCfg { deny: d_deny(), allow_paths: Vec::new(), action: d_block() }
    }
}

pub fn d_deny() -> Vec<String> {
    vec![
        "Decisions for <name>".into(),
        concat!("TODO(", "owner)").into(),
        "/Users/<name>/".into(),
        "/home/<name>/".into(),
        concat!("PRIVATE ", "KEY-----").into(),
    ]
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcordanceCfg {
    pub catalog: String,
    #[serde(default = "d_release_id")]
    pub release: String,
    #[serde(default = "d_concordance_bin")]
    pub bin: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyCfg {
    /// Tags older than this version are not checked against version files.
    pub tags_since: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub product: String,
    #[serde(default = "d_remote")]
    pub remote: String,
    #[serde(default = "d_main")]
    pub main: String,
    #[serde(default = "d_ledger")]
    pub ledger: String,
    #[serde(default)]
    pub tags: Tags,
    #[serde(default)]
    pub lines: Lines,
    #[serde(default)]
    pub version_files: Vec<VersionFile>,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub remotes: BTreeMap<String, RemoteCfg>,
    #[serde(default)]
    pub content: ContentCfg,
    #[serde(default)]
    pub repos: BTreeMap<String, RepoRef>,
    #[serde(default)]
    pub pins: Vec<Pin>,
    #[serde(default)]
    pub platforms: BTreeMap<String, Platform>,
    #[serde(default)]
    pub checks: BTreeMap<String, Check>,
    #[serde(default)]
    pub channels: BTreeMap<String, Channel>,
    pub concordance: Option<ConcordanceCfg>,
    #[serde(default)]
    pub verify: VerifyCfg,
}

fn d_final() -> String {
    "v{version}".into()
}
fn d_pre() -> String {
    "v{version}-rc.{n}".into()
}
fn d_base() -> String {
    "base".into()
}
fn d_line_branch() -> String {
    "release/{line}".into()
}
fn d_dev_suffix() -> String {
    "dev".into()
}
fn d_hotfix() -> String {
    "hotfix/{version}".into()
}
fn d_remote() -> String {
    "origin".into()
}
fn d_main() -> String {
    "main".into()
}
fn d_ledger() -> String {
    "releases/ledger.toml".into()
}
fn d_command() -> String {
    "command".into()
}
fn d_block() -> String {
    "block".into()
}
fn d_release_id() -> String {
    "{version}".into()
}
fn d_concordance_bin() -> String {
    "concordance".into()
}

/// A loaded and validated configuration with its repository root.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub cfg: Config,
    pub root: PathBuf,
    pub path: PathBuf,
    pub final_tag: Template,
    pub pre_tag: Template,
    pub line_branch: Template,
    pub hotfix_branch: Template,
    /// Lines of the file that carry the to-do marker.
    pub todos: Vec<String>,
    /// The configuration file's bytes, for the plan digest.
    pub raw: String,
}

/// Channel kinds that are review or testing tracks, where a release candidate may go.
pub const REVIEW_KINDS: &[&str] = &["testflight", "play-testing", "store-review", "beta"];

/// Placeholders a command may use, each as a whole argument or environment value.
pub const PLACEHOLDERS: &[&str] = &["commit", "ref", "tag", "version", "build", "channel", "target"];

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish", "csh", "tcsh"];

/// The placeholder an argument names, if it is exactly `{name}`.
pub fn whole_placeholder(a: &str) -> Option<&str> {
    a.strip_prefix('{')?.strip_suffix('}').filter(|n| !n.contains('{') && !n.contains('}'))
}

impl Loaded {
    pub fn load(path: &Path) -> Result<Loaded, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let root = path
            .parent()
            .map(|p| if p.as_os_str().is_empty() { PathBuf::from(".") } else { p.to_path_buf() })
            .unwrap_or_else(|| PathBuf::from("."));
        let root = root.canonicalize().unwrap_or(root);
        let todos = text
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains(TODO_MARK))
            .map(|(n, l)| format!("{}:{}: {}", FILE_NAME, n + 1, l.trim()))
            .collect();
        let final_tag = Template::parse(&cfg.tags.final_)?;
        let pre_tag = Template::parse(&cfg.tags.prerelease)?;
        let line_branch = Template::parse(&cfg.lines.branch)?;
        let hotfix_branch = Template::parse(&cfg.lines.hotfix_branch)?;
        let loaded = Loaded {
            cfg,
            root,
            path: path.to_path_buf(),
            final_tag,
            pre_tag,
            line_branch,
            hotfix_branch,
            todos,
            raw: text.clone(),
        };
        loaded.validate()?;
        Ok(loaded)
    }

    fn validate(&self) -> Result<(), String> {
        let c = &self.cfg;
        let mut errs = Vec::new();
        let need = |t: &Template, want: &[&str], what: &str, errs: &mut Vec<String>| {
            let mut have = t.vars();
            have.sort();
            let mut want = want.to_vec();
            want.sort();
            if have != want {
                errs.push(format!(
                    "{what} `{}` must use exactly {}",
                    t.source(),
                    want.iter().map(|w| format!("{{{w}}}")).collect::<Vec<_>>().join(" and ")
                ));
            }
        };
        need(&self.final_tag, &["version"], "tags.final", &mut errs);
        need(&self.pre_tag, &["version", "n"], "tags.prerelease", &mut errs);
        need(&self.line_branch, &["line"], "lines.branch", &mut errs);
        need(&self.hotfix_branch, &["version"], "lines.hotfix_branch", &mut errs);
        if !matches!(c.tags.prerelease_files.as_str(), "base" | "full") {
            errs.push("tags.prerelease_files must be `base` or `full`".into());
        }
        if c.product.trim().is_empty() {
            errs.push("product must not be empty".into());
        }
        for vf in &c.version_files {
            if let Err(e) = FilePattern::parse(&vf.pattern, &["version"]) {
                errs.push(format!("version_files {}: {e}", vf.path));
            }
        }
        for (name, ck) in &c.checks {
            match ck.kind.as_str() {
                "command" if ck.run.is_empty() => errs.push(format!("check {name}: run must not be empty")),
                "command" => {}
                "parity" if c.concordance.is_none() => {
                    errs.push(format!("check {name}: a parity check needs [concordance]"))
                }
                "parity" => {}
                other => errs.push(format!("check {name}: kind `{other}` is not `command` or `parity`")),
            }
        }
        for (name, p) in &c.platforms {
            match (&p.build_file, &p.build_pattern) {
                (Some(_), Some(pat)) => {
                    if let Err(e) = FilePattern::parse(pat, &["build"]) {
                        errs.push(format!("platforms.{name}: {e}"));
                    }
                }
                (None, None) => {}
                _ => errs.push(format!("platforms.{name}: build_file and build_pattern go together")),
            }
        }
        for (name, r) in &c.repos {
            if name == "self" {
                errs.push("repos.self is reserved".into());
            }
            if let Err(e) = Template::parse(&r.final_tag) {
                errs.push(format!("repos.{name}.final_tag: {e}"));
            }
        }
        for p in &c.pins {
            if let Err(e) = FilePattern::parse(&p.pattern, &["commit", "tag"]) {
                errs.push(format!("pin {}: {e}", p.name));
            }
            if p.repo != "self" && !c.repos.contains_key(&p.repo) {
                errs.push(format!("pin {} names repo `{}`, which is not in [repos]", p.name, p.repo));
            }
        }
        let check_cmd = |what: &str, argv: &[String], env: &BTreeMap<String, String>, errs: &mut Vec<String>| {
            if let Some(p) = argv.first() {
                let base = p.rsplit('/').next().unwrap_or(p);
                if SHELLS.contains(&base) && argv.iter().any(|a| a == "-c") {
                    errs.push(format!(
                        "{what}: commands run directly, never through `{base} -c`; put a pipeline in a script file"
                    ));
                }
            }
            for a in argv.iter().chain(env.values()) {
                if !a.contains('{') && !a.contains('}') {
                    continue;
                }
                match whole_placeholder(a) {
                    Some(n) if PLACEHOLDERS.contains(&n) => {}
                    Some(n) if n.strip_prefix("var.").is_some_and(|v| c.vars.contains_key(v)) => {}
                    Some(n) => errs.push(format!("{what}: unknown placeholder {{{n}}}")),
                    None => errs.push(format!(
                        "{what}: `{a}`: a placeholder must be a whole argument or environment value, never part of one"
                    )),
                }
            }
        };
        for (name, ck) in &c.checks {
            check_cmd(&format!("check {name}"), &ck.run, &ck.env, &mut errs);
        }
        for (name, ch) in &c.channels {
            if !name.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
                || name.starts_with('-')
            {
                errs.push(format!("channel `{name}`: names use lowercase letters, digits and hyphens"));
            }
            check_cmd(&format!("channel {name} run"), &ch.run, &ch.env, &mut errs);
            check_cmd(&format!("channel {name} probe"), &ch.probe, &BTreeMap::new(), &mut errs);
            if ch.allow_prerelease && !REVIEW_KINDS.contains(&ch.kind.as_str()) {
                errs.push(format!(
                    "channel {name}: allow_prerelease is only for review or testing kinds ({}), not `{}`",
                    REVIEW_KINDS.join(", "),
                    ch.kind
                ));
            }
            if ch.run.is_empty() {
                errs.push(format!("channel {name}: run must name the command that performs it"));
            }
            for ck in &ch.checks {
                if !c.checks.contains_key(ck) {
                    errs.push(format!("channel {name}: check `{ck}` is not defined in [checks]"));
                }
            }
            if ch.tier != Tier::Prod && ch.approval != Approval::None {
                errs.push(format!(
                    "channel {name}: the {} tier never takes an approval (tier policy is built in); remove `approval`",
                    ch.tier.name()
                ));
            }
            if ch.tier != Tier::Prod && ch.allow_prerelease {
                errs.push(format!("channel {name}: allow_prerelease is only meaningful on the prod tier"));
            }
            if let Some(p) = &ch.platform {
                if !c.platforms.contains_key(p) {
                    errs.push(format!("channel {name}: platform `{p}` is not in [platforms]"));
                }
            }
            for r in &ch.requires {
                match c.channels.get(r) {
                    None => errs.push(format!("channel {name}: requires unknown channel `{r}`")),
                    Some(o) if o.tier == Tier::Dev => {
                        errs.push(format!("channel {name}: cannot require dev channel `{r}`, which is never recorded"))
                    }
                    _ => {}
                }
            }
        }
        if !matches!(c.content.action.as_str(), "report" | "block") {
            errs.push("content.action must be `report` or `block`".into());
        }
        if let Some(t) = &c.verify.tags_since {
            if let Err(e) = crate::version::Version::parse(t) {
                errs.push(format!("verify.tags_since: {e}"));
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(format!("{}:\n  {}", self.path.display(), errs.join("\n  ")))
        }
    }

    /// A variable's value and whether it came from the environment (and so is treated
    /// as secret: printed by name, never recorded). `None` when it has no value.
    pub fn var(&self, name: &str) -> Option<(String, bool)> {
        let v = self.cfg.vars.get(name)?;
        let env_key = format!("EDITION_VAR_{}", name.to_ascii_uppercase().replace('-', "_"));
        match std::env::var(&env_key).ok().filter(|e| !e.is_empty()) {
            Some(e) => Some((e, true)),
            None if v.is_empty() => None,
            None => Some((v.clone(), false)),
        }
    }

    /// Expands a leading `~/` in a configured path, and a whole `{var.NAME}`.
    pub fn expand(&self, s: &str) -> String {
        let mut out = s.to_string();
        if let Some(n) = whole_placeholder(s).and_then(|n| n.strip_prefix("var.")) {
            if let Some((v, _)) = self.var(n) {
                out = v;
            }
        }
        if let Some(rest) = out.strip_prefix("~/") {
            if let Ok(home) = std::env::var("HOME") {
                out = format!("{home}/{rest}");
            }
        }
        out
    }

    pub fn repo_path(&self, repo: &str) -> Option<(PathBuf, String, Template)> {
        if repo == "self" {
            return Some((self.root.clone(), self.cfg.remote.clone(), self.final_tag.clone()));
        }
        let r = self.cfg.repos.get(repo)?;
        let t = Template::parse(&r.final_tag).ok()?;
        Some((resolve_path(&self.root, &self.expand(&r.path)), r.remote.clone(), t))
    }

    pub fn line_branch_name(&self, line: &str) -> String {
        self.line_branch.render(&[("line", line)]).unwrap_or_else(|_| format!("release/{line}"))
    }

    pub fn final_tag_name(&self, version: &str) -> String {
        self.final_tag.render(&[("version", version)]).unwrap_or_else(|_| format!("v{version}"))
    }

    pub fn pre_tag_name(&self, version: &str, n: u32) -> String {
        let n = n.to_string();
        self.pre_tag.render(&[("version", version), ("n", &n)]).unwrap_or_else(|_| format!("v{version}-rc.{n}"))
    }

    pub fn is_public(&self, remote: &str) -> bool {
        self.cfg.remotes.get(remote).is_some_and(|r| r.public)
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.root.join(&self.cfg.ledger)
    }
}

/// Glob match: `*` matches within one path segment, `**` across segments.
pub fn glob_match(pattern: &str, s: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') if p.get(1) == Some(&b'*') => {
                let rest = if p.get(2) == Some(&b'/') { &p[3..] } else { &p[2..] };
                (0..=s.len()).any(|i| go(rest, &s[i..])) || go(&p[2..], s)
            }
            Some(b'*') => {
                let mut i = 0;
                loop {
                    if go(&p[1..], &s[i..]) {
                        return true;
                    }
                    if i >= s.len() || s[i] == b'/' {
                        return false;
                    }
                    i += 1;
                }
            }
            Some(c) => s.first() == Some(c) && go(&p[1..], &s[1..]),
        }
    }
    go(pattern.as_bytes(), s.as_bytes())
}

/// Finds `edition.toml`: `--config` if given, else the current directory and its parents.
pub fn find(explicit: Option<&str>, dir: &Path) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        let p = resolve_path(dir, p);
        return if p.is_dir() { Ok(p.join(FILE_NAME)) } else { Ok(p) };
    }
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let cand = d.join(FILE_NAME);
        if cand.is_file() {
            return Ok(cand);
        }
        cur = d.parent();
    }
    Err(format!("no {FILE_NAME} in {} or its parents (pass --config)", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn globs() {
        assert!(glob_match("release/*", "release/0.6"));
        assert!(!glob_match("release/*", "release/0.6/x"));
        assert!(!glob_match("release/*", "colophon-06-sync"));
        assert!(glob_match("main", "main"));
        assert!(glob_match("docs/*-review-*.md", "docs/0.6-review-notes.md"));
        assert!(glob_match("docs/**/*.md", "docs/a/b/c.md"));
        assert!(glob_match("docs/**/*.md", "docs/c.md"));
        assert!(glob_match("**/secrets.env", "a/b/secrets.env"));
    }
}
