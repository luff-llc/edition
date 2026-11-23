//! Tests for the protections: receipts, live remote state, ref ambiguity, the ledger
//! chain, injection, secret redaction, the public content check and clean checkouts.

mod common;
use common::*;
use std::path::Path;

fn staging_receipt(work: &Path) -> String {
    let dry = edition(work, &["deploy", "staging-home", "main"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    dry.receipt()
}

#[test]
fn a_receipt_binds_the_exact_plan_and_is_single_use() {
    let (root, work) = project("receipt", BASE_CONFIG, "0.6.0-dev");

    // Changed ref: main moves between the dry run and the apply.
    let r1 = staging_receipt(&work);
    let other = other_clone(&root, "other");
    write(&other, "moved.md", "moved");
    commit_all(&other, "Move main");
    git(&other, &["push", "-q", "origin", "main"]);
    git(&work, &["pull", "-q", "--ff-only", "origin", "main"]);
    let moved = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &r1]);
    assert_eq!(moved.code, 2, "{}", moved.all());
    assert!(moved.stderr.contains("does not match this plan"), "{}", moved.stderr);
    assert!(!work.join("staging-deploys.txt").exists());

    // Changed configuration.
    let r2 = staging_receipt(&work);
    let cfg = std::fs::read_to_string(work.join("edition.toml")).expect("cfg");
    write(
        &work,
        "edition.toml",
        &cfg.replace("target = \"https://staging.example.com\"", "target = \"https://staging2.example.com\""),
    );
    commit_all(&work, "Change the staging target");
    git(&work, &["push", "-q", "origin", "main"]);
    let changed = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &r2]);
    assert_eq!(changed.code, 2, "{}", changed.all());
    assert!(changed.stderr.contains("does not match this plan"));

    // Expired.
    let r3 = staging_receipt(&work);
    let late = edition_env(
        &work,
        &["deploy", "staging-home", "main", "--apply", "--confirm", &r3],
        &[("EDITION_NOW", "2026-11-23T12:31:00Z")],
    );
    assert_eq!(late.code, 2, "{}", late.all());
    assert!(late.stderr.contains("expired"), "{}", late.stderr);

    // A receipt the dry run never issued.
    let forged = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", "0123456789abcdef"]);
    assert_eq!(forged.code, 2);

    // Used once, then refused.
    let r4 = staging_receipt(&work);
    let ok = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &r4]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let replay = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &r4]);
    assert_eq!(replay.code, 2, "{}", replay.all());
    assert!(replay.stderr.contains("already used"), "{}", replay.stderr);
    // The ledger alone also stops a replay (local receipt state removed).
    std::fs::remove_dir_all(work.join(".edition")).expect("rm state");
    let replay2 = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &r4]);
    assert_eq!(replay2.code, 2);
    assert!(replay2.stderr.contains("already used"), "{}", replay2.stderr);
}

#[test]
fn stale_tracking_refs_are_not_trusted() {
    let (root, work) = project("stale", BASE_CONFIG, "0.6.0-dev");
    let other = other_clone(&root, "other");
    write(&other, "new.md", "new");
    let newer = commit_all(&other, "Newer main");
    git(&other, &["push", "-q", "origin", "main"]);
    // `work` has not fetched: its origin/main is stale. The live head is not in this clone.
    let out = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stderr.contains("fetch first"), "{}", out.stderr);
    git(&work, &["fetch", "-q", "origin"]);
    let dry = edition(&work, &["deploy", "staging-home", "main"]);
    assert!(dry.stdout.contains(&newer[..10]), "{}", dry.stdout);
}

#[test]
fn the_remote_is_rechecked_immediately_before_each_outward_step() {
    // A check that moves main on the remote after the receipt was accepted, standing in
    // for a concurrent push. The outward step must not run.
    let (root, work) = project("recheck", BASE_CONFIG, "0.6.0-dev");
    let other = other_clone(&root, "other");
    let mover = format!(
        "#!/bin/sh\ncd '{}' && echo x >> moved.md && git -c user.name=T -c user.email=t@example.invalid add -A && git -c user.name=T -c user.email=t@example.invalid commit -qm move && git push -q origin main\n",
        other.display()
    );
    executable(&work, "scripts/mover.sh", &mover);
    let cfg = BASE_CONFIG.replace("[checks.unit]\nrun = [\"true\"]", "[checks.unit]\nrun = [\"scripts/mover.sh\"]");
    write(&work, "edition.toml", &cfg);
    commit_all(&work, "Mover check");
    git(&work, &["push", "-q", "origin", "main"]);
    git(&other, &["pull", "-q", "--ff-only", "origin", "main"]);
    let out = apply(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stderr.contains("remote refs/heads/main is now"), "{}", out.stderr);
    assert!(!work.join("staging-deploys.txt").exists(), "the deploy never ran");
    let ledger = std::fs::read_to_string(work.join("releases/ledger.toml")).expect("ledger");
    assert!(ledger.contains("result = \"failed\""));
    assert!(ledger.contains("stopped: remote refs/heads/main is now"));
}

#[test]
fn a_moved_tag_or_force_moved_line_invalidates_a_prod_plan() {
    let (root, work) = project("moved-tag", BASE_CONFIG, "0.7.0-dev");
    let line = line_with_version(&work, "0.6", "0.6.0");
    git(&work, &["tag", "-a", "v0.6.0", &line, "-m", "final"]);
    git(&work, &["push", "-q", "origin", "v0.6.0"]);
    let dry = edition(&work, &["deploy", "production-home", "v0.6.0", "--approved-by", "owner"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    let r = dry.receipt();

    // Someone force-moves the release line on the remote.
    let other = other_clone(&root, "other");
    git(&other, &["checkout", "-q", "-b", "rewrite", "origin/main"]);
    write(&other, "rewrite.md", "rewrite");
    commit_all(&other, "Rewrite");
    git(&other, &["push", "-q", "-f", "origin", "rewrite:release/0.6"]);
    let out =
        edition(&work, &["deploy", "production-home", "v0.6.0", "--approved-by", "owner", "--apply", "--confirm", &r]);
    assert_ne!(out.code, 0, "{}", out.all());
    assert!(
        out.stderr.contains("is not on origin/release/0.6") || out.stderr.contains("does not match this plan"),
        "{}",
        out.stderr
    );
    assert!(!work.join("prod-deploys.txt").exists());

    // And a tag moved on the remote is caught the same way.
    git(&other, &["push", "-q", "-f", "origin", &format!("{line}:refs/heads/release/0.6")]);
    let dry2 = edition(&work, &["deploy", "production-home", "v0.6.0", "--approved-by", "owner"]);
    let r2 = dry2.receipt();
    git(&other, &["fetch", "-q", "--tags", "origin"]);
    git(&other, &["tag", "-f", "-a", "v0.6.0", "origin/main", "-m", "moved"]);
    git(&other, &["push", "-q", "-f", "origin", "v0.6.0"]);
    let out2 =
        edition(&work, &["deploy", "production-home", "v0.6.0", "--approved-by", "owner", "--apply", "--confirm", &r2]);
    assert_ne!(out2.code, 0, "{}", out2.all());
    assert!(out2.stderr.contains("is not on origin") || out2.stderr.contains("does not match"), "{}", out2.stderr);
    assert!(!work.join("prod-deploys.txt").exists());
}

#[test]
fn prod_refuses_a_branch_or_commit_even_when_a_tag_points_at_it() {
    let (_root, work) = project("ambiguous", BASE_CONFIG, "0.7.0-dev");
    let line = line_with_version(&work, "0.6", "0.6.0");
    git(&work, &["tag", "-a", "v0.6.0", &line, "-m", "final"]);
    git(&work, &["push", "-q", "origin", "v0.6.0"]);
    let sha = edition(&work, &["deploy", "production-home", &line, "--approved-by", "owner"]);
    assert_eq!(sha.code, 1);
    assert!(sha.stderr.contains("prod takes a release tag by name"), "{}", sha.stderr);
    git(&work, &["branch", "v0.6.0", &line]);
    let ambiguous = edition(&work, &["deploy", "production-home", "v0.6.0", "--approved-by", "owner"]);
    assert_eq!(ambiguous.code, 1, "{}", ambiguous.all());
    assert!(ambiguous.stderr.contains("not also a branch"), "{}", ambiguous.stderr);
    let explicit = edition(&work, &["deploy", "production-home", "refs/tags/v0.6.0", "--approved-by", "owner"]);
    assert_eq!(explicit.code, 0, "{}", explicit.all());
}

fn two_entry_ledger(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let (root, work) = project(name, BASE_CONFIG, "0.6.0-dev");
    assert_eq!(apply(&work, &["deploy", "staging-home", "main", "--note", "first"]).code, 0);
    commit_all(&work, "Record first");
    git(&work, &["push", "-q", "origin", "main"]);
    assert_eq!(apply(&work, &["deploy", "staging-home", "main", "--note", "second"]).code, 0);
    commit_all(&work, "Record second");
    git(&work, &["push", "-q", "origin", "main"]);
    (root, work)
}

#[test]
fn committed_ledger_forgery_truncation_and_reordering_are_caught() {
    let (_root, work) = two_entry_ledger("forgery");
    let v = edition(&work, &["verify"]);
    assert!(v.stdout.contains("hash chain intact (2 entries)"), "{}", v.stdout);
    let path = work.join("releases/ledger.toml");
    let text = std::fs::read_to_string(&path).expect("ledger");

    // Forgery in the working copy.
    write(&work, "releases/ledger.toml", &text.replacen("note = \"first\"", "note = \"forged\"", 1));
    let forged = edition(&work, &["verify"]);
    assert_eq!(forged.code, 1);
    assert!(forged.stdout.contains("hash chain broken"), "{}", forged.stdout);

    // Truncation, committed: the history check names the commit.
    let first_end = text
        .find("\n[[entry]]")
        .and_then(|a| text[a + 1..].find("\n[[entry]]").map(|b| a + 1 + b))
        .expect("two entries");
    write(&work, "releases/ledger.toml", &text[..first_end + 1]);
    let trunc = commit_all(&work, "Drop an entry");
    let truncated = edition(&work, &["verify"]);
    assert_eq!(truncated.code, 1);
    assert!(truncated.stdout.contains(&format!("was rewritten at {}", &trunc[..10])), "{}", truncated.stdout);

    // Reordering, committed.
    git(&work, &["reset", "-q", "--hard", "HEAD~1"]);
    let body = &text[text.find("\n[[entry]]").expect("entry")..];
    let second = body[1..].find("\n[[entry]]").map(|i| i + 1).expect("second");
    let reordered = format!("{}{}{}", &text[..text.find("\n[[entry]]").expect("e")], &body[second..], &body[..second]);
    write(&work, "releases/ledger.toml", &reordered);
    commit_all(&work, "Reorder");
    let re = edition(&work, &["verify"]);
    assert_eq!(re.code, 1);
    assert!(re.stdout.contains("hash chain broken"), "{}", re.stdout);
    assert!(re.stdout.contains("was rewritten at"), "{}", re.stdout);
}

#[test]
fn concurrent_appends_keep_the_chain() {
    let dir = tmp("concurrent");
    git(&dir, &["init", "-q"]);
    let path = dir.join("ledger.toml");
    let handles: Vec<_> = (0..8)
        .map(|t| {
            let dir = dir.clone();
            let path = path.clone();
            std::thread::spawn(move || {
                let g = edition::git::Git::new(&dir);
                for i in 0..10 {
                    let e = edition::ledger::Entry {
                        id: format!("t{t}-{i}"),
                        at: NOW.into(),
                        by: "test".into(),
                        action: "deploy".into(),
                        product: "lib".into(),
                        ref_: "main".into(),
                        commit: "0".repeat(40),
                        result: "ok".into(),
                        ..Default::default()
                    };
                    edition::ledger::append(&g, &path, &e).expect("append");
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread");
    }
    let entries = edition::ledger::load(&path).expect("load");
    assert_eq!(entries.len(), 80);
    let g = edition::git::Git::new(&dir);
    assert!(edition::ledger::check_chain(&g, &entries).is_ok());
}

#[test]
fn shell_metacharacters_never_reach_a_command() {
    let (_root, work) = project("inject", BASE_CONFIG, "0.6.0-dev");
    for bad in ["main;touch x", "$(touch x)", "main x", "-main", "a`b`", "a|b"] {
        let out = edition(&work, &["deploy", "staging-home", bad]);
        assert_ne!(out.code, 0, "{bad}: {}", out.all());
        assert!(
            out.stderr.contains("not a valid ref name") || out.stderr.contains("unknown option"),
            "{bad}: {}",
            out.stderr
        );
        assert!(!work.join("staging-deploys.txt").exists() && !work.join("x").exists());
    }
    let build = edition(&work, &["deploy", "staging-home", "main", "--build", "1;touch x"]);
    assert_eq!(build.code, 1);
    assert!(build.stderr.contains("build label"), "{}", build.stderr);
    let at = edition(&work, &["tag", "0.6.0", "--at", "$(x)"]);
    assert_eq!(at.code, 1);
    let actor = edition_env(&work, &["deploy", "staging-home", "main"], &[("EDITION_ACTOR", "a\nb")]);
    assert_eq!(actor.code, 1);

    // Configuration: no shell -c, no placeholder inside other text.
    let shell = BASE_CONFIG.replace(
        "run = [\"scripts/record.sh\", \"staging-deploys.txt\", \"{commit}\"]",
        "run = [\"sh\", \"-c\", \"echo {commit}\"]",
    );
    write(&work, "edition.toml", &shell);
    let s = edition(&work, &["status"]);
    assert_eq!(s.code, 2);
    assert!(s.stderr.contains("never through `sh -c`"), "{}", s.stderr);
    let partial =
        BASE_CONFIG.replace("\"staging-deploys.txt\", \"{commit}\"", "\"staging-deploys.txt\", \"--sha={commit}\"");
    write(&work, "edition.toml", &partial);
    let p = edition(&work, &["status"]);
    assert_eq!(p.code, 2);
    assert!(p.stderr.contains("must be a whole argument"), "{}", p.stderr);

    // A version file with metacharacters is not a version.
    write(&work, "edition.toml", BASE_CONFIG);
    git(&work, &["checkout", "-q", "-b", "release/0.6"]);
    set_version(&work, "0.6.0;x");
    commit_all(&work, "Bad version");
    git(&work, &["push", "-q", "origin", "release/0.6"]);
    git(&work, &["checkout", "-q", "main"]);
    let v = edition(&work, &["tag", "0.6.0"]);
    assert_eq!(v.code, 1, "{}", v.all());

    // A machine value that looks like an option is refused.
    let var_cfg = BASE_CONFIG.replace("[checks.unit]", "[vars]\nkit = \"\"\n\n[checks.unit]").replace(
        "run = [\"scripts/record.sh\", \"staging-deploys.txt\", \"{commit}\"]",
        "run = [\"scripts/record.sh\", \"{var.kit}\", \"{commit}\"]",
    );
    write(&work, "edition.toml", &var_cfg);
    commit_all(&work, "Var config");
    git(&work, &["push", "-q", "origin", "main"]);
    let opt = edition_env(&work, &["deploy", "staging-home", "main"], &[("EDITION_VAR_KIT", "--upload-pack=x")]);
    assert_eq!(opt.code, 1, "{}", opt.all());
    assert!(opt.stderr.contains("must not start with `-`"), "{}", opt.stderr);
}

#[test]
fn values_from_the_environment_are_printed_by_name_and_never_recorded() {
    let cfg = BASE_CONFIG.replace("[checks.unit]", "[vars]\nout = \"\"\n\n[checks.unit]").replace(
        "run = [\"scripts/record.sh\", \"staging-deploys.txt\", \"{commit}\"]\nprobe",
        "run = [\"scripts/record.sh\", \"{var.out}\", \"{commit}\"]\nenv = { DEPLOY_TOKEN = \"{var.out}\" }\nprobe",
    );
    let (_root, work) = project("secrets", &cfg, "0.6.0-dev");
    let secret = "staging-deploys-SECRET-7f3a.txt";
    let env = [("EDITION_VAR_OUT", secret)];
    let dry = edition_env(&work, &["deploy", "staging-home", "main"], &env);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(!dry.all().contains("SECRET-7f3a"), "{}", dry.all());
    assert!(dry.stdout.contains("<var.out>"));
    assert!(dry.stdout.contains("env: DEPLOY_TOKEN"));
    let ok = apply_env(&work, &["deploy", "staging-home", "main"], &env);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert!(work.join(secret).exists(), "the real value reached the command");
    let ledger = std::fs::read_to_string(work.join("releases/ledger.toml")).expect("ledger");
    assert!(!ledger.contains("SECRET-7f3a"), "{ledger}");
    assert!(ledger.contains("{var.out}"));
}

fn public_project(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let cfg = format!("{BASE_CONFIG}\n[remotes.origin]\npublic = true\n\n[content]\nallow_paths = [\"allowed/**\"]\n");
    let (root, work) = project(name, &cfg, "0.7.0-dev");
    (root, work)
}

fn note_text() -> String {
    format!("## Decisions {} Alex\n", "for")
}

#[test]
fn the_publish_scan_covers_exactly_what_the_remote_lacks_and_verify_blocks_without_a_hook() {
    let (_root, work) = public_project("content-scan");
    write(&work, "docs/plan.md", &note_text());
    let local = commit_all(&work, "A note, not pushed");
    assert!(!work.join(".git/hooks/pre-push").exists());

    let loaded = edition::config::Loaded::load(&work.join("edition.toml")).expect("config");
    let g = edition::git::Git::new(&work);
    let view = edition::policy::RemoteView::build(&g, "origin", true).expect("remote");
    let scanner = edition::content::Scanner::new(&loaded);
    let found = scanner.scan_publish(&g, std::slice::from_ref(&local), &view.tips());
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].place.contains("docs/plan.md"));
    assert_eq!(found[0].id.len(), 10);

    // `verify` fails on it (content.action defaults to block), with no hook installed.
    let v = edition(&work, &["verify"]);
    assert_eq!(v.code, 1, "{}", v.all());
    assert!(v.stdout.contains("ERROR content:"), "{}", v.stdout);

    // Once the remote has it, nothing is left to publish.
    git(&work, &["push", "-q", "origin", "main"]);
    let view2 = edition::policy::RemoteView::build(&g, "origin", true).expect("remote");
    assert!(scanner.scan_publish(&g, &[local], &view2.tips()).is_empty());
}

#[test]
fn merge_only_and_renamed_findings_are_scanned() {
    let (_root, work) = public_project("content-merge");
    let s = |w: &Path| edition(w, &["check-push", "origin"]);

    // Renamed out of an allowed path: the full file counts as added.
    write(&work, "allowed/x.md", &note_text());
    commit_all(&work, "Allowed note");
    git(&work, &["push", "-q", "origin", "main"]);
    std::fs::create_dir_all(work.join("docs")).expect("mkdir");
    git(&work, &["mv", "allowed/x.md", "docs/x.md"]);
    commit_all(&work, "Move it");
    let renamed = s(&work);
    assert_eq!(renamed.code, 1, "{}", renamed.all());
    assert!(renamed.stderr.contains("docs/x.md"), "{}", renamed.stderr);
    git(&work, &["reset", "-q", "--hard", "origin/main"]);

    // Merge-only: both sides clean, the merge commit adds the note.
    git(&work, &["checkout", "-q", "-b", "side"]);
    write(&work, "side.md", "side\n");
    commit_all(&work, "Side");
    git(&work, &["checkout", "-q", "main"]);
    write(&work, "main.md", "main\n");
    commit_all(&work, "Main");
    git(&work, &["merge", "-q", "--no-ff", "--no-commit", "side"]);
    write(&work, "merge.md", &note_text());
    git(&work, &["add", "merge.md"]);
    git(&work, &["commit", "-q", "-m", "Merge side"]);
    let merged = s(&work);
    assert_eq!(merged.code, 1, "{}", merged.all());
    assert!(merged.stderr.contains("merge.md"), "{}", merged.stderr);
}

#[test]
fn outward_plans_refuse_a_dirty_tree_or_detached_head() {
    let (_root, work) = project("clean", BASE_CONFIG, "0.6.0-dev");
    write(&work, "Cargo.toml", "[package]\nname = \"lib\"\nversion = \"0.6.0-dev\"\n# local edit\n");
    let dirty = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(dirty.code, 1);
    assert!(dirty.stderr.contains("uncommitted changes: Cargo.toml"), "{}", dirty.stderr);
    git(&work, &["checkout", "-q", "--", "Cargo.toml"]);
    git(&work, &["checkout", "-q", "--detach", "HEAD"]);
    let detached = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(detached.code, 1);
    assert!(detached.stderr.contains("HEAD is detached"), "{}", detached.stderr);
    // Dev deploys are local and need neither.
    assert_eq!(edition(&work, &["deploy", "local", "HEAD"]).code, 0);
}

#[test]
fn outward_plans_are_refused_when_the_remote_cannot_be_read() {
    let (root, work) = project("offline", BASE_CONFIG, "0.6.0-dev");
    std::fs::rename(root.join("origin.git"), root.join("gone.git")).expect("rename");
    let out = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(out.stderr.contains("outward steps are refused"), "{}", out.stderr);
    let status = edition(&work, &["status"]);
    assert_eq!(status.code, 0, "read-only commands still work: {}", status.all());
}
