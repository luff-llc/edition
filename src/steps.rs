//! A plan is a list of steps. Edition prints every step; `--apply` runs local ones;
//! outward ones also need `--confirm <token>` for this run.

use crate::util::shell_line;
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A read-only or test command that must pass before anything else runs.
    Check,
    /// Changes only this machine (a local branch, a local tag, a file).
    Local,
    /// Leaves this machine: a push, a deploy, an upload, a pull request.
    Outward,
    /// Runs after the outward step succeeded (health probe, Concordance fact).
    After,
    /// Printed only: something a person or another tool must do.
    Note,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Check => "check",
            Kind::Local => "local",
            Kind::Outward => "OUTWARD",
            Kind::After => "after",
            Kind::Note => "note",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Step {
    pub kind: Kind,
    pub what: String,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

impl Step {
    pub fn new(kind: Kind, what: &str, argv: Vec<String>, cwd: &std::path::Path) -> Step {
        Step { kind, what: what.to_string(), argv, env: Vec::new(), cwd: cwd.to_path_buf() }
    }

    pub fn note(what: &str) -> Step {
        Step { kind: Kind::Note, what: what.to_string(), argv: Vec::new(), env: Vec::new(), cwd: PathBuf::new() }
    }

    pub fn display(&self) -> String {
        let mut s = format!("  [{}] {}", self.kind.label(), self.what);
        if !self.argv.is_empty() {
            let env: String = self.env.iter().map(|(k, v)| format!("{k}={} ", crate::util::shell_quote(v))).collect();
            s.push_str(&format!(
                "\n      $ (cd {} && {env}{})",
                crate::util::shell_quote(&self.cwd.display().to_string()),
                shell_line(&self.argv)
            ));
        }
        s
    }
}

#[derive(Clone, Debug, Default)]
pub struct Mode {
    pub apply: bool,
    pub confirm: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub kind: Kind,
    pub what: String,
    pub argv: Vec<String>,
    pub code: Option<i32>,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

pub enum Ran {
    /// Nothing ran: the plan was printed.
    DryRun,
    /// Steps ran (stopping at the first failure).
    Ran(Vec<Outcome>),
}

pub fn print(title: &str, steps: &[Step]) {
    println!("{title}");
    for s in steps {
        println!("{}", s.display());
    }
}

/// Runs the plan. Outward steps need `confirm == token`; this is checked before
/// anything runs, so a missing confirmation never leaves a half-done plan.
pub fn execute(steps: &[Step], mode: &Mode, token: &str) -> Result<Ran, String> {
    let has_outward = steps.iter().any(|s| s.kind == Kind::Outward);
    if !mode.apply {
        println!();
        if has_outward {
            println!("Dry run: nothing was run. To run it: --apply --confirm {token}");
        } else {
            println!("Dry run: nothing was run. To run it: --apply");
        }
        return Ok(Ran::DryRun);
    }
    if has_outward && mode.confirm.as_deref() != Some(token) {
        return Err(format!("this plan has outward steps; nothing was run. Confirm this run with --confirm {token}"));
    }
    let mut outcomes = Vec::new();
    for s in steps {
        if s.kind == Kind::Note || s.argv.is_empty() {
            continue;
        }
        println!("> [{}] {}", s.kind.label(), s.what);
        let mut c = Command::new(&s.argv[0]);
        c.args(&s.argv[1..]).current_dir(&s.cwd);
        for (k, v) in &s.env {
            c.env(k, v);
        }
        let code = match c.status() {
            Ok(st) => st.code(),
            Err(e) => {
                eprintln!("edition: cannot run {}: {e}", s.argv[0]);
                None
            }
        };
        let o = Outcome { kind: s.kind, what: s.what.clone(), argv: s.argv.clone(), code };
        let ok = o.ok();
        outcomes.push(o);
        if !ok {
            eprintln!("edition: step failed ({}), stopping", s.what);
            break;
        }
    }
    Ok(Ran::Ran(outcomes))
}
