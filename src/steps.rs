//! Plans, receipts and execution.
//!
//! A plan is a list of steps plus the facts it was checked against (resolved commits,
//! live remote heads, the configuration digest). A dry run prints the plan and, when it
//! has outward steps, issues a **receipt**: a token derived from a hash of the canonical
//! plan. `--apply --confirm <receipt>` runs the plan only if the plan recomputed at that
//! moment hashes to the same receipt, the receipt is young enough and has not been used.
//! Immediately before each outward step the remote is read again and the step's
//! expectations (which refs point where) are checked.

use crate::git::Git;
use crate::util::{now_secs, random_hex, sha256_hex, shell_quote};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A read-only or test command that must pass before anything else runs.
    Check,
    /// Changes only this machine (a local branch, a local tag, a file).
    Local,
    /// Leaves this machine: a push, a deploy, an upload, a pull request.
    Outward,
    /// Runs after the outward steps succeeded (health probe, Concordance fact).
    After,
    /// Printed only.
    Note,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Check => "check",
            Kind::Local => "local",
            Kind::Outward => "OUTWARD",
            Kind::After => "after",
            Kind::Note => "note",
        }
    }
}

/// A ref on the remote that must hold exactly this commit (`None`: must not exist)
/// immediately before a step runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expect {
    pub refname: String,
    pub oid: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Step {
    pub kind: Kind,
    pub what: String,
    /// What runs.
    pub argv: Vec<String>,
    /// What is printed: values that came from the environment are shown by name.
    pub shown: Vec<String>,
    /// What the ledger records: the configured template, never expanded values.
    pub template: Vec<String>,
    /// Environment for the command. Only the names are printed or recorded.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub expects: Vec<Expect>,
    /// Configured commands run with a cleared environment: the base variables
    /// (`PATH`, `HOME`, `LANG`, `TMPDIR`) and what the channel declares, nothing else.
    pub isolated: bool,
    /// The program, resolved to an absolute path when the plan is made.
    pub program: PathBuf,
    /// Where an outward step goes (a push URL, a repository, a channel target).
    pub dest: Option<String>,
}

/// The `PATH` configured commands get when `edition.toml` does not set `[run] path`.
pub const DEFAULT_PATH: &str = "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Inherited variables an isolated command keeps, besides `PATH`.
pub const BASE_VARS: &[&str] = &["HOME", "LANG", "TMPDIR"];

/// Finds `program` the way a shell would: a path containing `/` is taken relative to
/// `cwd`; a bare name is searched for in `path`.
pub fn resolve_program(program: &str, cwd: &Path, path: &str) -> Result<PathBuf, String> {
    let is_exec = |p: &Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    };
    if program.contains('/') {
        let p = if Path::new(program).is_absolute() { PathBuf::from(program) } else { cwd.join(program) };
        return if is_exec(&p) { Ok(p) } else { Err(format!("{} is not an executable file", p.display())) };
    }
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let p = Path::new(dir).join(program);
        if is_exec(&p) {
            return Ok(p);
        }
    }
    Err(format!("{program} is not on the PATH commands run with ({path})"))
}

impl Step {
    /// A step whose arguments are all safe to print and record (git, gh).
    pub fn plain(kind: Kind, what: &str, argv: Vec<String>, cwd: &Path) -> Step {
        let shown: Vec<String> = argv.iter().map(|a| crate::util::redact(a)).collect();
        Step {
            kind,
            what: crate::util::redact(what),
            template: shown.clone(),
            shown,
            argv,
            env: Vec::new(),
            cwd: cwd.to_path_buf(),
            expects: Vec::new(),
            isolated: false,
            program: PathBuf::new(),
            dest: None,
        }
    }

    pub fn to(mut self, dest: &str) -> Step {
        self.dest = Some(dest.to_string());
        self
    }

    /// The complete environment the command runs with. Edition's own git and gh steps
    /// get the tool environment (see `git::tool_env`) plus their own variables; configured
    /// commands get the base variables plus what they declare.
    pub fn run_env(&self, run_path: &str, tool_env: &[(String, String)]) -> Vec<(String, String)> {
        if !self.isolated {
            let mut env = tool_env.to_vec();
            for (k, v) in &self.env {
                env.retain(|(n, _)| n != k);
                env.push((k.clone(), v.clone()));
            }
            return env;
        }
        let mut env = vec![("PATH".to_string(), run_path.to_string())];
        for k in BASE_VARS {
            if let Ok(v) = std::env::var(k) {
                env.push((k.to_string(), v));
            }
        }
        for (k, v) in &self.env {
            env.retain(|(n, _)| n != k);
            env.push((k.clone(), v.clone()));
        }
        env
    }

    pub fn note(what: &str) -> Step {
        Step::plain(Kind::Note, what, Vec::new(), Path::new(""))
    }

    pub fn expecting(mut self, refname: &str, oid: Option<&str>) -> Step {
        self.expects.push(Expect { refname: refname.to_string(), oid: oid.map(str::to_string) });
        self
    }

    pub fn display(&self) -> String {
        let mut s = format!("  [{}] {}", self.kind.label(), self.what);
        if !self.argv.is_empty() {
            let line = self.shown.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
            s.push_str(&format!("\n      $ (cd {} && {line})", shell_quote(&self.cwd.display().to_string())));
            if !self.env.is_empty() {
                let names: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
                s.push_str(&format!("\n      env: {}", names.join(", ")));
            }
        }
        if let Some(d) = &self.dest {
            s.push_str(&format!("\n      destination: {}", crate::util::redact(d)));
        }
        for e in &self.expects {
            s.push_str(&format!(
                "\n      requires remote {} = {}",
                e.refname,
                e.oid.as_deref().map(crate::util::short).unwrap_or("absent")
            ));
        }
        s
    }

    /// The ledger's description of this step: its name and configured arguments.
    pub fn description(&self) -> String {
        format!("{}: {}", self.what, self.template.join(" "))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub title: String,
    pub steps: Vec<Step>,
    /// Facts the plan was checked against: `name=value`.
    pub facts: Vec<String>,
    pub remote: String,
    /// `PATH` for isolated commands.
    pub run_path: String,
    /// The environment for Edition's own git and gh steps.
    pub tool_env: Vec<(String, String)>,
    /// Group-writable `PATH` directories trusted by configuration (printed and hashed).
    pub trusted_dirs: Vec<String>,
    /// Default `PATH` directories skipped for being group-writable (named when a program
    /// is not found).
    pub skipped_dirs: Vec<String>,
}

impl Plan {
    pub fn new(title: &str, remote: &str) -> Plan {
        Plan { title: title.to_string(), remote: remote.to_string(), run_path: DEFAULT_PATH.into(), ..Plan::default() }
    }

    /// Resolves every step's program to an absolute path. Called once the steps are
    /// final; the resolved paths are part of the plan hash.
    pub fn resolve(&mut self) -> Result<(), String> {
        for s in self.steps.iter_mut().filter(|s| !s.argv.is_empty()) {
            let path = self.run_path.clone();
            s.program = resolve_program(&s.argv[0], &s.cwd, &path).map_err(|e| {
                let hint = if self.skipped_dirs.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; skipped for being group-writable: {} (list a directory in [run] trust_group_writable to use it)",
                        self.skipped_dirs.join(", ")
                    )
                };
                format!("{}: {e}{hint}", s.what)
            })?;
        }
        Ok(())
    }

    pub fn fact(&mut self, k: &str, v: &str) {
        self.facts.push(format!("{k}={v}"));
    }

    pub fn has_outward(&self) -> bool {
        self.steps.iter().any(|s| s.kind == Kind::Outward)
    }

    pub fn print(&self) {
        crate::say!("{}", self.title);
        if !self.trusted_dirs.is_empty() {
            crate::say!("  trusting group-writable PATH directories: {}", self.trusted_dirs.join(", "));
        }
        for s in &self.steps {
            crate::say!("{}", s.display());
        }
    }

    /// The canonical text of the plan: everything that decides what would happen.
    /// Notes and timestamps are excluded; environment values are included (hashed,
    /// never printed) so a changed value invalidates the receipt.
    pub fn canonical(&self, config_digest: &str) -> String {
        let mut t = String::from("edition-plan 2\n");
        t.push_str(&format!("config {config_digest}\nremote {}\nrun-path {:?}\n", self.remote, self.run_path));
        for d in &self.trusted_dirs {
            t.push_str(&format!("trusted-group-writable {d:?}\n"));
        }
        for f in &self.facts {
            t.push_str(&format!("fact {f:?}\n"));
        }
        for s in self.steps.iter().filter(|s| s.kind != Kind::Note) {
            t.push_str(&format!("step {} {:?}\n", s.kind.label(), s.what));
            t.push_str(&format!("  cwd {:?}\n", s.cwd.display().to_string()));
            t.push_str(&format!("  program {:?}\n", s.program.display().to_string()));
            for (k, v) in s.run_env(&self.run_path, &self.tool_env) {
                t.push_str(&format!("  env {k:?}={v:?}\n"));
            }
            t.push_str(&format!("  dest {:?}\n", s.dest));
            for a in &s.argv {
                t.push_str(&format!("  arg {a:?}\n"));
            }
            for e in &s.expects {
                t.push_str(&format!("  expect {} {}\n", e.refname, e.oid.as_deref().unwrap_or("absent")));
            }
        }
        t
    }

    /// SHA-256 of the canonical plan.
    pub fn digest(&self, config_digest: &str) -> String {
        sha256_hex(self.canonical(config_digest).as_bytes())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Mode {
    pub apply: bool,
    pub confirm: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub index: usize,
    pub kind: Kind,
    pub what: String,
    pub code: Option<i32>,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    pub fn describe(&self) -> String {
        format!("{}: exit {}", self.what, self.code.map(|c| c.to_string()).unwrap_or_else(|| "none".into()))
    }
}

pub enum Ran {
    DryRun,
    Ran { outcomes: Vec<Outcome>, receipt: Option<String>, stopped: Option<String> },
}

/// Receipt state, in `.edition/receipts` beside the configuration (never committed).
///
/// A dry run issues a random 128-bit nonce and stores `issued <nonce> <plan sha256>
/// <issued at> <expires at>`. Redemption holds one exclusive lock across reading the
/// file, validating the nonce (issued, unexpired, unused, same plan hash) and appending
/// `used <nonce> <time>`, and happens before the first step runs: a crash afterwards
/// leaves the receipt spent, and the dry run must be repeated.
pub struct Receipts {
    dir: PathBuf,
}

/// Default and maximum receipt lifetime, in minutes.
pub const RECEIPT_TTL_DEFAULT: u64 = 30;
pub const RECEIPT_TTL_MAX: u64 = 60;

// open(2) flags, per target, so no libc dependency is needed. Values from each
// platform's <fcntl.h>; any other target fails to compile rather than guess.
#[cfg(target_os = "macos")]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(target_os = "macos")]
const O_DIRECTORY: i32 = 0x0010_0000;
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
const O_NOFOLLOW: i32 = 0o400_000;
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
const O_DIRECTORY: i32 = 0o200_000;
#[cfg(all(target_os = "linux", target_arch = "arm"))]
const O_NOFOLLOW: i32 = 0o100_000;
#[cfg(all(target_os = "linux", target_arch = "arm"))]
const O_DIRECTORY: i32 = 0o40_000;
#[cfg(not(any(
    target_os = "macos",
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "arm"))
)))]
compile_error!("Edition knows O_NOFOLLOW and O_DIRECTORY for macOS and Linux x86_64, aarch64 and arm only");

fn private(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o077 == 0
}

impl Receipts {
    pub fn at(root: &Path) -> Receipts {
        Receipts { dir: root.join(".edition") }
    }

    /// Opens the state directory (mode 0700) and returns the locked receipts file
    /// (mode 0600). Refuses either if group or others can access it.
    fn open_locked(&self) -> Result<std::fs::File, String> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
        if std::fs::symlink_metadata(&self.dir).is_err() {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&self.dir)
                .map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
            let _ = std::fs::write(self.dir.join(".gitignore"), "*\n");
        }
        // Open the directory itself without following a symlink and check it through the
        // handle, then open the file without following a symlink and check that handle.
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_DIRECTORY)
            .open(&self.dir)
            .map_err(|e| format!("{} must be a private directory, not a link: {e}", self.dir.display()))?;
        let dm = dir.metadata().map_err(|e| e.to_string())?;
        if !dm.is_dir() || !private(&dm) {
            return Err(format!(
                "{} must be a private directory (mode 0700); refusing to use receipts kept there",
                self.dir.display()
            ));
        }
        let path = self.dir.join("receipts");
        let f = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(&path)
            .map_err(|e| format!("cannot open {} (it must be a regular file, not a link): {e}", path.display()))?;
        let fm = f.metadata().map_err(|e| e.to_string())?;
        if !fm.is_file() || !private(&fm) || fm.uid() != dm.uid() {
            return Err(format!(
                "{} must be private (mode 0600) and owned like its directory; refusing to use it",
                path.display()
            ));
        }
        // The directory path must still name the directory that was checked.
        let again = std::fs::symlink_metadata(&self.dir).map_err(|e| e.to_string())?;
        if again.dev() != dm.dev() || again.ino() != dm.ino() {
            return Err(format!("{} changed while it was being opened; refusing", self.dir.display()));
        }
        f.lock().map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
        Ok(f)
    }

    fn read_all(f: &mut std::fs::File) -> Result<String, String> {
        let mut text = String::new();
        f.rewind().map_err(|e| e.to_string())?;
        f.read_to_string(&mut text).map_err(|e| e.to_string())?;
        Ok(text)
    }

    /// Issues a receipt for a plan hash; returns the nonce the user passes back.
    pub fn issue(&self, plan_hash: &str, ttl_minutes: u64) -> Result<String, String> {
        let nonce = random_hex(16)?;
        let now = now_secs();
        let mut f = self.open_locked()?;
        f.write_all(format!("issued {nonce} {plan_hash} {now} {}\n", now + ttl_minutes * 60).as_bytes())
            .map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        Ok(nonce)
    }

    /// Validates and spends a receipt under one lock. `used_elsewhere` reports whether
    /// the ledger already records it.
    pub fn redeem(&self, nonce: &str, plan_hash: &str, used_elsewhere: bool) -> Result<(), String> {
        if nonce.len() != 32 || !nonce.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("a receipt is the 32-character value a dry run printed".into());
        }
        let mut f = self.open_locked()?;
        let text = Self::read_all(&mut f)?;
        let mut issued: Option<(String, u64)> = None;
        let mut used = used_elsewhere;
        for line in text.lines() {
            let p: Vec<&str> = line.split(' ').collect();
            match p.as_slice() {
                ["issued", n, h, _, exp] if *n == nonce => issued = Some((h.to_string(), exp.parse().unwrap_or(0))),
                ["used", n, _] if *n == nonce => used = true,
                _ => {}
            }
        }
        if used {
            return Err(format!("receipt {nonce} was already used; run the dry run again"));
        }
        let Some((stored, expires)) = issued else {
            return Err(format!("receipt {nonce} was not issued by a dry run here"));
        };
        if now_secs() > expires {
            return Err(format!("receipt {nonce} expired; run the dry run again"));
        }
        if stored != plan_hash {
            return Err(format!(
                "receipt {nonce} does not match this plan (the refs, configuration, environment or remote state changed since the dry run); nothing was run"
            ));
        }
        f.write_all(format!("used {nonce} {}\n", now_secs()).as_bytes()).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Reads the remote and checks every expectation. Returns the first mismatch.
pub fn check_expects(git: &Git, remote: &str, expects: &[Expect]) -> Option<String> {
    if expects.is_empty() {
        return None;
    }
    let (heads, tags) = match git.ls_remote(remote) {
        Ok(x) => x,
        Err(e) => return Some(crate::util::redact(&format!("cannot read {remote}: {e}"))),
    };
    for e in expects {
        let actual = if let Some(h) = e.refname.strip_prefix("refs/heads/") {
            heads.iter().find(|(n, _)| n == h).map(|(_, c)| c.clone())
        } else if let Some(t) = e.refname.strip_prefix("refs/tags/") {
            tags.iter().find(|(n, _)| n == t).map(|(_, c)| c.clone())
        } else {
            None
        };
        if actual != e.oid {
            return Some(format!(
                "remote {} is now {} but the plan was checked with {}; nothing further was run",
                e.refname,
                actual.as_deref().map(crate::util::short).unwrap_or("absent"),
                e.oid.as_deref().map(crate::util::short).unwrap_or("absent")
            ));
        }
    }
    None
}

/// Prints the plan; with `--apply`, runs it. `used_in_ledger` says whether the ledger
/// already records a receipt.
pub fn execute(
    plan: &Plan,
    mode: &Mode,
    git: &Git,
    root: &Path,
    config_digest: &str,
    ttl_minutes: u64,
    used_in_ledger: &dyn Fn(&str) -> bool,
) -> Result<Ran, String> {
    plan.print();
    let outward = plan.has_outward();
    let receipts = Receipts::at(root);
    let _ = git;
    if !mode.apply {
        crate::say!();
        if outward {
            let r = receipts.issue(&plan.digest(config_digest), ttl_minutes)?;
            crate::say!("Receipt: {r} (valid {ttl_minutes} minutes, once)");
            crate::say!("Dry run: nothing was run. To run exactly this plan: --apply --confirm {r}");
        } else {
            crate::say!("Dry run: nothing was run. To run it: --apply");
        }
        return Ok(Ran::DryRun);
    }
    let mut receipt = None;
    if outward {
        let Some(nonce) = mode.confirm.as_deref() else {
            return Err("this plan has outward steps; run the dry run and pass its receipt with --confirm".into());
        };
        receipts.redeem(nonce, &plan.digest(config_digest), used_in_ledger(nonce))?;
        receipt = Some(nonce.to_string());
    }
    let mut outcomes = Vec::new();
    let mut stopped = None;
    for (i, s) in plan.steps.iter().enumerate() {
        if s.kind == Kind::Note || s.argv.is_empty() {
            continue;
        }
        if s.kind == Kind::Outward {
            if let Some(why) = check_expects(git, &plan.remote, &s.expects) {
                crate::warn!("edition: {why}");
                stopped = Some(why);
                break;
            }
        }
        crate::say!("> [{}] {}", s.kind.label(), s.what);
        let mut c = Command::new(&s.program);
        c.args(&s.argv[1..]).current_dir(&s.cwd);
        c.env_clear();
        c.envs(s.run_env(&plan.run_path, &plan.tool_env));
        // Edition's own git and gh output can name the remote URL: capture it and print it
        // redacted. Configured commands write straight to the terminal.
        let code = if s.isolated {
            match c.status() {
                Ok(st) => st.code(),
                Err(e) => {
                    crate::warn!("edition: cannot run {}: {e}", s.shown.first().cloned().unwrap_or_default());
                    None
                }
            }
        } else {
            match c.output() {
                Ok(out) => {
                    crate::say_raw!("{}", crate::util::redact(&String::from_utf8_lossy(&out.stdout)));
                    crate::warn_raw!("{}", crate::util::redact(&String::from_utf8_lossy(&out.stderr)));
                    out.status.code()
                }
                Err(e) => {
                    crate::warn!("edition: cannot run {}: {e}", s.shown.first().cloned().unwrap_or_default());
                    None
                }
            }
        };
        let o = Outcome { index: i, kind: s.kind, what: s.what.clone(), code };
        let ok = o.ok();
        outcomes.push(o);
        if !ok {
            crate::warn!("edition: step failed ({}), stopping", s.what);
            break;
        }
    }
    Ok(Ran::Ran { outcomes, receipt, stopped })
}
