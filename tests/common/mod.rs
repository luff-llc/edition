//! Helpers for scenario tests: temporary repositories with bare remotes, and running
//! the `edition` binary. Nothing here touches the network.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn all(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }

    pub fn receipt(&self) -> String {
        self.stdout
            .lines()
            .find_map(|l| l.strip_prefix("Receipt: "))
            .and_then(|r| r.split(' ').next())
            .map(str::to_string)
            .unwrap_or_else(|| panic!("no receipt in:\n{}", self.all()))
    }
}

pub fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let d = std::env::temp_dir().join(format!("edition-test-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&d).expect("tmp dir");
    d.canonicalize().expect("canonical")
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub const NOW: &str = "2026-11-23T12:00:00Z";

/// Moves a receipt's expiry into the past by editing the state file, standing in for
/// the passage of time (Edition has no clock override).
pub fn expire_receipt(work: &Path, receipt: &str) {
    let path = work.join(".edition/receipts");
    let text = std::fs::read_to_string(&path).expect("receipts");
    let out: String = text
        .lines()
        .map(|l| {
            let p: Vec<&str> = l.split(' ').collect();
            if p.len() == 5 && p[0] == "issued" && p[1] == receipt {
                format!("issued {} {} {} 1\n", p[1], p[2], p[3])
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    std::fs::write(&path, out).expect("write receipts");
}

pub fn edition_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Out {
    let mut c = Command::new(env!("CARGO_BIN_EXE_edition"));
    c.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .env("EDITION_ACTOR", "test-agent")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid");
    // Check trees go beside the test's repositories, never in the real cache.
    if let Some(root) = dir.parent() {
        c.env("XDG_CACHE_HOME", root.join("cache"));
    }
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().expect("edition");
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

pub fn edition(dir: &Path, args: &[&str]) -> Out {
    edition_env(dir, args, &[])
}

/// Dry run, then apply with the receipt the dry run issued.
pub fn apply_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Out {
    let dry = edition_env(dir, args, env);
    assert_eq!(dry.code, 0, "dry run failed: {}", dry.all());
    let r = dry.receipt();
    let mut full: Vec<&str> = args.to_vec();
    full.extend(["--apply", "--confirm", &r]);
    edition_env(dir, &full, env)
}

pub fn apply(dir: &Path, args: &[&str]) -> Out {
    apply_env(dir, args, &[])
}

pub fn write(dir: &Path, path: &str, text: &str) {
    let p = dir.join(path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(p, text).expect("write");
}

pub fn executable(dir: &Path, path: &str, text: &str) {
    write(dir, path, text);
    Command::new("chmod").arg("+x").arg(dir.join(path)).status().expect("chmod");
}

pub fn commit_all(dir: &Path, msg: &str) -> String {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

pub const BASE_CONFIG: &str = r#"
product = "lib"

[[version_files]]
path = "Cargo.toml"
after = "[package]"
pattern = 'version = "{version}"'

[[version_files]]
path = "pkg/package.json"
pattern = '"version": "{version}"'

[checks.unit]
run = ["true"]

[channels.local]
tier = "dev"
kind = "home"
target = "http://localhost:4795"
run = ["scripts/record.sh", "dev-deploys.txt", "{commit}"]

[channels.staging-home]
tier = "staging"
kind = "home"
target = "https://staging.example.com"
checks = ["unit"]
run = ["scripts/record.sh", "staging-deploys.txt", "{commit}"]
probe = ["true"]

[channels.production-home]
tier = "prod"
kind = "home"
target = "https://home.example.com"
approval = "owner"
run = ["scripts/record.sh", "prod-deploys.txt", "{commit}"]

[channels.store-review]
tier = "prod"
kind = "testflight"
target = "external testers"
allow_prerelease = true
run = ["scripts/record.sh", "review.txt", "{commit}"]
"#;

pub const RECORD_SH: &str = "#!/bin/sh\nout=\"$1\"; shift; printf '%s\\n' \"$*\" >> \"$out\"\n";

pub fn set_version(dir: &Path, v: &str) {
    write(dir, "Cargo.toml", &format!("[package]\nname = \"lib\"\nversion = \"{v}\"\n"));
    write(dir, "pkg/package.json", &format!("{{\n  \"name\": \"lib\",\n  \"version\": \"{v}\"\n}}\n"));
}

/// A work clone of a bare origin with main at `version`, pushed.
pub fn project(name: &str, config: &str, version: &str) -> (PathBuf, PathBuf) {
    let root = tmp(name);
    let origin = root.join("origin.git");
    git(&root, &["init", "-q", "--bare", origin.to_str().expect("utf8")]);
    let work = root.join("work");
    git(&root, &["clone", "-q", origin.to_str().expect("utf8"), "work"]);
    git(&work, &["checkout", "-q", "-b", "main"]);
    write(&work, "edition.toml", config);
    write(&work, ".gitignore", "*deploys*.txt\nreview.txt\n");
    executable(&work, "scripts/record.sh", RECORD_SH);
    set_version(&work, version);
    commit_all(&work, "Start");
    git(&work, &["push", "-q", "origin", "main"]);
    (root, work)
}

/// A second clone of the same origin, standing in for another agent.
pub fn other_clone(root: &Path, name: &str) -> PathBuf {
    let origin = root.join("origin.git");
    git(root, &["clone", "-q", origin.to_str().expect("utf8"), name]);
    root.join(name)
}

pub fn line_with_version(work: &Path, line: &str, version: &str) -> String {
    let branch = format!("release/{line}");
    git(work, &["checkout", "-q", "-b", &branch]);
    set_version(work, version);
    let c = commit_all(work, &format!("Start the {line} line"));
    git(work, &["push", "-q", "origin", &branch]);
    git(work, &["checkout", "-q", "main"]);
    c
}

/// A directory with a fake `gh` that records its arguments and `GH_HOST`, and the
/// `[run] path` line that puts it first.
pub fn fake_gh(root: &Path) -> String {
    let bin = root.join("bin");
    executable(
        &bin,
        "gh",
        "#!/bin/sh\nprintf 'GH_HOST=%s TOKEN=%s %s\\n' \"$GH_HOST\" \"$GH_ENTERPRISE_TOKEN\" \"$*\" >> \"$(dirname \"$0\")/gh-calls.log\"\n",
    );
    format!("\n[run]\npath = \"{}:/usr/bin:/bin\"\n", bin.display())
}

/// Gives origin a forge-style URL that git rewrites to the local bare repository, as a
/// real checkout of a hosted repository would have.
pub fn forge_alias(root: &Path, work: &Path) {
    let origin = root.join("origin.git");
    git(work, &["config", "remote.origin.url", "https://github.com/example/lib.git"]);
    git(work, &["config", &format!("url.{}.insteadOf", origin.display()), "https://github.com/example/lib.git"]);
}

/// The ledger as last published to origin (empty when nothing was recorded).
pub fn ledger_text(work: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(work)
        .args(["show", "refs/edition/ledger:ledger.toml"])
        .output()
        .expect("git");
    let local = String::from_utf8_lossy(&out.stdout).to_string();
    let origin = work.parent().map(|r| r.join("origin.git"));
    let remote = origin
        .and_then(|o| {
            Command::new("git").arg("--git-dir").arg(o).args(["show", "edition/ledger:ledger.toml"]).output().ok()
        })
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    if remote.is_empty() {
        local
    } else {
        remote
    }
}
