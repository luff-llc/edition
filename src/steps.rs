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
use crate::util::{now_secs, shell_quote};
use std::io::Write;
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
}

impl Step {
    /// A step whose arguments are all safe to print and record (git, gh).
    pub fn plain(kind: Kind, what: &str, argv: Vec<String>, cwd: &Path) -> Step {
        Step {
            kind,
            what: what.to_string(),
            shown: argv.clone(),
            template: argv.clone(),
            argv,
            env: Vec::new(),
            cwd: cwd.to_path_buf(),
            expects: Vec::new(),
        }
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
}

impl Plan {
    pub fn new(title: &str, remote: &str) -> Plan {
        Plan { title: title.to_string(), remote: remote.to_string(), ..Plan::default() }
    }

    pub fn fact(&mut self, k: &str, v: &str) {
        self.facts.push(format!("{k}={v}"));
    }

    pub fn has_outward(&self) -> bool {
        self.steps.iter().any(|s| s.kind == Kind::Outward)
    }

    pub fn print(&self) {
        println!("{}", self.title);
        for s in &self.steps {
            println!("{}", s.display());
        }
    }

    /// The canonical text of the plan: everything that decides what would happen.
    /// Notes and timestamps are excluded; environment values are included (hashed,
    /// never printed) so a changed value invalidates the receipt.
    pub fn canonical(&self, config_digest: &str) -> String {
        let mut t = String::from("edition-plan 1\n");
        t.push_str(&format!("config {config_digest}\nremote {}\n", self.remote));
        for f in &self.facts {
            t.push_str(&format!("fact {f:?}\n"));
        }
        for s in self.steps.iter().filter(|s| s.kind != Kind::Note) {
            t.push_str(&format!("step {} {:?}\n", s.kind.label(), s.what));
            t.push_str(&format!("  cwd {:?}\n", s.cwd.display().to_string()));
            for a in &s.argv {
                t.push_str(&format!("  arg {a:?}\n"));
            }
            for (k, v) in &s.env {
                t.push_str(&format!("  env {k:?}={v:?}\n"));
            }
            for e in &s.expects {
                t.push_str(&format!("  expect {} {}\n", e.refname, e.oid.as_deref().unwrap_or("absent")));
            }
        }
        t
    }

    pub fn receipt(&self, git: &Git, config_digest: &str) -> Result<String, String> {
        let h = git.hash(&self.canonical(config_digest))?;
        Ok(h.chars().take(16).collect())
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

/// Receipt state, kept in `.edition/receipts` beside the configuration (never committed).
pub struct Receipts {
    path: PathBuf,
}

pub fn receipt_ttl() -> u64 {
    std::env::var("EDITION_RECEIPT_TTL").ok().and_then(|v| v.parse().ok()).unwrap_or(1800)
}

impl Receipts {
    pub fn at(root: &Path) -> Receipts {
        Receipts { path: root.join(".edition").join("receipts") }
    }

    fn lines(&self) -> Vec<(String, String, u64)> {
        std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut it = l.split(' ');
                Some((it.next()?.to_string(), it.next()?.to_string(), it.next()?.parse().ok()?))
            })
            .collect()
    }

    fn append(&self, kind: &str, receipt: &str) -> Result<(), String> {
        let dir = self.path.parent().ok_or("no state directory")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            let _ = std::fs::write(&ignore, "*\n");
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("receipts.lock"))
            .map_err(|e| e.to_string())?;
        lock.lock().map_err(|e| format!("cannot lock receipts: {e}"))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("cannot write {}: {e}", self.path.display()))?;
        f.write_all(format!("{kind} {receipt} {}\n", now_secs()).as_bytes()).map_err(|e| e.to_string())?;
        drop(lock);
        Ok(())
    }

    pub fn issue(&self, receipt: &str) -> Result<(), String> {
        self.append("issued", receipt)
    }

    /// Checks a receipt and marks it used. `used_elsewhere`: the ledger already has it.
    pub fn redeem(&self, receipt: &str, used_elsewhere: bool) -> Result<(), String> {
        let lines = self.lines();
        if used_elsewhere || lines.iter().any(|(k, r, _)| k == "used" && r == receipt) {
            return Err(format!("receipt {receipt} was already used; run the dry run again"));
        }
        let issued = lines.iter().filter(|(k, r, _)| k == "issued" && r == receipt).map(|(_, _, t)| *t).max();
        let Some(at) = issued else {
            return Err(format!("receipt {receipt} was not issued by a dry run of this plan here"));
        };
        let age = now_secs().saturating_sub(at);
        if age > receipt_ttl() {
            return Err(format!(
                "receipt {receipt} expired ({age}s old, limit {}s); run the dry run again",
                receipt_ttl()
            ));
        }
        self.append("used", receipt)
    }
}

/// Reads the remote and checks every expectation. Returns the first mismatch.
pub fn check_expects(git: &Git, remote: &str, expects: &[Expect]) -> Option<String> {
    if expects.is_empty() {
        return None;
    }
    let (heads, tags) = match git.ls_remote(remote) {
        Ok(x) => x,
        Err(e) => return Some(format!("cannot read {remote}: {e}")),
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
    used_in_ledger: &dyn Fn(&str) -> bool,
) -> Result<Ran, String> {
    plan.print();
    let outward = plan.has_outward();
    let receipts = Receipts::at(root);
    if !mode.apply {
        println!();
        if outward {
            let r = plan.receipt(git, config_digest)?;
            receipts.issue(&r)?;
            println!("Receipt: {r} (valid {} minutes, once)", receipt_ttl() / 60);
            println!("Dry run: nothing was run. To run exactly this plan: --apply --confirm {r}");
        } else {
            println!("Dry run: nothing was run. To run it: --apply");
        }
        return Ok(Ran::DryRun);
    }
    let mut receipt = None;
    if outward {
        let want = plan.receipt(git, config_digest)?;
        match mode.confirm.as_deref() {
            None => {
                return Err("this plan has outward steps; run the dry run and pass its receipt with --confirm".into())
            }
            Some(c) if c != want => {
                return Err(format!(
                    "receipt {c} does not match this plan (the refs, configuration or remote state changed since the dry run, or it was issued for another plan); nothing was run"
                ))
            }
            Some(_) => {}
        }
        receipts.redeem(&want, used_in_ledger(&want))?;
        receipt = Some(want);
    }
    let mut outcomes = Vec::new();
    let mut stopped = None;
    for (i, s) in plan.steps.iter().enumerate() {
        if s.kind == Kind::Note || s.argv.is_empty() {
            continue;
        }
        if s.kind == Kind::Outward {
            if let Some(why) = check_expects(git, &plan.remote, &s.expects) {
                eprintln!("edition: {why}");
                stopped = Some(why);
                break;
            }
        }
        println!("> [{}] {}", s.kind.label(), s.what);
        let mut program = PathBuf::from(&s.argv[0]);
        if program.components().count() > 1 && program.is_relative() {
            program = s.cwd.join(program);
        }
        let mut c = Command::new(&program);
        c.args(&s.argv[1..]).current_dir(&s.cwd);
        for (k, v) in &s.env {
            c.env(k, v);
        }
        let code = match c.status() {
            Ok(st) => st.code(),
            Err(e) => {
                eprintln!("edition: cannot run {}: {e}", s.shown.first().cloned().unwrap_or_default());
                None
            }
        };
        let o = Outcome { index: i, kind: s.kind, what: s.what.clone(), code };
        let ok = o.ok();
        outcomes.push(o);
        if !ok {
            eprintln!("edition: step failed ({}), stopping", s.what);
            break;
        }
    }
    Ok(Ran::Ran { outcomes, receipt, stopped })
}
