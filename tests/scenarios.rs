//! Scenario tests against temporary git repositories. A bare repository stands in for
//! each remote, so nothing here touches the network.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn all(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let d = std::env::temp_dir().join(format!("edition-test-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&d).expect("tmp dir");
    d.canonicalize().expect("canonical")
}

fn git(dir: &Path, args: &[&str]) -> String {
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

fn edition(dir: &Path, args: &[&str]) -> Out {
    let out = Command::new(env!("CARGO_BIN_EXE_edition"))
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .env("EDITION_NOW", "2026-11-23T12:00:00Z")
        .env("EDITION_ACTOR", "test-agent")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("edition");
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

fn write(dir: &Path, path: &str, text: &str) {
    let p = dir.join(path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(p, text).expect("write");
}

fn commit_all(dir: &Path, msg: &str) -> String {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

const BASE_CONFIG: &str = r#"
product = "lib"

[[version_files]]
path = "Cargo.toml"
after = "[package]"
pattern = 'version = "{version}"'

[[version_files]]
path = "pkg/package.json"
pattern = '"version": "{version}"'

[checks.unit]
run = ["sh", "-c", "exit 0"]

[channels.local]
tier = "dev"
kind = "home"
target = "http://localhost:4795"
run = ["sh", "-c", "echo {commit} >> dev-deploys.txt"]

[channels.staging-home]
tier = "staging"
kind = "home"
target = "https://staging.example.com"
checks = ["unit"]
run = ["sh", "-c", "echo {commit} >> staging-deploys.txt"]
probe = ["sh", "-c", "exit 0"]

[channels.production-home]
tier = "prod"
kind = "home"
target = "https://home.example.com"
approval = "owner"
run = ["sh", "-c", "echo {tag} >> prod-deploys.txt"]

[channels.store-review]
tier = "prod"
kind = "testflight"
target = "external testers"
allow_prerelease = true
run = ["sh", "-c", "echo {tag} >> review.txt"]
"#;

fn set_version(dir: &Path, v: &str) {
    write(dir, "Cargo.toml", &format!("[package]\nname = \"lib\"\nversion = \"{v}\"\n"));
    write(dir, "pkg/package.json", &format!("{{\n  \"name\": \"lib\",\n  \"version\": \"{v}\"\n}}\n"));
}

/// A work clone of a bare origin with main at `version`, pushed.
fn project(name: &str, config: &str, version: &str) -> (PathBuf, PathBuf) {
    let root = tmp(name);
    let origin = root.join("origin.git");
    git(&root, &["init", "-q", "--bare", origin.to_str().expect("utf8")]);
    let work = root.join("work");
    git(&root, &["clone", "-q", origin.to_str().expect("utf8"), "work"]);
    git(&work, &["checkout", "-q", "-b", "main"]);
    write(&work, "edition.toml", config);
    write(&work, ".gitignore", "*-deploys.txt\nreview.txt\n");
    set_version(&work, version);
    commit_all(&work, "Start");
    git(&work, &["push", "-q", "origin", "main"]);
    (root, work)
}

fn line_with_version(work: &Path, line: &str, version: &str) -> String {
    let branch = format!("release/{line}");
    git(work, &["checkout", "-q", "-b", &branch]);
    set_version(work, version);
    let c = commit_all(work, &format!("Start the {line} line"));
    git(work, &["push", "-q", "origin", &branch]);
    git(work, &["checkout", "-q", "main"]);
    c
}

#[test]
fn cut_rc_and_tag_follow_the_line_model() {
    let (_root, work) = project("cut", BASE_CONFIG, "0.6.0-dev");

    let dry = edition(&work, &["cut", "0.6"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains("[local] create release/0.6"), "{}", dry.stdout);
    assert!(dry.stdout.contains("[OUTWARD] publish release/0.6"));
    assert!(dry.stdout.contains("--apply --confirm push"));
    assert!(!work.join("releases/ledger.toml").exists());

    let unconfirmed = edition(&work, &["cut", "0.6", "--apply"]);
    assert_eq!(unconfirmed.code, 2, "{}", unconfirmed.all());
    assert!(unconfirmed.stderr.contains("--confirm push"));
    assert!(git(&work, &["branch", "--list", "release/0.6"]).is_empty(), "nothing runs without confirmation");

    let applied = edition(&work, &["cut", "0.6", "--apply", "--confirm", "push"]);
    assert_eq!(applied.code, 0, "{}", applied.all());
    git(&work, &["fetch", "-q", "origin"]);
    assert!(!git(&work, &["branch", "-r", "--list", "origin/release/0.6"]).is_empty());
    let ledger = std::fs::read_to_string(work.join("releases/ledger.toml")).expect("ledger");
    assert!(ledger.contains("action = \"push-branch\""));
    assert!(ledger.contains("by = \"test-agent\""));

    let again = edition(&work, &["cut", "0.6"]);
    assert_eq!(again.code, 1);
    assert!(again.stderr.contains("already exists"));

    // The line still says 0.6.0-dev: an rc is refused until the line is bumped.
    let early = edition(&work, &["rc", "0.6.0"]);
    assert_eq!(early.code, 1, "{}", early.all());
    assert!(early.stderr.contains("say 0.6.0-dev, not 0.6.0"), "{}", early.stderr);

    git(&work, &["checkout", "-q", "release/0.6"]);
    let bump = edition(&work, &["bump", "0.6.0", "--apply"]);
    assert_eq!(bump.code, 0, "{}", bump.all());
    commit_all(&work, "Start the 0.6 line");
    git(&work, &["push", "-q", "origin", "release/0.6"]);
    git(&work, &["checkout", "-q", "main"]);

    let rc1 = edition(&work, &["rc", "0.6.0", "--apply", "--confirm", "push"]);
    assert_eq!(rc1.code, 0, "{}", rc1.all());
    assert!(rc1.stdout.contains("v0.6.0-rc.1"));
    let rc2 = edition(&work, &["rc", "0.6.0"]);
    assert!(rc2.stdout.contains("v0.6.0-rc.2"), "{}", rc2.stdout);

    let fin = edition(&work, &["tag", "0.6.0", "--apply", "--confirm", "push"]);
    assert_eq!(fin.code, 0, "{}", fin.all());
    let remote_tags = git(&work, &["ls-remote", "--tags", "origin"]);
    assert!(remote_tags.contains("refs/tags/v0.6.0-rc.1"));
    assert!(
        remote_tags.contains("refs/tags/v0.6.0\n")
            || remote_tags.ends_with("refs/tags/v0.6.0")
            || remote_tags.contains("refs/tags/v0.6.0^{}")
    );
    let twice = edition(&work, &["tag", "0.6.0"]);
    assert_eq!(twice.code, 1);

    let wrong_line = edition(&work, &["tag", "0.7.0"]);
    assert_eq!(wrong_line.code, 1);
    assert!(wrong_line.stderr.contains("release/0.7 does not exist"));
}

#[test]
fn staging_takes_only_the_tip_of_main() {
    let (_root, work) = project("staging", BASE_CONFIG, "0.6.0-dev");
    let first = git(&work, &["rev-parse", "HEAD"]);
    write(&work, "a.txt", "a");
    let tip = commit_all(&work, "Second");
    git(&work, &["push", "-q", "origin", "main"]);

    git(&work, &["checkout", "-q", "-b", "feature"]);
    write(&work, "b.txt", "b");
    let feature = commit_all(&work, "Feature");
    git(&work, &["checkout", "-q", "main"]);

    let refused = edition(&work, &["deploy", "staging-home", &feature]);
    assert_eq!(refused.code, 1);
    assert!(refused.stderr.contains("not on main"), "{}", refused.stderr);

    let old = edition(&work, &["deploy", "staging-home", &first]);
    assert_eq!(old.code, 1);
    assert!(old.stderr.contains("an older main commit"), "{}", old.stderr);

    let dry = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains("[check] check unit"));
    assert!(dry.stdout.contains(&format!("echo {tip}")), "the exact command is printed: {}", dry.stdout);
    assert!(dry.stdout.contains("--confirm staging-home"));

    let wrong_token = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", "push"]);
    assert_eq!(wrong_token.code, 2);
    assert!(!work.join("staging-deploys.txt").exists());

    let ok = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", "staging-home"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert_eq!(std::fs::read_to_string(work.join("staging-deploys.txt")).expect("deployed").trim(), tip);
    let ledger = std::fs::read_to_string(work.join("releases/ledger.toml")).expect("ledger");
    assert!(ledger.contains("tier = \"staging\""));
    assert!(ledger.contains(&format!("commit = \"{tip}\"")));
    assert!(ledger.contains("check unit: exit 0"));
    assert!(ledger.contains("version = \"0.6.0-dev\""));

    let status = edition(&work, &["status"]);
    assert!(status.stdout.contains("staging-home"));
    assert!(status.stdout.contains(&tip[..10]));
}

#[test]
fn dev_tier_runs_locally_and_records_nothing() {
    let (_root, work) = project("dev", BASE_CONFIG, "0.6.0-dev");
    git(&work, &["checkout", "-q", "-b", "wip"]);
    write(&work, "w.txt", "w");
    let wip = commit_all(&work, "WIP");
    let out = edition(&work, &["deploy", "local", "wip", "--apply"]);
    assert_eq!(out.code, 0, "{}", out.all());
    assert_eq!(std::fs::read_to_string(work.join("dev-deploys.txt")).expect("ran").trim(), wip);
    assert!(!work.join("releases/ledger.toml").exists(), "dev deploys are never recorded");
}

#[test]
fn prod_needs_a_final_tag_on_the_remote_and_an_approval() {
    let (_root, work) = project("prod", BASE_CONFIG, "0.7.0-dev");
    let line = line_with_version(&work, "0.6", "0.6.0");

    let untagged = edition(&work, &["deploy", "production-home", "origin/release/0.6", "--approved-by", "owner"]);
    assert_eq!(untagged.code, 1);
    assert!(untagged.stderr.contains("no release tag points at"), "{}", untagged.stderr);

    let main_tip = edition(&work, &["deploy", "production-home", "main", "--approved-by", "owner"]);
    assert_eq!(main_tip.code, 1);

    git(&work, &["tag", "-a", "v0.6.0-rc.1", &line, "-m", "rc"]);
    git(&work, &["push", "-q", "origin", "v0.6.0-rc.1"]);
    let rc_home = edition(&work, &["deploy", "production-home", "v0.6.0-rc.1", "--approved-by", "owner", "--online"]);
    assert_eq!(rc_home.code, 1);
    assert!(rc_home.stderr.contains("pre-release tag"), "{}", rc_home.stderr);
    let rc_review =
        edition(&work, &["deploy", "store-review", "v0.6.0-rc.1", "--online", "--apply", "--confirm", "store-review"]);
    assert_eq!(rc_review.code, 0, "{}", rc_review.all());

    // A final tag that exists only locally is refused once the remote is read.
    git(&work, &["tag", "-a", "v0.6.0", &line, "-m", "final"]);
    let local_only = edition(&work, &["deploy", "production-home", "v0.6.0", "--approved-by", "owner", "--online"]);
    assert_eq!(local_only.code, 1);
    assert!(local_only.stderr.contains("is not on origin"), "{}", local_only.stderr);

    git(&work, &["push", "-q", "origin", "v0.6.0"]);
    let no_approval = edition(&work, &["deploy", "production-home", "v0.6.0", "--online"]);
    assert_eq!(no_approval.code, 1);
    assert!(no_approval.stderr.contains("--approved-by"));

    let ok = edition(
        &work,
        &["deploy", "production-home", "v0.6.0", "--approved-by", "owner", "--apply", "--confirm", "production-home"],
    );
    assert_eq!(ok.code, 0, "{}", ok.all());
    let ledger = std::fs::read_to_string(work.join("releases/ledger.toml")).expect("ledger");
    assert!(ledger.contains("approved_by = \"owner\""));
    assert!(ledger.contains("ref = \"v0.6.0\""));

    let verify = edition(&work, &["verify", "--online"]);
    assert!(verify.stdout.contains("production-home holds v0.6.0"), "{}", verify.stdout);
}

#[test]
fn promote_to_prod_requires_the_held_commit_to_be_tagged() {
    let cfg = BASE_CONFIG.replace("[channels.staging-home]", "[channels.line-staging]\ntier = \"staging\"\nkind = \"home\"\ntarget = \"x\"\nrun = [\"true\"]\n\n[channels.staging-home]");
    let (_root, work) = project("promote", &cfg, "0.6.0");
    // Here main itself carries 0.6.0 (a project whose line was cut at the tip).
    let ok = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", "staging-home"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let refused = edition(&work, &["promote", "staging-home", "production-home", "--approved-by", "owner"]);
    assert_eq!(refused.code, 1);
    assert!(refused.stderr.contains("carries no release tag"), "{}", refused.stderr);

    git(&work, &["checkout", "-q", "-b", "release/0.6"]);
    git(&work, &["push", "-q", "origin", "release/0.6"]);
    git(&work, &["checkout", "-q", "main"]);
    git(&work, &["tag", "-a", "v0.6.0", "HEAD", "-m", "final"]);
    git(&work, &["push", "-q", "origin", "v0.6.0"]);
    let promoted = edition(&work, &["promote", "staging-home", "production-home", "--approved-by", "owner"]);
    assert_eq!(promoted.code, 0, "{}", promoted.all());
    assert!(promoted.stdout.contains("deploying v0.6.0 to production-home"));
}

#[test]
fn verify_finds_unmerged_lines_and_forward_offers_a_pull_request() {
    let (_root, work) = project("forward", BASE_CONFIG, "0.6.0-dev");
    line_with_version(&work, "0.5", "0.5.2");
    git(&work, &["checkout", "-q", "release/0.5"]);
    write(&work, "fix.txt", "fix");
    commit_all(&work, "Hotfix on the line");
    git(&work, &["push", "-q", "origin", "release/0.5"]);
    git(&work, &["checkout", "-q", "main"]);

    let v = edition(&work, &["verify"]);
    assert_eq!(v.code, 1, "{}", v.all());
    assert!(v.stdout.contains("origin/release/0.5 has 2 commit(s) not merged forward into main"), "{}", v.stdout);

    let fwd = edition(&work, &["forward"]);
    assert_eq!(fwd.code, 0, "{}", fwd.all());
    assert!(fwd.stdout.contains("gh pr create --base main --head release/0.5"), "{}", fwd.stdout);
    assert!(fwd.stdout.contains("--confirm pr"));

    // Merge forward (keeping main's version) and the check passes for the line.
    git(&work, &["merge", "-q", "--no-ff", "-s", "ours", "origin/release/0.5", "-m", "Merge release/0.5 forward"]);
    git(&work, &["push", "-q", "origin", "main"]);
    let v2 = edition(&work, &["verify"]);
    assert!(v2.stdout.contains("origin/release/0.5 is merged forward into main"), "{}", v2.stdout);
    assert!(v2.stdout.contains("origin/main builds as 0.6.0-dev"));
    assert_eq!(v2.code, 0, "{}", v2.all());
}

#[test]
fn hotfix_start_names_the_next_patch_and_finish_tags_it() {
    let (_root, work) = project("hotfix", BASE_CONFIG, "0.6.0-dev");
    let line = line_with_version(&work, "0.5", "0.5.2");
    git(&work, &["tag", "-a", "v0.5.2", &line, "-m", "0.5.2"]);
    git(&work, &["push", "-q", "origin", "v0.5.2"]);

    let start = edition(&work, &["hotfix", "start", "0.5", "--apply"]);
    assert_eq!(start.code, 0, "{}", start.all());
    assert!(start.stdout.contains("hotfix/0.5.3"));
    assert!(!git(&work, &["branch", "--list", "hotfix/0.5.3"]).is_empty());
    assert!(start.stdout.contains("never into main"));

    git(&work, &["checkout", "-q", "hotfix/0.5.3"]);
    set_version(&work, "0.5.3");
    commit_all(&work, "Fix and bump to 0.5.3");
    git(&work, &["push", "-q", "origin", "hotfix/0.5.3:release/0.5"]);
    git(&work, &["checkout", "-q", "main"]);

    let finish = edition(&work, &["hotfix", "finish", "0.5.3"]);
    assert_eq!(finish.code, 0, "{}", finish.all());
    assert!(finish.stdout.contains("v0.5.3"));
    assert!(finish.stdout.contains("gh pr create --base main --head release/0.5"));
}

#[test]
fn a_pinned_commit_on_no_remote_is_detected() {
    let root = tmp("pins");
    // The library and its remote.
    let lib_origin = root.join("lib.git");
    git(&root, &["init", "-q", "--bare", lib_origin.to_str().expect("utf8")]);
    git(&root, &["clone", "-q", lib_origin.to_str().expect("utf8"), "lib"]);
    let lib = root.join("lib");
    git(&lib, &["checkout", "-q", "-b", "main"]);
    write(&lib, "lib.txt", "1");
    commit_all(&lib, "Lib");
    git(&lib, &["push", "-q", "origin", "main"]);
    git(&lib, &["checkout", "-q", "-b", "local-only"]);
    write(&lib, "lib.txt", "2");
    let unpushed = commit_all(&lib, "Only here");

    // The app, pinning that commit.
    let app_origin = root.join("app.git");
    git(&root, &["init", "-q", "--bare", app_origin.to_str().expect("utf8")]);
    git(&root, &["clone", "-q", app_origin.to_str().expect("utf8"), "app"]);
    let app = root.join("app");
    git(&app, &["checkout", "-q", "-b", "main"]);
    let cfg = r#"
product = "app"

[repos.lib]
path = "../lib"

[[pins]]
name = "lib-sdk"
file = "tools/sdk.sh"
pattern = "lib_commit={commit}"
repo = "lib"

[channels.store]
tier = "prod"
kind = "testflight"
target = "app store"
run = ["true"]
"#;
    write(&app, "edition.toml", cfg);
    write(&app, "tools/sdk.sh", &format!("#!/bin/sh\nlib_commit={unpushed}\n"));
    commit_all(&app, "App");
    git(&app, &["push", "-q", "origin", "main"]);

    let v = edition(&app, &["verify"]);
    assert_eq!(v.code, 1, "{}", v.all());
    assert!(v.stdout.contains("pin lib-sdk:"), "{}", v.stdout);
    assert!(v.stdout.contains("exists only in a local checkout"), "{}", v.stdout);

    // On a remote branch but untagged: still not acceptable for production.
    git(&lib, &["push", "-q", "origin", "local-only"]);
    let v2 = edition(&app, &["verify", "--online"]);
    assert_eq!(v2.code, 1);
    assert!(v2.stdout.contains("is not a release tag commit"), "{}", v2.stdout);

    // A final tag that is only local is caught online.
    git(&lib, &["tag", "-a", "v1.0.0", &unpushed, "-m", "1.0.0"]);
    let v3 = edition(&app, &["verify", "--online"]);
    assert_eq!(v3.code, 1);
    assert!(v3.stdout.contains("is not a release tag commit"), "{}", v3.stdout);

    git(&lib, &["push", "-q", "origin", "v1.0.0"]);
    let v4 = edition(&app, &["verify", "--online"]);
    assert!(v4.stdout.contains("pin lib-sdk is release tag v1.0.0"), "{}", v4.stdout);
    assert!(!v4.stdout.contains("ERROR pin"), "{}", v4.stdout);
}

#[test]
fn the_ledger_is_append_only() {
    let (_root, work) = project("ledger", BASE_CONFIG, "0.6.0-dev");
    let ok = edition(
        &work,
        &["deploy", "staging-home", "main", "--apply", "--confirm", "staging-home", "--note", "first \"quoted\" note"],
    );
    assert_eq!(ok.code, 0, "{}", ok.all());
    let listed = edition(&work, &["ledger"]);
    assert!(listed.stdout.contains("deploy"));
    assert!(listed.stdout.contains("staging-home"));
    commit_all(&work, "Record the staging deploy");
    git(&work, &["push", "-q", "origin", "main"]);

    let v = edition(&work, &["verify"]);
    assert!(v.stdout.contains("releases/ledger.toml is append-only against HEAD"), "{}", v.stdout);

    // A second deploy appends; editing a committed entry is caught.
    let ok2 = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", "staging-home"]);
    assert_eq!(ok2.code, 0, "{}", ok2.all());
    let v2 = edition(&work, &["verify"]);
    assert!(v2.stdout.contains("append-only against HEAD"));
    let text = std::fs::read_to_string(work.join("releases/ledger.toml")).expect("ledger");
    assert_eq!(text.matches("[[entry]]").count(), 2);
    write(&work, "releases/ledger.toml", &text.replacen("by = \"test-agent\"", "by = \"someone else\"", 1));
    let v3 = edition(&work, &["verify"]);
    assert_eq!(v3.code, 1);
    assert!(v3.stdout.contains("was edited"), "{}", v3.stdout);
}

#[test]
fn tier_policy_cannot_be_loosened_in_config() {
    let cfg = BASE_CONFIG.replace(
        "[channels.staging-home]\ntier = \"staging\"",
        "[channels.staging-home]\ntier = \"staging\"\napproval = \"owner\"\nallow_prerelease = true",
    );
    let (_root, work) = project("config", BASE_CONFIG, "0.6.0-dev");
    write(&work, "edition.toml", &cfg);
    let out = edition(&work, &["status"]);
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("never takes an approval"), "{}", out.stderr);
    assert!(out.stderr.contains("allow_prerelease is only meaningful on the prod tier"));
}

#[test]
fn tags_must_agree_with_version_files_and_main_builds_as_dev() {
    let (_root, work) = project("versions", BASE_CONFIG, "0.6.0");
    let v = edition(&work, &["verify"]);
    assert_eq!(v.code, 1);
    assert!(
        v.stdout.contains("origin/main builds as 0.6.0; main should build as the next version with -dev"),
        "{}",
        v.stdout
    );

    set_version(&work, "0.6.0-dev");
    write(&work, "pkg/package.json", "{\n  \"version\": \"0.5.9\"\n}\n");
    commit_all(&work, "Disagree");
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["tag", "-a", "v0.6.0", "HEAD", "-m", "bad tag"]);
    let v2 = edition(&work, &["verify"]);
    assert_eq!(v2.code, 1);
    assert!(v2.stdout.contains("version files disagree"), "{}", v2.stdout);
}

#[test]
fn content_check_reports_internal_notes_bound_for_a_public_remote() {
    let cfg = format!("{BASE_CONFIG}\n[remotes.origin]\npublic = true\n");
    let (_root, work) = project("content", &cfg, "0.6.0-dev");
    write(&work, "docs/plan.md", "# Plan\n\n## Decisions for Alex\n\nSee /Users/someone/notes.\n");
    commit_all(&work, "Plan");

    let check = edition(&work, &["check-push", "origin"]);
    assert_eq!(check.code, 0, "report is the default: {}", check.all());
    assert!(check.stderr.contains("Decisions for <name>"), "{}", check.stderr);
    assert!(check.stderr.contains("/Users/<name>/"));

    let v = edition(&work, &["verify"]);
    assert!(v.stdout.contains("content:"), "{}", v.stdout);

    let blocking = format!("{cfg}\n[content]\naction = \"block\"\nallow_paths = [\"docs/allowed/**\"]\n");
    write(&work, "edition.toml", &blocking);
    commit_all(&work, "Block");
    let blocked = edition(&work, &["check-push", "origin"]);
    assert_eq!(blocked.code, 1, "{}", blocked.all());

    let private = edition(&work, &["check-push", "elsewhere"]);
    assert_eq!(private.code, 0);
    assert!(private.stdout.contains("not marked public"));
}

#[test]
fn concordance_builds_must_match_the_ledger() {
    let root = tmp("concordance-bin");
    let fake = root.join("concordance");
    // Stands in for `concordance track <dir> --format json --out <file>`.
    write(
        &root,
        "concordance",
        "#!/bin/sh\n[ \"$1\" = log ] && exit 0\nwhile [ $# -gt 0 ]; do case \"$1\" in --out) shift; out=\"$1\";; esac; shift; done\nprintf '%s' '{\"releases\":[{\"id\":\"0.6.0\",\"state\":\"candidate\",\"builds\":{\"ios\":\"41\"},\"carries\":[{\"id\":\"sync.join\",\"ready\":true},{\"id\":\"bug.x\",\"ready\":false}]}]}' > \"$out\"\n",
    );
    Command::new("chmod").arg("+x").arg(&fake).status().expect("chmod");
    let cfg = format!(
        "{BASE_CONFIG}\n[concordance]\ncatalog = \"spec/app\"\nbin = \"{}\"\n\n[channels.testflight-internal]\ntier = \"staging\"\nkind = \"testflight\"\ntarget = \"internal\"\nplatform = \"ios\"\nrun = [\"true\"]\n\n[platforms.ios]\n",
        fake.display()
    );
    let (_root, work) = project("concordance", &cfg, "0.6.0-dev");

    let plan = edition(&work, &["plan", "0.6.0"]);
    assert!(plan.stdout.contains("Concordance 0.6.0: candidate"), "{}", plan.stdout);
    assert!(plan.stdout.contains("1/2 carried items ready"));
    assert!(plan.stdout.contains("not ready: bug.x"));

    let need_build = edition(&work, &["deploy", "testflight-internal", "main"]);
    assert_eq!(need_build.code, 1);
    assert!(need_build.stderr.contains("--build"));

    let dry = edition(&work, &["deploy", "testflight-internal", "main", "--build", "42"]);
    assert!(dry.stdout.contains("log uploaded 0.6.0 --platform ios --build 42"), "{}", dry.stdout);

    // Record the upload without running the fake's `log` (it ignores it), then verify.
    let ran = edition(
        &work,
        &["deploy", "testflight-internal", "main", "--build", "42", "--apply", "--confirm", "testflight-internal"],
    );
    assert_eq!(ran.code, 0, "{}", ran.all());
    let v = edition(&work, &["verify"]);
    assert!(v.stdout.contains("Concordance 0.6.0 says ios build 41, the ledger says 42"), "{}", v.stdout);
}

/// A fake `concordance` whose `track` reports release 0.6.0 with the given readiness.
fn fake_concordance(dir: &Path, all_ready: bool) -> PathBuf {
    let ready = if all_ready { "true" } else { "false" };
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = log ] && {{ echo \"$@\" >> \"$(dirname \"$0\")/log-calls.txt\"; exit 0; }}\nwhile [ $# -gt 0 ]; do case \"$1\" in --out) shift; out=\"$1\";; esac; shift; done\nprintf '%s' '{{\"releases\":[{{\"id\":\"1.2.0\",\"state\":\"candidate\",\"builds\":{{}},\"carries\":[{{\"id\":\"sync.join\",\"ready\":true}},{{\"id\":\"player.handoff\",\"ready\":{ready}}}]}}]}}' > \"$out\"\n"
    );
    write(dir, "concordance", &script);
    let path = dir.join("concordance");
    Command::new("chmod").arg("+x").arg(&path).status().expect("chmod");
    path
}

fn app_config(bin: &Path) -> String {
    format!(
        r#"
product = "app"

[[version_files]]
path = "ios/version.txt"
pattern = "v={{version}}"

[[version_files]]
path = "android/build.gradle.kts"
pattern = 'versionName = "{{version}}"'

[platforms.ios]
build_file = "ios/version.txt"
build_pattern = "build={{build}}"

[platforms.android]
build_file = "android/build.gradle.kts"
build_pattern = "versionCode = {{build}}"

[concordance]
catalog = "spec/app"
bin = "{}"

[checks.parity]
kind = "parity"

[channels.testflight-internal]
tier = "staging"
kind = "testflight"
target = "internal testers"
platform = "ios"
run = ["true"]

[channels.play-internal]
tier = "staging"
kind = "play"
target = "internal track"
platform = "android"
run = ["true"]

[channels.testflight-external]
tier = "prod"
kind = "testflight"
target = "external testers"
platform = "ios"
allow_prerelease = true
run = ["true"]

[channels.app-store]
tier = "prod"
kind = "app-store"
target = "App Store"
platform = "ios"
checks = ["parity"]
requires = ["testflight-external"]
run = ["true"]
"#,
        bin.display()
    )
}

fn set_app_version(dir: &Path, v: &str, ios_build: u32, android_build: u32) {
    write(dir, "ios/version.txt", &format!("v={v}\nbuild={ios_build}\n"));
    write(
        dir,
        "android/build.gradle.kts",
        &format!("android {{\n  versionCode = {android_build}\n  versionName = \"{v}\"\n}}\n"),
    );
}

#[test]
fn one_version_many_platforms_with_gates_before_production() {
    let root = tmp("platforms-bin");
    let bin = fake_concordance(&root, false);
    let (_r, work) = project("platforms", &app_config(&bin), "0.0.0");
    // project() wrote generic version files; replace them with the app's.
    std::fs::remove_file(work.join("Cargo.toml")).expect("rm");
    std::fs::remove_dir_all(work.join("pkg")).expect("rm");
    set_app_version(&work, "1.2.0", 51, 134);
    let tip = commit_all(&work, "App 1.2.0");
    git(&work, &["push", "-q", "origin", "main"]);

    // Staging takes main; the build number comes from each platform's own file.
    let events = root.join("events.jsonl");
    let ios = Command::new(env!("CARGO_BIN_EXE_edition"))
        .args(["deploy", "testflight-internal", "main", "--apply", "--confirm", "testflight-internal"])
        .current_dir(&work)
        .stdin(Stdio::null())
        .env("EDITION_NOW", "2026-11-23T12:00:00Z")
        .env("EDITION_ACTOR", "test-agent")
        .env("EDITION_EVENTS", &events)
        .output()
        .expect("run");
    assert!(ios.status.success(), "{}", String::from_utf8_lossy(&ios.stderr));
    let calls = std::fs::read_to_string(root.join("log-calls.txt")).expect("concordance log called");
    assert!(calls.contains("log uploaded 1.2.0 --platform ios --build 51"), "{calls}");
    let ev = std::fs::read_to_string(&events).expect("events");
    assert!(ev.contains("\"event\":\"channel-updated\""), "{ev}");
    assert!(ev.contains("\"channel\":\"testflight-internal\""));

    let android = edition(&work, &["deploy", "play-internal", "main", "--apply", "--confirm", "play-internal"]);
    assert_eq!(android.code, 0, "{}", android.all());

    let status = edition(&work, &["status"]);
    assert!(status.stdout.contains("1.2.0 (51)"), "{}", status.stdout);
    assert!(status.stdout.contains("1.2.0 (134)"), "{}", status.stdout);

    // Production: a release line and tag, then the gates.
    git(&work, &["branch", "release/1.2", &tip]);
    git(&work, &["push", "-q", "origin", "release/1.2"]);
    git(&work, &["tag", "-a", "v1.2.0-rc.1", &tip, "-m", "1.2.0 rc 1"]);
    git(&work, &["push", "-q", "origin", "v1.2.0-rc.1"]);
    let review = edition(
        &work,
        &["deploy", "testflight-external", "v1.2.0-rc.1", "--apply", "--confirm", "testflight-external"],
    );
    assert_eq!(review.code, 0, "{}", review.all());
    git(&work, &["tag", "-a", "v1.2.0", &tip, "-m", "1.2.0"]);
    git(&work, &["push", "-q", "origin", "v1.2.0"]);
    let gap = edition(&work, &["deploy", "app-store", "v1.2.0"]);
    assert_eq!(gap.code, 1, "{}", gap.all());
    assert!(
        gap.stderr.contains("gate parity: Concordance 1.2.0: 1 of 2 carried items not ready: player.handoff"),
        "{}",
        gap.stderr
    );

    fake_concordance(&root, true);
    let ok = edition(&work, &["deploy", "app-store", "v1.2.0", "--apply", "--confirm", "app-store"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert!(ok.stdout.contains("gate: testflight-external already holds"));
    let ledger = std::fs::read_to_string(work.join("releases/ledger.toml")).expect("ledger");
    assert!(ledger.contains("gate parity: Concordance 1.2.0: 2/2 carried items ready on every platform"), "{ledger}");
    assert!(ledger.contains("platform = \"ios\""));

    // Version sources must agree across platforms.
    write(&work, "android/build.gradle.kts", "android {\n  versionCode = 135\n  versionName = \"1.2.1\"\n}\n");
    commit_all(&work, "Android drifts");
    git(&work, &["push", "-q", "origin", "main"]);
    let v = edition(&work, &["verify"]);
    assert!(v.stdout.contains("version files disagree"), "{}", v.stdout);
}

#[test]
fn requires_gate_refuses_a_commit_staging_never_held() {
    let root = tmp("requires-bin");
    let bin = fake_concordance(&root, true);
    let (_r, work) = project("requires", &app_config(&bin), "0.0.0");
    std::fs::remove_file(work.join("Cargo.toml")).expect("rm");
    std::fs::remove_dir_all(work.join("pkg")).expect("rm");
    set_app_version(&work, "1.2.0", 51, 134);
    let tip = commit_all(&work, "App 1.2.0");
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["branch", "release/1.2", &tip]);
    git(&work, &["push", "-q", "origin", "release/1.2"]);
    git(&work, &["tag", "-a", "v1.2.0", &tip, "-m", "1.2.0"]);
    git(&work, &["push", "-q", "origin", "v1.2.0"]);
    let out = edition(&work, &["deploy", "app-store", "v1.2.0"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stderr.contains("gate: testflight-external has not held"), "{}", out.stderr);
}

#[test]
fn hooks_install_writes_a_pre_push_hook_once() {
    let (_root, work) = project("hooks", BASE_CONFIG, "0.6.0-dev");
    let dry = edition(&work, &["hooks", "install"]);
    assert_eq!(dry.code, 0);
    assert!(!work.join(".git/hooks/pre-push").exists());
    let ok = edition(&work, &["hooks", "install", "--apply"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let hook = std::fs::read_to_string(work.join(".git/hooks/pre-push")).expect("hook");
    assert!(hook.contains("edition check-push"));
    let again = edition(&work, &["hooks", "install", "--apply"]);
    assert!(again.stdout.contains("already installed"));
    write(&work, ".git/hooks/pre-push", "#!/bin/sh\necho mine\n");
    let foreign = edition(&work, &["hooks", "install", "--apply"]);
    assert_eq!(foreign.code, 1);
}
