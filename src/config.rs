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
    /// The probe's environment. When absent the probe gets nothing beyond the base
    /// variables. A table gives it exactly those variables; `"channel"` gives it the
    /// channel's `env` (so it can read a private registry with the deploy credential).
    pub probe_env: Option<ProbeEnv>,
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

/// Limits on the tree a deploy's checks run on (`[check_tree]`). A tree over any of them
/// refuses the plan before anything is written.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckTreeCfg {
    /// Files, links and submodules in the tree (default 100 000).
    pub max_entries: Option<u64>,
    /// The largest single file, in bytes (default 256 MiB).
    pub max_file_bytes: Option<u64>,
    /// All files together, in bytes (default 2 GiB).
    pub max_total_bytes: Option<u64>,
    /// The tree's listing (paths and their metadata), in bytes (default 32 MiB).
    pub max_listing_bytes: Option<u64>,
}

impl CheckTreeCfg {
    pub fn limits(&self) -> crate::checktree::Limits {
        let d = crate::checktree::Limits::default();
        crate::checktree::Limits {
            max_entries: self.max_entries.unwrap_or(d.max_entries),
            max_file_bytes: self.max_file_bytes.unwrap_or(d.max_file_bytes),
            max_total_bytes: self.max_total_bytes.unwrap_or(d.max_total_bytes),
            max_listing_bytes: self.max_listing_bytes.unwrap_or(d.max_listing_bytes),
        }
    }
}

/// Variables that change where git finds a repository. A check may not declare them: it
/// runs in a tree with no repository, and git must not find another one.
pub const GIT_DISCOVERY_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_NAMESPACE",
    "GIT_CONFIG",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_COUNT",
];

/// A probe's declared environment: its own table, or `"channel"` for the channel's `env`.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ProbeEnv {
    Same(String),
    Own(BTreeMap<String, String>),
}

/// The only string `probe_env` accepts.
pub const PROBE_ENV_CHANNEL: &str = "channel";

/// Channel kinds that publish a library package to a registry (Maven, npm, crates.io).
/// A library has no build number of its own, so a deploy to one of these records its
/// version as the build unless `--build` or a platform `build_file` says otherwise.
pub const REGISTRY_KINDS: &[&str] = &["registry"];

impl Channel {
    /// The environment the probe declares: none unless `probe_env` says otherwise. A
    /// probe never inherits the deploy's credentials implicitly.
    pub fn probe_env(&self) -> std::borrow::Cow<'_, BTreeMap<String, String>> {
        match &self.probe_env {
            None => std::borrow::Cow::Owned(BTreeMap::new()),
            Some(ProbeEnv::Own(m)) => std::borrow::Cow::Borrowed(m),
            Some(ProbeEnv::Same(_)) => std::borrow::Cow::Borrowed(&self.env),
        }
    }

    /// True when the probe is configured to get the channel's `env`.
    pub fn probe_shares_env(&self) -> bool {
        matches!(&self.probe_env, Some(ProbeEnv::Same(_)))
    }

    pub fn is_registry(&self) -> bool {
        REGISTRY_KINDS.contains(&self.kind.as_str())
    }
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

/// Per-machine settings in `.edition/local.toml` (git-ignored, never committed).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalCfg {
    #[serde(default)]
    pub run: LocalRun,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalRun {
    /// Directories put before `[run] path` on this machine only.
    #[serde(default)]
    pub path_prepend: Vec<String>,
    /// Directories put after `[run] path` on this machine only.
    #[serde(default)]
    pub path_append: Vec<String>,
    /// Group-writable directories this machine trusts (same rule as the committed list).
    #[serde(default)]
    pub trust_group_writable: Vec<String>,
}

pub const LOCAL_FILE: &str = ".edition/local.toml";

/// Reads `.edition/local.toml`, if present. The file and its directory must be owned by
/// the current user, not be links, and not be writable by group or others.
pub fn load_local(root: &Path) -> Result<Option<LocalCfg>, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let path = root.join(LOCAL_FILE);
    let Ok(meta) = std::fs::symlink_metadata(&path) else { return Ok(None) };
    let uid = current_uid()?;
    for (p, m) in [
        (
            path.parent().unwrap_or(root).to_path_buf(),
            std::fs::symlink_metadata(path.parent().unwrap_or(root)).map_err(|e| e.to_string())?,
        ),
        (path.clone(), meta),
    ] {
        if m.file_type().is_symlink() || m.uid() != uid || m.permissions().mode() & 0o022 != 0 {
            return Err(format!(
                "{} must be owned by you, not a link, and not writable by group or others",
                p.display()
            ));
        }
    }
    // It holds this machine's settings only: refuse it if git would commit it.
    let git = crate::git::Git::new(root);
    if git.is_repo() {
        if git.is_tracked(LOCAL_FILE) {
            return Err(format!(
                "{LOCAL_FILE} is tracked by git; it is per-machine and must never be committed (git rm --cached it)"
            ));
        }
        if !git.ok(&["check-ignore", "-q", "--", LOCAL_FILE]) {
            return Err(format!(
                "{LOCAL_FILE} is not ignored by git; add `.edition/` to .gitignore (or let Edition create .edition/.gitignore)"
            ));
        }
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let cfg: LocalCfg = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    for d in &cfg.run.trust_group_writable {
        if !d.starts_with('/') || d.contains(['*', '?', '[']) {
            return Err(format!("{}: trust_group_writable {d:?} must be an exact canonical path", path.display()));
        }
    }
    Ok(Some(cfg))
}

/// How configured commands run.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCfg {
    /// `PATH` for every command Edition runs, including git and gh (default: a fixed
    /// system path).
    pub path: Option<String>,
    /// Extra variables git and gh may inherit, such as `GH_TOKEN`. Never `GIT_*`,
    /// `GH_HOST`, `GH_REPO` or enterprise tokens.
    #[serde(default)]
    pub tool_env: Vec<String>,
    /// Group-writable directories to trust in `path` (exact canonical paths). Any member
    /// of the group can replace the programs in them.
    #[serde(default)]
    pub trust_group_writable: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptCfg {
    /// Receipt lifetime in minutes (default 30, at most 60).
    pub ttl_minutes: Option<u64>,
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
    /// Where a pre-0.2 in-tree ledger lived; read only by `edition ledger migrate`.
    #[serde(default = "d_ledger")]
    pub ledger: String,
    /// The orphan branch that holds the ledger.
    #[serde(default = "d_ledger_branch")]
    pub ledger_branch: String,
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
    #[serde(default)]
    pub run: RunCfg,
    #[serde(default)]
    pub receipts: ReceiptCfg,
    #[serde(default)]
    pub check_tree: CheckTreeCfg,
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
fn d_ledger_branch() -> String {
    "edition/ledger".into()
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
    /// The checked `PATH` for every command Edition runs.
    pub run_path: String,
    /// Group-writable directories trusted by `[run] trust_group_writable` and in use.
    pub trusted_dirs: Vec<String>,
    /// Default directories skipped for being group-writable.
    pub skipped_dirs: Vec<String>,
    /// `PATH` entries added by `.edition/local.toml` on this machine.
    pub local_dirs: Vec<String>,
}

/// The effective user id, without libc: the owner of a file this process just created.
pub fn current_uid() -> Result<u32, String> {
    use std::os::unix::fs::MetadataExt;
    static UID: std::sync::OnceLock<Result<u32, String>> = std::sync::OnceLock::new();
    UID.get_or_init(|| {
        let nonce = crate::util::random_hex(8)?;
        let p = std::env::temp_dir().join(format!(".edition-uid-{nonce}"));
        let f = std::fs::OpenOptions::new().write(true).create_new(true).open(&p).map_err(|e| e.to_string())?;
        let uid = f.metadata().map(|m| m.uid()).map_err(|e| e.to_string());
        let _ = std::fs::remove_file(&p);
        uid
    })
    .clone()
}

/// Why a directory failed the `run.path` rule.
#[derive(Debug)]
enum Untrusted {
    /// Group-writable, owned by root or the user, and not listed in `trust_group_writable`.
    Group(String),
    Other(String),
}

/// The `run.path` trust rule, for an entry and each of its ancestors up to `/`:
/// - owned by root or the current user, always;
/// - never writable by others, except an ancestor with the sticky bit (as `/tmp` is);
/// - not group-writable unless its exact canonical path is in `trust_group_writable`.
fn trusted_dir(
    path: &std::path::Path,
    meta: &std::fs::Metadata,
    entry: bool,
    trusted: &[String],
    used: &mut Vec<String>,
) -> Result<(), Untrusted> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let uid = current_uid().map_err(|e| Untrusted::Other(format!("cannot tell the current user: {e}")))?;
    let what = if entry { "run.path entry" } else { "run.path ancestor" };
    if meta.uid() != 0 && meta.uid() != uid {
        return Err(Untrusted::Other(format!(
            "{what} {} is owned by another user; refusing to run commands from it",
            path.display()
        )));
    }
    let mode = meta.permissions().mode();
    let sticky = mode & 0o1000 != 0;
    if mode & 0o002 != 0 && (entry || !sticky) {
        return Err(Untrusted::Other(format!(
            "{what} {} is writable by others; refusing to run commands from it",
            path.display()
        )));
    }
    if mode & 0o020 != 0 && (entry || !sticky) {
        let p = path.display().to_string();
        if trusted.contains(&p) {
            if !used.contains(&p) {
                used.push(p);
            }
        } else {
            return Err(Untrusted::Group(format!(
                "{what} {p} is writable by its group; list it in [run] trust_group_writable to trust it"
            )));
        }
    }
    Ok(())
}

/// The checked `PATH`, the group-writable directories it trusts, and the default
/// directories it skipped for being group-writable.
#[derive(Clone, Debug, Default)]
pub struct RunPath {
    pub path: String,
    pub trusted: Vec<String>,
    pub skipped_group: Vec<String>,
}

/// Resolves `[run] path` (or the default) into the `PATH` every command runs with.
/// A leading `~/` needs `HOME` to be absolute and free of `:`. Every entry must end up
/// absolute, without `:`, canonical, and not writable by others; entries that do not
/// exist are dropped. Planning runs git before any receipt exists, so this is checked
/// before anything runs.
pub fn checked_run_path(configured: Option<&str>, trust: &[String]) -> Result<RunPath, String> {
    let p = configured.unwrap_or(crate::steps::DEFAULT_PATH);
    let mut out = RunPath::default();
    let mut dirs: Vec<String> = Vec::new();
    for d in p.split(':').filter(|d| !d.is_empty()) {
        let expanded = match d.strip_prefix("~/") {
            Some(rest) => {
                let home = std::env::var("HOME").map_err(|_| "run.path uses ~/ but HOME is not set".to_string())?;
                if !home.starts_with('/') || home.contains(':') || home.chars().any(char::is_control) {
                    return Err(format!("run.path uses ~/ but HOME ({home:?}) is not an absolute path without `:`"));
                }
                format!("{}/{rest}", home.trim_end_matches('/'))
            }
            None => d.to_string(),
        };
        if !expanded.starts_with('/') || expanded.contains(':') {
            return Err(format!("run.path entry {expanded:?} must be an absolute directory"));
        }
        let canon = match std::fs::canonicalize(&expanded) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let meta = std::fs::metadata(&canon).map_err(|e| format!("run.path entry {expanded}: {e}"))?;
        if !meta.is_dir() {
            continue;
        }
        let mut used = Vec::new();
        let mut check = || -> Result<(), Untrusted> {
            trusted_dir(&canon, &meta, true, trust, &mut used)?;
            let mut up = canon.parent();
            while let Some(a) = up {
                let am = std::fs::metadata(a)
                    .map_err(|e| Untrusted::Other(format!("run.path ancestor {}: {e}", a.display())))?;
                trusted_dir(a, &am, false, trust, &mut used)?;
                up = a.parent();
            }
            Ok(())
        };
        match check() {
            Ok(()) => {}
            // The built-in default skips directories that fail the rule; a configured
            // entry that fails is an error.
            Err(Untrusted::Group(_)) if configured.is_none() => {
                out.skipped_group.push(canon.display().to_string());
                continue;
            }
            Err(_) if configured.is_none() => continue,
            Err(Untrusted::Group(e)) | Err(Untrusted::Other(e)) => return Err(e),
        }
        for u in used {
            if !out.trusted.contains(&u) {
                out.trusted.push(u);
            }
        }
        let c = canon.display().to_string();
        if c.contains(':') {
            return Err(format!("run.path entry {c:?} contains `:`"));
        }
        if !dirs.contains(&c) {
            dirs.push(c);
        }
    }
    out.path = dirs.join(":");
    Ok(out)
}

/// Channel kinds that are review or testing tracks, where a release candidate may go.
pub const REVIEW_KINDS: &[&str] = &["testflight", "play-testing", "store-review", "beta"];

/// Placeholders a command may use, each as a whole argument or environment value.
/// Commands get the verified commit, never a ref name that could move.
pub const PLACEHOLDERS: &[&str] = &["commit", "version", "build", "channel", "target"];

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
        let mut loaded = Loaded {
            cfg,
            root,
            path: path.to_path_buf(),
            final_tag,
            pre_tag,
            line_branch,
            hotfix_branch,
            todos,
            raw: text.clone(),
            run_path: String::new(),
            trusted_dirs: Vec::new(),
            skipped_dirs: Vec::new(),
            local_dirs: Vec::new(),
        };
        loaded.validate()?;
        let local = load_local(&loaded.root)?.unwrap_or_default();
        let mut trust = loaded.cfg.run.trust_group_writable.clone();
        trust.extend(local.run.trust_group_writable.iter().cloned());
        let rp =
            checked_run_path(loaded.cfg.run.path.as_deref(), &trust).map_err(|e| format!("{}: {e}", path.display()))?;
        // Local entries follow exactly the same rule as configured ones, and are refused,
        // never skipped, when they fail it.
        let local_err = |e: String| format!("{LOCAL_FILE}: {e}");
        let pre = checked_run_path(Some(&local.run.path_prepend.join(":")), &trust).map_err(local_err)?;
        let post = checked_run_path(Some(&local.run.path_append.join(":")), &trust).map_err(local_err)?;
        let mut dirs: Vec<String> = Vec::new();
        for part in [&pre.path, &rp.path, &post.path] {
            for d in part.split(':').filter(|d| !d.is_empty()) {
                if !dirs.iter().any(|x| x == d) {
                    dirs.push(d.to_string());
                }
            }
        }
        loaded.local_dirs =
            pre.path.split(':').chain(post.path.split(':')).filter(|d| !d.is_empty()).map(str::to_string).collect();
        loaded.run_path = dirs.join(":");
        let mut trusted = rp.trusted;
        for t in pre.trusted.into_iter().chain(post.trusted) {
            if !trusted.contains(&t) {
                trusted.push(t);
            }
        }
        loaded.trusted_dirs = trusted;
        loaded.skipped_dirs = rp.skipped_group;
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
        if !crate::util::valid_ref(&c.ledger_branch)
            || c.ledger_branch == c.main
            || self.line_branch.matches(&c.ledger_branch).is_some()
        {
            errs.push(format!("ledger_branch `{}` must be a branch name of its own", c.ledger_branch));
        }
        // Paths in the configuration stay inside the repository: `bump` writes version
        // files, and `ledger migrate` reads the legacy ledger.
        let inside = |p: &str| {
            let path = Path::new(p);
            !p.is_empty()
                && path.is_relative()
                && path.components().all(|c| matches!(c, std::path::Component::Normal(_)))
        };
        if !inside(&c.ledger) {
            errs.push(format!("ledger `{}` must be a relative path inside the repository", c.ledger));
        }
        for vf in &c.version_files {
            if !inside(&vf.path) {
                errs.push(format!("version_files path `{}` must be a relative path inside the repository", vf.path));
            }
        }
        for p in &c.pins {
            if !inside(&p.file) {
                errs.push(format!("pin {}: file `{}` must be a relative path inside the repository", p.name, p.file));
            }
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
            for k in ck.env.keys() {
                if GIT_DISCOVERY_VARS.contains(&k.as_str()) || k.starts_with("GIT_CONFIG_") {
                    errs.push(format!("check {name}: env may not set {k} (checks run where git finds no repository)"));
                }
            }
        }
        for (k, v) in [
            ("max_entries", c.check_tree.max_entries),
            ("max_file_bytes", c.check_tree.max_file_bytes),
            ("max_total_bytes", c.check_tree.max_total_bytes),
            ("max_listing_bytes", c.check_tree.max_listing_bytes),
        ] {
            if v == Some(0) {
                errs.push(format!("check_tree.{k} must be at least 1"));
            }
        }
        for (name, ch) in &c.channels {
            if !name.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
                || name.starts_with('-')
            {
                errs.push(format!("channel `{name}`: names use lowercase letters, digits and hyphens"));
            }
            check_cmd(&format!("channel {name} run"), &ch.run, &ch.env, &mut errs);
            if let Some(ProbeEnv::Same(s)) = &ch.probe_env {
                if s != PROBE_ENV_CHANNEL {
                    errs.push(format!(
                        "channel {name}: probe_env is a table of variables or \"{PROBE_ENV_CHANNEL}\", not {s:?}"
                    ));
                }
            }
            check_cmd(&format!("channel {name} probe"), &ch.probe, &ch.probe_env(), &mut errs);
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
        if let Some(t) = c.receipts.ttl_minutes {
            if t == 0 || t > crate::steps::RECEIPT_TTL_MAX {
                errs.push(format!("receipts.ttl_minutes must be between 1 and {}", crate::steps::RECEIPT_TTL_MAX));
            }
        }
        for d in &c.run.trust_group_writable {
            let canonical = std::fs::canonicalize(d).map(|p| p.display().to_string()).ok();
            // A listed directory that does not exist on this machine is simply unused.
            let mismatch = canonical.as_deref().is_some_and(|c| c != d);
            if !d.starts_with('/') || d.contains(['*', '?', '[']) || mismatch {
                errs.push(format!(
                    "run.trust_group_writable: {d:?} must be an exact canonical path{}",
                    canonical.map(|c| format!(" (did you mean {c:?}?)")).unwrap_or_default()
                ));
            }
        }
        for v in &c.run.tool_env {
            if crate::git::forbidden_tool_var(v) {
                errs.push(format!("run.tool_env: {v} may not be passed to git or gh"));
            }
        }
        if let Some(p) = &c.run.path {
            if p.split(':').any(|d| !d.is_empty() && !d.starts_with('/') && !d.starts_with("~/")) {
                errs.push("run.path must list absolute directories (or ones under ~/)".into());
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

    pub fn receipt_ttl(&self) -> u64 {
        self.cfg.receipts.ttl_minutes.unwrap_or(crate::steps::RECEIPT_TTL_DEFAULT)
    }

    /// `PATH` for every command, checked when the configuration loads.
    pub fn run_path(&self) -> String {
        self.run_path.clone()
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
        assert!(!glob_match("release/*", "feature-sync"));
        assert!(glob_match("main", "main"));
        assert!(glob_match("docs/*-review-*.md", "docs/0.6-review-notes.md"));
        assert!(glob_match("docs/**/*.md", "docs/a/b/c.md"));
        assert!(glob_match("docs/**/*.md", "docs/c.md"));
        assert!(glob_match("**/secrets.env", "a/b/secrets.env"));
    }
}
