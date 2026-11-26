//! Holding a release while more work for its line is in flight: open pull requests,
//! changelog entries and version files that would replace the release right away.

mod common;
use common::*;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A project whose origin looks like a GitHub repository, with a fake gh, and a 0.5
/// line that carries a fix bumped to 0.5.7 on top of v0.5.6. Returns the work clone
/// and the fake gh's directory.
fn line_ready(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let bins = tmp(&format!("{name}-bin"));
    let cfg = format!("{BASE_CONFIG}{}", fake_gh(&bins));
    let (root, work) = project(name, &cfg, "0.6.0-dev");
    forge_alias(&root, &work);
    let line = line_with_version(&work, "0.5", "0.5.6");
    git(&work, &["tag", "-a", "v0.5.6", &line, "-m", "0.5.6"]);
    git(&work, &["push", "-q", "origin", "v0.5.6"]);
    git(&work, &["checkout", "-q", "release/0.5"]);
    set_version(&work, "0.5.7");
    write(&work, "fix.md", "fix");
    commit_all(&work, "Fix and bump to 0.5.7");
    git(&work, &["push", "-q", "origin", "release/0.5"]);
    git(&work, &["checkout", "-q", "main"]);
    (root, work, bins.join("bin"))
}

fn open_prs(bin: &Path, prs: &[(u64, &str, &str, &str)]) {
    let list: Vec<String> = prs
        .iter()
        .map(|(n, head, base, title)| {
            format!(r#"{{"number":{n},"headRefName":"{head}","baseRefName":"{base}","title":"{title}"}}"#)
        })
        .collect();
    write(bin, "prs.json", &format!("[{}]\n", list.join(",")));
}

/// Commits `text` as the changelog on the 0.5 line and pushes it.
fn changelog_on_line(work: &Path, text: &str) {
    git(work, &["checkout", "-q", "release/0.5"]);
    write(work, "CHANGELOG.md", text);
    commit_all(work, "Changelog");
    git(work, &["push", "-q", "origin", "release/0.5"]);
    git(work, &["checkout", "-q", "main"]);
}

#[test]
fn hotfix_finish_refuses_while_a_pull_request_into_the_line_is_open_and_names_it() {
    let (_root, work, bin) = line_ready("inflight-pr");
    open_prs(
        &bin,
        &[
            (12, "fix-export", "release/0.5", "Fix the export"),
            (13, "other", "release/0.6", "Another line"),
            (14, "feature", "main", "Next version"),
        ],
    );

    let out = edition(&work, &["hotfix", "finish", "0.5.7"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stderr.contains("more work is heading for the 0.5 line"), "{}", out.stderr);
    assert!(out.stderr.contains("open pull request #12 fix-export -> release/0.5: Fix the export"), "{}", out.stderr);
    assert!(out.stderr.contains("--allow-in-flight #12"), "{}", out.stderr);
    assert!(!out.stderr.contains("#13") && !out.stderr.contains("#14"), "{}", out.stderr);
    assert!(!out.stdout.contains("Receipt: "), "no receipt for a refused plan");
    let calls = std::fs::read_to_string(bin.join("gh-calls.log")).expect("gh ran");
    assert!(calls.contains("GH_HOST=github.com TOKEN= pr list --repo github.com/example/lib --state open"), "{calls}");
    assert!(!git(&work, &["ls-remote", "--tags", "origin"]).contains("v0.5.7"));

    // The same holds for rc and tag.
    for cmd in [&["rc", "0.5.7"][..], &["tag", "0.5.7"][..]] {
        let out = edition(&work, cmd);
        assert_eq!(out.code, 1, "{cmd:?}: {}", out.all());
        assert!(out.stderr.contains("#12 fix-export"), "{cmd:?}: {}", out.stderr);
    }
}

#[test]
fn an_allowed_pull_request_lets_the_release_through_and_the_ledger_records_it() {
    let (_root, work, bin) = line_ready("inflight-allow");
    open_prs(&bin, &[(12, "fix-export", "release/0.5", "Fix the export")]);

    let args = ["hotfix", "finish", "0.5.7", "--allow-in-flight", "12", "--note", "export fix needs another review"];
    let dry = edition(&work, &args);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains("allowing in-flight work: open pull request #12"), "{}", dry.stdout);
    let receipt = dry.receipt();

    // An allowance is bound in the receipt: the plan without it is refused.
    let without = edition(&work, &["hotfix", "finish", "0.5.7", "--apply", "--confirm", &receipt]);
    assert_ne!(without.code, 0, "{}", without.all());

    let ran = apply(&work, &args);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert!(git(&work, &["ls-remote", "--tags", "origin"]).contains("refs/tags/v0.5.7"));
    let ledger = ledger_text(&work);
    assert!(ledger.contains("in-flight #12 allowed for v0.5.7"), "{ledger}");
    assert!(ledger.contains("by = \"test-agent\""), "{ledger}");
    assert!(ledger.contains("export fix needs another review"), "{ledger}");

    // An allowance must name something that is in flight.
    let stray = edition(&work, &["tag", "0.5.8", "--allow-in-flight", "99"]);
    assert_eq!(stray.code, 1, "{}", stray.all());
    assert!(stray.stderr.contains("--allow-in-flight 99 names nothing in flight for 0.5"), "{}", stray.stderr);
}

#[test]
fn a_pull_request_from_a_hotfix_branch_of_the_line_holds_the_release() {
    let (_root, work, bin) = line_ready("inflight-hotfix-branch");
    open_prs(&bin, &[(21, "hotfix/0.5.8", "main", "Second fix"), (22, "hotfix/0.4.9", "release/0.4", "Old line")]);
    let out = edition(&work, &["tag", "0.5.7"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stderr.contains("#21 hotfix/0.5.8 -> main"), "{}", out.stderr);
    assert!(!out.stderr.contains("#22"), "{}", out.stderr);

    let plan = edition(&work, &["plan", "0.5.7", "--online"]);
    assert_eq!(plan.code, 0, "{}", plan.all());
    assert!(plan.stdout.contains("#21"), "{}", plan.stdout);
    assert!(plan.stdout.contains("fold the in-flight work into 0.5.7"), "{}", plan.stdout);
}

#[test]
fn plan_reports_a_changelog_entry_for_the_next_patch_while_this_one_is_untagged() {
    let (_root, work, _bin) = line_ready("inflight-changelog");
    changelog_on_line(&work, "# Changelog\n\n## 0.5.8\n- a later fix\n\n## 0.5.7\n- the export fix\n");

    let plan = edition(&work, &["plan", "0.5.7"]);
    assert_eq!(plan.code, 0, "{}", plan.all());
    assert!(plan.stdout.contains("changelog:0.5.8"), "{}", plan.stdout);
    assert!(plan.stdout.contains("CHANGELOG.md has an entry for 0.5.8 while 0.5.7 is untagged (fold into 0.5.7)"));
    assert!(plan.stdout.contains("pull requests: not checked offline"), "{}", plan.stdout);

    let tag = edition(&work, &["tag", "0.5.7"]);
    assert_eq!(tag.code, 1, "{}", tag.all());
    assert!(tag.stderr.contains("--allow-in-flight changelog:0.5.8"), "{}", tag.stderr);
    let allowed = edition(&work, &["tag", "0.5.7", "--allow-in-flight", "changelog:0.5.8"]);
    assert_eq!(allowed.code, 0, "{}", allowed.all());
}

#[test]
fn unreleased_entries_and_later_version_files_are_in_flight() {
    let (_root, work, _bin) = line_ready("inflight-unreleased");
    changelog_on_line(&work, "# Changelog\n\n## [Unreleased]\n### Fixed\n- a crash on launch\n\n## [0.5.7]\n- fix\n");
    let plan = edition(&work, &["plan", "0.5.7"]);
    assert!(plan.stdout.contains("changelog:Unreleased"), "{}", plan.stdout);
    let rc = edition(&work, &["rc", "0.5.7"]);
    assert_eq!(rc.code, 1, "{}", rc.all());
    assert!(rc.stderr.contains("entries under \"Unreleased\""), "{}", rc.stderr);

    // An empty Unreleased section is not in flight.
    changelog_on_line(&work, "# Changelog\n\n## [Unreleased]\n\n## [0.5.7]\n- fix\n");
    let clear = edition(&work, &["plan", "0.5.7"]);
    assert!(clear.stdout.contains("in flight for 0.5: nothing"), "{}", clear.stdout);

    // The line already says 0.5.8 while 0.5.7 was never tagged.
    git(&work, &["checkout", "-q", "release/0.5"]);
    set_version(&work, "0.5.8");
    commit_all(&work, "Bump to 0.5.8");
    git(&work, &["push", "-q", "origin", "release/0.5"]);
    git(&work, &["checkout", "-q", "main"]);
    let later = edition(&work, &["plan", "0.5.7"]);
    assert!(later.stdout.contains("version:0.5.8"), "{}", later.stdout);
    assert!(later.stdout.contains("say 0.5.8 while 0.5.7 is untagged (fold into 0.5.7)"), "{}", later.stdout);
}

#[test]
fn the_check_fails_closed_when_the_forge_cannot_be_read() {
    let (_root, work, bin) = line_ready("inflight-offline");
    write(&bin, "gh-offline", "");

    let out = edition(&work, &["hotfix", "finish", "0.5.7"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stderr.contains("in-flight work for the 0.5 line cannot be checked"), "{}", out.stderr);
    assert!(out.stderr.contains("fail closed"), "{}", out.stderr);
    assert!(out.stderr.contains("error connecting to api.github.com"), "{}", out.stderr);
    // No allowance gets around it: there is nothing known to allow.
    let allowed = edition(&work, &["tag", "0.5.7", "--allow-in-flight", "12"]);
    assert_eq!(allowed.code, 1, "{}", allowed.all());

    let online = edition(&work, &["plan", "0.5.7", "--online"]);
    assert_eq!(online.code, 1, "{}", online.all());
    assert!(online.stdout.contains("cannot be checked"), "{}", online.stdout);
    // The offline plan reads nothing from the forge, and says so.
    let offline = edition(&work, &["plan", "0.5.7"]);
    assert_eq!(offline.code, 0, "{}", offline.all());
    assert!(offline.stdout.contains("pull requests: not checked offline"), "{}", offline.stdout);
}

fn tag_at(work: &Path, name: &str, rev: &str, date: &str) {
    let out = Command::new("git")
        .arg("-C")
        .arg(work)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "tag.gpgsign=false"])
        .args(["tag", "-a", name, rev, "-m", name])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .expect("git");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn verify_warns_about_a_release_superseded_within_a_week() {
    let (_root, work) = project("superseded", BASE_CONFIG, "0.6.0-dev");
    line_with_version(&work, "0.5", "0.5.6");
    git(&work, &["checkout", "-q", "release/0.5"]);
    tag_at(&work, "v0.5.6", "HEAD", "2026-11-01T12:00:00Z");
    for (v, date) in [("0.5.7", "2026-11-03T12:00:00Z"), ("0.5.8", "2026-11-20T12:00:00Z")] {
        set_version(&work, v);
        commit_all(&work, &format!("Release {v}"));
        tag_at(&work, &format!("v{v}"), "HEAD", date);
    }
    git(&work, &["push", "-q", "origin", "release/0.5", "--tags"]);
    git(&work, &["checkout", "-q", "main"]);
    git(&work, &["merge", "-q", "--no-ff", "-s", "ours", "release/0.5", "-m", "Merge release/0.5 forward"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);

    let v = edition(&work, &["verify"]);
    assert!(v.stdout.contains("warn  v0.5.6 was superseded by v0.5.7 2 days after it was tagged"), "{}", v.stdout);
    assert!(!v.stdout.contains("v0.5.7 was superseded"), "17 days is outside the window: {}", v.stdout);
    assert_eq!(v.code, 0, "a warning, not an error: {}", v.all());

    write(&work, "edition.toml", &format!("{BASE_CONFIG}\n[verify]\nsuperseded_days = 0\n"));
    let off = edition(&work, &["verify"]);
    assert!(!off.stdout.contains("superseded"), "{}", off.stdout);
}
