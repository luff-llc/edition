//! Scenario tests for the release model against temporary git repositories.

mod common;
use common::*;
use std::path::{Path, PathBuf};

#[test]
fn cut_rc_and_tag_follow_the_line_model() {
    let (_root, work) = project("cut", BASE_CONFIG, "0.6.0-dev");

    let dry = edition(&work, &["cut", "0.6"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains("[local] create release/0.6"), "{}", dry.stdout);
    assert!(dry.stdout.contains("[OUTWARD] publish release/0.6"));
    assert!(dry.stdout.contains("requires remote refs/heads/release/0.6 = absent"));
    let receipt = dry.receipt();
    assert!(!work.join("releases/ledger.toml").exists());

    let unconfirmed = edition(&work, &["cut", "0.6", "--apply"]);
    assert_eq!(unconfirmed.code, 2, "{}", unconfirmed.all());
    assert!(unconfirmed.stderr.contains("receipt"));
    assert!(git(&work, &["branch", "--list", "release/0.6"]).is_empty(), "nothing runs without a receipt");

    let applied = edition(&work, &["cut", "0.6", "--apply", "--confirm", &receipt]);
    assert_eq!(applied.code, 0, "{}", applied.all());
    assert!(git(&work, &["ls-remote", "--heads", "origin"]).contains("refs/heads/release/0.6"));
    let ledger = std::fs::read_to_string(work.join("releases/ledger.toml")).expect("ledger");
    assert!(ledger.contains("action = \"push-branch\""));
    assert!(ledger.contains("by = \"test-agent\""));
    assert!(ledger.contains(&format!("receipt = \"{receipt}\"")));

    let again = edition(&work, &["cut", "0.6"]);
    assert_eq!(again.code, 1);
    assert!(again.stderr.contains("already exists"));

    // The line still says 0.6.0-dev: a candidate is refused until the line is bumped.
    git(&work, &["fetch", "-q", "origin"]);
    let early = edition(&work, &["rc", "0.6.0"]);
    assert_eq!(early.code, 1, "{}", early.all());
    assert!(early.stderr.contains("say 0.6.0-dev, not 0.6.0"), "{}", early.stderr);

    git(&work, &["checkout", "-q", "release/0.6"]);
    let bump = edition(&work, &["bump", "0.6.0", "--apply"]);
    assert_eq!(bump.code, 0, "{}", bump.all());
    commit_all(&work, "Start the 0.6 line");
    git(&work, &["push", "-q", "origin", "release/0.6"]);
    git(&work, &["checkout", "-q", "main"]);

    let rc1 = apply(&work, &["rc", "0.6.0"]);
    assert_eq!(rc1.code, 0, "{}", rc1.all());
    assert!(rc1.stdout.contains("v0.6.0-rc.1"));
    let rc2 = edition(&work, &["rc", "0.6.0"]);
    assert!(rc2.stdout.contains("v0.6.0-rc.2"), "{}", rc2.stdout);

    let fin = apply(&work, &["tag", "0.6.0"]);
    assert_eq!(fin.code, 0, "{}", fin.all());
    let remote_tags = git(&work, &["ls-remote", "--tags", "origin"]);
    assert!(remote_tags.contains("refs/tags/v0.6.0-rc.1"));
    assert!(remote_tags.contains("refs/tags/v0.6.0^{}"));
    assert_eq!(edition(&work, &["tag", "0.6.0"]).code, 1);

    let wrong_line = edition(&work, &["tag", "0.7.0"]);
    assert_eq!(wrong_line.code, 1);
    assert!(wrong_line.stderr.contains("release/0.7 does not exist"));
}

#[test]
fn staging_takes_only_the_tip_of_main() {
    let (_root, work) = project("staging", BASE_CONFIG, "0.6.0-dev");
    let first = git(&work, &["rev-parse", "HEAD"]);
    write(&work, "a.md", "a");
    let tip = commit_all(&work, "Second");
    git(&work, &["push", "-q", "origin", "main"]);

    git(&work, &["checkout", "-q", "-b", "feature"]);
    write(&work, "b.md", "b");
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
    assert!(dry.stdout.contains(&format!("scripts/record.sh staging-deploys.txt {tip}")), "{}", dry.stdout);
    assert!(dry.stdout.contains(&format!("requires remote refs/heads/main = {}", &tip[..10])));

    let wrong = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", "staging-home"]);
    assert_eq!(wrong.code, 2);
    assert!(!work.join("staging-deploys.txt").exists());

    let ok = apply(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert_eq!(std::fs::read_to_string(work.join("staging-deploys.txt")).expect("deployed").trim(), tip);
    let ledger = std::fs::read_to_string(work.join("releases/ledger.toml")).expect("ledger");
    assert!(ledger.contains("tier = \"staging\""));
    assert!(ledger.contains(&format!("commit = \"{tip}\"")));
    assert!(ledger.contains("check unit: exit 0"));
    assert!(ledger.contains("version = \"0.6.0-dev\""));
    assert!(ledger.contains("result = \"ok\""));

    let status = edition(&work, &["status"]);
    assert!(status.stdout.contains("staging-home"));
    assert!(status.stdout.contains(&tip[..10]));
}

#[test]
fn dev_tier_runs_locally_and_records_nothing() {
    let (_root, work) = project("dev", BASE_CONFIG, "0.6.0-dev");
    git(&work, &["checkout", "-q", "-b", "wip"]);
    write(&work, "w.md", "w");
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
    assert!(untagged.stderr.contains("prod takes a release tag by name"), "{}", untagged.stderr);

    git(&work, &["tag", "-a", "v0.6.0-rc.1", &line, "-m", "rc"]);
    git(&work, &["push", "-q", "origin", "v0.6.0-rc.1"]);
    let rc_home = edition(&work, &["deploy", "production-home", "v0.6.0-rc.1", "--approved-by", "owner"]);
    assert_eq!(rc_home.code, 1);
    assert!(rc_home.stderr.contains("pre-release tag"), "{}", rc_home.stderr);
    let rc_review = apply(&work, &["deploy", "store-review", "v0.6.0-rc.1"]);
    assert_eq!(rc_review.code, 0, "{}", rc_review.all());

    // A final tag that exists only locally is refused: the remote is read live.
    git(&work, &["tag", "-a", "v0.6.0", &line, "-m", "final"]);
    let local_only = edition(&work, &["deploy", "production-home", "v0.6.0", "--approved-by", "owner"]);
    assert_eq!(local_only.code, 1);
    assert!(local_only.stderr.contains("is not on origin"), "{}", local_only.stderr);

    git(&work, &["push", "-q", "origin", "v0.6.0"]);
    let no_approval = edition(&work, &["deploy", "production-home", "v0.6.0"]);
    assert_eq!(no_approval.code, 1);
    assert!(no_approval.stderr.contains("--approved-by"));

    let ok = apply(&work, &["deploy", "production-home", "v0.6.0", "--approved-by", "owner"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let ledger = std::fs::read_to_string(work.join("releases/ledger.toml")).expect("ledger");
    assert!(ledger.contains("approved_by = \"owner\""));
    assert!(ledger.contains("ref = \"v0.6.0\""));

    let verify = edition(&work, &["verify", "--online"]);
    assert!(verify.stdout.contains("production-home holds v0.6.0"), "{}", verify.stdout);
}

#[test]
fn promote_to_prod_requires_the_held_commit_to_be_tagged() {
    let (_root, work) = project("promote", BASE_CONFIG, "0.6.0");
    let ok = apply(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let refused = edition(&work, &["promote", "staging-home", "production-home", "--approved-by", "owner"]);
    assert_eq!(refused.code, 1);
    assert!(refused.stderr.contains("carries no release tag"), "{}", refused.stderr);

    git(&work, &["push", "-q", "origin", "main:release/0.6"]);
    git(&work, &["tag", "-a", "v0.6.0", "HEAD", "-m", "final"]);
    git(&work, &["push", "-q", "origin", "v0.6.0"]);
    let promoted = edition(&work, &["promote", "staging-home", "production-home", "--approved-by", "owner"]);
    assert_eq!(promoted.code, 0, "{}", promoted.all());
    assert!(promoted.stdout.contains("deploying refs/tags/v0.6.0 to production-home"));
}

#[test]
fn verify_finds_unmerged_lines_and_forward_offers_a_pull_request() {
    let (_root, work) = project("forward", BASE_CONFIG, "0.6.0-dev");
    line_with_version(&work, "0.5", "0.5.2");
    git(&work, &["checkout", "-q", "release/0.5"]);
    write(&work, "fix.md", "fix");
    commit_all(&work, "Hotfix on the line");
    git(&work, &["push", "-q", "origin", "release/0.5"]);
    git(&work, &["checkout", "-q", "main"]);

    let v = edition(&work, &["verify"]);
    assert_eq!(v.code, 1, "{}", v.all());
    assert!(v.stdout.contains("origin/release/0.5 has 2 commit(s) not merged forward into main"), "{}", v.stdout);

    let fwd = edition(&work, &["forward"]);
    assert_eq!(fwd.code, 0, "{}", fwd.all());
    assert!(fwd.stdout.contains("gh pr create --base main --head release/0.5"), "{}", fwd.stdout);
    fwd.receipt();

    git(&work, &["merge", "-q", "--no-ff", "-s", "ours", "origin/release/0.5", "-m", "Merge release/0.5 forward"]);
    git(&work, &["push", "-q", "origin", "main"]);
    let v2 = edition(&work, &["verify"]);
    assert!(v2.stdout.contains("origin/release/0.5 is merged forward into main"), "{}", v2.stdout);
    assert!(v2.stdout.contains("origin/main builds as 0.6.0-dev"));
    assert_eq!(v2.code, 0, "{}", v2.all());
}

#[test]
fn hotfix_start_then_finish_tags_and_opens_the_forward_merge_in_one_plan() {
    let (root, work) = project("hotfix", BASE_CONFIG, "0.6.0-dev");
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
    let fix = commit_all(&work, "Fix and bump to 0.5.3");
    git(&work, &["push", "-q", "origin", "hotfix/0.5.3:release/0.5"]);
    git(&work, &["checkout", "-q", "main"]);

    let path = fake_gh(&root);
    let env = [("PATH", path.as_str())];
    let dry = edition_env(&work, &["hotfix", "finish", "0.5.3"], &env);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert_eq!(dry.stdout.matches("Receipt: ").count(), 1, "one plan, one receipt");
    let finish = apply_env(&work, &["hotfix", "finish", "0.5.3"], &env);
    assert_eq!(finish.code, 0, "{}", finish.all());
    assert!(git(&work, &["ls-remote", "--tags", "origin"]).contains("refs/tags/v0.5.3"));
    let calls = std::fs::read_to_string(root.join("bin/gh-calls.log")).expect("gh ran");
    assert!(calls.contains("pr create --base main --head release/0.5"), "{calls}");
    let ledger = std::fs::read_to_string(work.join("releases/ledger.toml")).expect("ledger");
    assert!(ledger.contains("action = \"push-tag\""));
    assert!(ledger.contains("action = \"open-pr\""));
    assert!(ledger.contains(&format!("commit = \"{fix}\"")));
}

#[test]
fn a_pinned_commit_on_no_remote_is_detected() {
    let root = tmp("pins");
    let lib_origin = root.join("lib.git");
    git(&root, &["init", "-q", "--bare", lib_origin.to_str().expect("utf8")]);
    git(&root, &["clone", "-q", lib_origin.to_str().expect("utf8"), "lib"]);
    let lib = root.join("lib");
    git(&lib, &["checkout", "-q", "-b", "main"]);
    write(&lib, "lib.md", "1");
    commit_all(&lib, "Lib");
    git(&lib, &["push", "-q", "origin", "main"]);
    git(&lib, &["checkout", "-q", "-b", "local-only"]);
    write(&lib, "lib.md", "2");
    let unpushed = commit_all(&lib, "Only here");

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
kind = "app-store"
target = "app store"
run = ["true"]
"#;
    write(&app, "edition.toml", cfg);
    write(&app, "tools/sdk.sh", &format!("#!/bin/sh\nlib_commit={unpushed}\n"));
    commit_all(&app, "App");
    git(&app, &["push", "-q", "origin", "main"]);

    let v = edition(&app, &["verify"]);
    assert_eq!(v.code, 1, "{}", v.all());
    assert!(v.stdout.contains("exists only in a local checkout"), "{}", v.stdout);

    git(&lib, &["push", "-q", "origin", "local-only"]);
    let v2 = edition(&app, &["verify", "--online"]);
    assert_eq!(v2.code, 1);
    assert!(v2.stdout.contains("is not a release tag commit"), "{}", v2.stdout);

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

    // Release candidates are for review kinds only, never for a production home.
    let home_rc = BASE_CONFIG.replace(
        "approval = \"owner\"\nrun = [\"scripts/record.sh\", \"prod-deploys.txt\"",
        "approval = \"owner\"\nallow_prerelease = true\nrun = [\"scripts/record.sh\", \"prod-deploys.txt\"",
    );
    write(&work, "edition.toml", &home_rc);
    let out2 = edition(&work, &["status"]);
    assert_eq!(out2.code, 2, "{}", out2.all());
    assert!(out2.stderr.contains("allow_prerelease is only for review or testing kinds"), "{}", out2.stderr);
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
fn rc_numbers_compare_numerically() {
    let (_root, work) = project("rc-order", BASE_CONFIG, "0.7.0-dev");
    let line = line_with_version(&work, "0.6", "0.6.0");
    for n in [2, 10, 9] {
        let t = format!("v0.6.0-rc.{n}");
        git(&work, &["tag", "-a", &t, &line, "-m", "rc"]);
        git(&work, &["push", "-q", "origin", &t]);
    }
    let next = edition(&work, &["rc", "0.6.0"]);
    assert_eq!(next.code, 0, "{}", next.all());
    assert!(next.stdout.contains("v0.6.0-rc.11"), "{}", next.stdout);
}

#[test]
fn concordance_builds_must_match_the_ledger() {
    let root = tmp("concordance-bin");
    let fake = root.join("concordance");
    executable(
        &root,
        "concordance",
        "#!/bin/sh\n[ \"$1\" = log ] && exit 0\nwhile [ $# -gt 0 ]; do case \"$1\" in --out) shift; out=\"$1\";; esac; shift; done\nprintf '%s' '{\"releases\":[{\"id\":\"0.6.0\",\"state\":\"candidate\",\"builds\":{\"ios\":\"41\"},\"carries\":[{\"id\":\"sync.join\",\"ready\":true},{\"id\":\"bug.x\",\"ready\":false}]}]}' > \"$out\"\n",
    );
    let cfg = format!(
        "{BASE_CONFIG}\n[concordance]\ncatalog = \"spec/app\"\nbin = \"{}\"\n\n[channels.testflight-internal]\ntier = \"staging\"\nkind = \"testflight\"\ntarget = \"internal\"\nplatform = \"ios\"\nrun = [\"true\"]\n\n[platforms.ios]\n",
        fake.display()
    );
    let (_r, work) = project("concordance", &cfg, "0.6.0-dev");

    let plan = edition(&work, &["plan", "0.6.0"]);
    assert!(plan.stdout.contains("Concordance 0.6.0: candidate"), "{}", plan.stdout);
    assert!(plan.stdout.contains("1/2 carried items ready"));
    assert!(plan.stdout.contains("not ready: bug.x"));

    let need_build = edition(&work, &["deploy", "testflight-internal", "main"]);
    assert_eq!(need_build.code, 1);
    assert!(need_build.stderr.contains("--build"));

    let dry = edition(&work, &["deploy", "testflight-internal", "main", "--build", "42"]);
    assert!(dry.stdout.contains("log uploaded 0.6.0 --platform ios --build 42"), "{}", dry.stdout);

    let ran = apply(&work, &["deploy", "testflight-internal", "main", "--build", "42"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let v = edition(&work, &["verify"]);
    assert!(v.stdout.contains("Concordance 0.6.0 says ios build 41, the ledger says 42"), "{}", v.stdout);
}

/// A fake `concordance` whose `track` reports release 1.2.0 with the given readiness.
fn fake_concordance(dir: &Path, all_ready: bool) -> PathBuf {
    let ready = if all_ready { "true" } else { "false" };
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = log ] && {{ echo \"$@\" >> \"$(dirname \"$0\")/log-calls.log\"; exit 0; }}\nwhile [ $# -gt 0 ]; do case \"$1\" in --out) shift; out=\"$1\";; esac; shift; done\nprintf '%s' '{{\"releases\":[{{\"id\":\"1.2.0\",\"state\":\"candidate\",\"builds\":{{}},\"carries\":[{{\"id\":\"sync.join\",\"ready\":true}},{{\"id\":\"player.handoff\",\"ready\":{ready}}}]}}]}}' > \"$out\"\n"
    );
    executable(dir, "concordance", &script);
    dir.join("concordance")
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
kind = "play-testing"
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

fn app_project(name: &str, ready: bool) -> (PathBuf, PathBuf, String) {
    let bins = tmp(&format!("{name}-bin"));
    let bin = fake_concordance(&bins, ready);
    let (_r, work) = project(name, &app_config(&bin), "0.0.0");
    std::fs::remove_file(work.join("Cargo.toml")).expect("rm");
    std::fs::remove_dir_all(work.join("pkg")).expect("rm");
    set_app_version(&work, "1.2.0", 51, 134);
    let tip = commit_all(&work, "App 1.2.0");
    git(&work, &["push", "-q", "origin", "main"]);
    (bins, work, tip)
}

#[test]
fn one_version_many_platforms_with_gates_before_production() {
    let (bins, work, tip) = app_project("platforms", false);
    let events = bins.join("events.jsonl");
    let ev = events.to_str().expect("utf8");
    let ios = apply_env(&work, &["deploy", "testflight-internal", "main"], &[("EDITION_EVENTS", ev)]);
    assert_eq!(ios.code, 0, "{}", ios.all());
    let calls = std::fs::read_to_string(bins.join("log-calls.log")).expect("concordance log called");
    assert!(calls.contains("log uploaded 1.2.0 --platform ios --build 51"), "{calls}");
    let evs = std::fs::read_to_string(&events).expect("events");
    assert!(evs.contains("\"event\":\"channel-updated\""), "{evs}");

    let android = apply(&work, &["deploy", "play-internal", "main"]);
    assert_eq!(android.code, 0, "{}", android.all());
    let status = edition(&work, &["status"]);
    assert!(status.stdout.contains("1.2.0 (51)"), "{}", status.stdout);
    assert!(status.stdout.contains("1.2.0 (134)"), "{}", status.stdout);

    git(&work, &["push", "-q", "origin", &format!("{tip}:refs/heads/release/1.2")]);
    git(&work, &["tag", "-a", "v1.2.0-rc.1", &tip, "-m", "1.2.0 rc 1"]);
    git(&work, &["push", "-q", "origin", "v1.2.0-rc.1"]);
    let review = apply(&work, &["deploy", "testflight-external", "v1.2.0-rc.1"]);
    assert_eq!(review.code, 0, "{}", review.all());
    git(&work, &["tag", "-a", "v1.2.0", &tip, "-m", "1.2.0"]);
    git(&work, &["push", "-q", "origin", "v1.2.0"]);
    let gap = edition(&work, &["deploy", "app-store", "v1.2.0"]);
    assert_eq!(gap.code, 1, "{}", gap.all());
    assert!(gap.stderr.contains("1 of 2 carried items not ready: player.handoff"), "{}", gap.stderr);

    fake_concordance(&bins, true);
    let ok = apply(&work, &["deploy", "app-store", "v1.2.0"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert!(ok.stdout.contains("gate: testflight-external already holds"));
    let ledger = std::fs::read_to_string(work.join("releases/ledger.toml")).expect("ledger");
    assert!(ledger.contains("2/2 carried items ready on every platform"), "{ledger}");
    assert!(ledger.contains("platform = \"ios\""));

    write(&work, "android/build.gradle.kts", "android {\n  versionCode = 135\n  versionName = \"1.2.1\"\n}\n");
    commit_all(&work, "Android drifts");
    git(&work, &["push", "-q", "origin", "main"]);
    let v = edition(&work, &["verify"]);
    assert!(v.stdout.contains("version files disagree"), "{}", v.stdout);
}

#[test]
fn requires_gate_refuses_a_commit_review_never_held() {
    let (_bins, work, tip) = app_project("requires", true);
    git(&work, &["push", "-q", "origin", &format!("{tip}:refs/heads/release/1.2")]);
    git(&work, &["tag", "-a", "v1.2.0", &tip, "-m", "1.2.0"]);
    git(&work, &["push", "-q", "origin", "v1.2.0"]);
    let out = edition(&work, &["deploy", "app-store", "v1.2.0"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stderr.contains("gate: testflight-external has not held"), "{}", out.stderr);
}

#[test]
fn hooks_install_writes_a_pre_push_hook_once() {
    let (_root, work) = project("hooks", BASE_CONFIG, "0.6.0-dev");
    assert_eq!(edition(&work, &["hooks", "install"]).code, 0);
    assert!(!work.join(".git/hooks/pre-push").exists());
    let ok = edition(&work, &["hooks", "install", "--apply"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let hook = std::fs::read_to_string(work.join(".git/hooks/pre-push")).expect("hook");
    assert!(hook.contains("edition check-push"));
    assert!(edition(&work, &["hooks", "install", "--apply"]).stdout.contains("already installed"));
    write(&work, ".git/hooks/pre-push", "#!/bin/sh\necho mine\n");
    assert_eq!(edition(&work, &["hooks", "install", "--apply"]).code, 1);
}

#[test]
fn the_examples_are_valid_configurations() {
    for ex in ["library", "app"] {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(ex).join("edition.toml");
        let loaded = edition::config::Loaded::load(&p);
        assert!(loaded.is_ok(), "{ex}: {:?}", loaded.err());
    }
}
