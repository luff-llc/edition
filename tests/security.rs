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
    expire_receipt(&work, &r3);
    let late = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &r3]);
    assert_eq!(late.code, 2, "{}", late.all());
    assert!(late.stderr.contains("expired"), "{}", late.stderr);

    // A receipt the dry run never issued.
    let forged = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", "0123456789abcdef"]);
    assert_eq!(forged.code, 2);
    let guessed = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &"0".repeat(32)]);
    assert_eq!(guessed.code, 2);
    assert!(guessed.stderr.contains("was not issued"), "{}", guessed.stderr);

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
    let found = scanner.scan_publish(&g, std::slice::from_ref(&local), &view.tips()).expect("scan");
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
    assert!(scanner.scan_publish(&g, &[local], &view2.tips()).expect("scan").is_empty());
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

#[test]
fn receipts_are_random_and_differ_for_the_same_plan() {
    let (_root, work) = project("nonce", BASE_CONFIG, "0.6.0-dev");
    let a = staging_receipt(&work);
    let b = staging_receipt(&work);
    assert_eq!(a.len(), 32);
    assert_ne!(a, b, "a receipt is a fresh nonce, not a hash of the plan");
    // Each nonce is bound to the plan it was issued for.
    let text = std::fs::read_to_string(work.join(".edition/receipts")).expect("state");
    assert!(text
        .lines()
        .any(|l| l.starts_with(&format!("issued {a} ")) && l.split(' ').nth(2).is_some_and(|h| h.len() == 64)));
}

#[test]
fn the_receipt_state_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let (_root, work) = project("perms", BASE_CONFIG, "0.6.0-dev");
    let r = staging_receipt(&work);
    let dir = work.join(".edition");
    assert_eq!(std::fs::metadata(&dir).expect("dir").permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(dir.join("receipts")).expect("file").permissions().mode() & 0o777, 0o600);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let out = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &r]);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(out.stderr.contains("must be a private directory"), "{}", out.stderr);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    std::fs::set_permissions(dir.join("receipts"), std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let out2 = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &r]);
    assert_eq!(out2.code, 2, "{}", out2.all());
    assert!(out2.stderr.contains("must be private"), "{}", out2.stderr);
}

#[test]
fn concurrent_applies_of_one_receipt_run_exactly_once() {
    let (_root, work) = project("race", BASE_CONFIG, "0.6.0-dev");
    let r = staging_receipt(&work);
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let work = work.clone();
            let r = r.clone();
            std::thread::spawn(move || {
                edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &r]).code
            })
        })
        .collect();
    let codes: Vec<i32> = handles.into_iter().map(|h| h.join().expect("thread")).collect();
    assert_eq!(codes.iter().filter(|c| **c == 0).count(), 1, "{codes:?}");
    let deployed = std::fs::read_to_string(work.join("staging-deploys.txt")).expect("deployed");
    assert_eq!(deployed.lines().count(), 1, "{deployed}");
}

#[test]
fn channel_commands_see_only_declared_environment() {
    let cfg = BASE_CONFIG
        .replace(
            "run = [\"scripts/record.sh\", \"staging-deploys.txt\", \"{commit}\"]\nprobe",
            "run = [\"scripts/env.sh\", \"{commit}\"]\nenv = { DECLARED = \"{var.token}\" }\nprobe",
        )
        .replace("[checks.unit]", "[vars]\ntoken = \"\"\n\n[checks.unit]");
    let (_root, work) = project("env", &cfg, "0.6.0-dev");
    executable(&work, "scripts/env.sh", "#!/bin/sh\nenv > env-deploys.txt\n");
    commit_all(&work, "env script");
    git(&work, &["push", "-q", "origin", "main"]);
    let env = [("EDITION_VAR_TOKEN", "declared-value"), ("AMBIENT_SECRET", "should-not-leak")];
    let ok = apply_env(&work, &["deploy", "staging-home", "main"], &env);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let seen = std::fs::read_to_string(work.join("env-deploys.txt")).expect("env");
    assert!(seen.contains("DECLARED=declared-value"), "{seen}");
    assert!(!seen.contains("AMBIENT_SECRET"), "{seen}");
    assert!(!seen.contains("EDITION_VAR_TOKEN"), "{seen}");

    // A changed declared value invalidates the receipt.
    let dry = edition_env(&work, &["deploy", "staging-home", "main"], &env);
    let r = dry.receipt();
    let changed = edition_env(
        &work,
        &["deploy", "staging-home", "main", "--apply", "--confirm", &r],
        &[("EDITION_VAR_TOKEN", "other-value")],
    );
    assert_eq!(changed.code, 2, "{}", changed.all());
    assert!(changed.stderr.contains("does not match this plan"));
}

#[test]
fn deploy_commands_get_the_commit_never_a_ref_name() {
    let (_root, work) = project("sha-only", BASE_CONFIG, "0.6.0-dev");
    let cfg = BASE_CONFIG.replace("\"staging-deploys.txt\", \"{commit}\"", "\"staging-deploys.txt\", \"{ref}\"");
    write(&work, "edition.toml", &cfg);
    let out = edition(&work, &["status"]);
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("unknown placeholder {ref}"), "{}", out.stderr);
}

#[test]
fn pushes_are_compare_and_swap() {
    let (root, work) = project("cas", BASE_CONFIG, "0.7.0-dev");
    line_with_version(&work, "0.6", "0.6.0");
    let dry = edition(&work, &["tag", "0.6.0"]);
    assert!(
        dry.stdout.contains("push --atomic '--force-with-lease=refs/tags/v0.6.0:' origin refs/tags/v0.6.0")
            || dry.stdout.contains("push --atomic --force-with-lease=refs/tags/v0.6.0: origin refs/tags/v0.6.0"),
        "{}",
        dry.stdout
    );
    let cut = edition(&work, &["cut", "0.7"]);
    assert!(cut.stdout.contains("--force-with-lease=refs/heads/release/0.7:"), "{}", cut.stdout);
    // The lease refuses a tag that appeared on the remote meanwhile.
    let other = other_clone(&root, "other");
    git(&other, &["tag", "v0.6.0", "origin/main"]);
    git(&other, &["push", "-q", "origin", "v0.6.0"]);
    git(&work, &["tag", "-a", "v0.6.0", "origin/release/0.6", "-m", "mine"]);
    let lease = std::process::Command::new("git")
        .current_dir(&work)
        .args(["push", "--atomic", "--force-with-lease=refs/tags/v0.6.0:", "origin", "refs/tags/v0.6.0"])
        .output()
        .expect("git");
    assert!(!lease.status.success(), "the lease must refuse an existing tag");
}

#[test]
fn verify_anchors_the_ledger_to_the_remote() {
    let (_root, work) = two_entry_ledger("anchor");
    let ok = edition(&work, &["verify", "--online"]);
    assert!(ok.stdout.contains("extends the ledger on origin/main"), "{}", ok.stdout);
    let offline = edition(&work, &["verify"]);
    assert!(offline.stdout.contains("remote anchor is unverified offline"), "{}", offline.stdout);
    let path = work.join("releases/ledger.toml");
    let text = std::fs::read_to_string(&path).expect("ledger");
    write(&work, "releases/ledger.toml", &text.replacen("note = \"first\"", "note = \"rewritten\"", 1));
    let bad = edition(&work, &["verify", "--online"]);
    assert_eq!(bad.code, 1);
    assert!(bad.stdout.contains("does not extend the ledger on origin/main"), "{}", bad.stdout);
}

#[test]
fn the_scanner_fails_closed() {
    let (_root, work) = public_project("fail-closed");
    let loaded = edition::config::Loaded::load(&work.join("edition.toml")).expect("config");
    let g = edition::git::Git::new(&work);
    let scanner = edition::content::Scanner::new(&loaded);
    assert!(scanner.scan_commits(&g, &["no-such-ref".to_string()]).is_err());

    // Binary content is reported as unscanned, in commits and in the tree, and blocks.
    std::fs::write(work.join("blob.bin"), [0u8, 1, 2, 0, 3]).expect("bin");
    commit_all(&work, "Binary");
    let push = edition(&work, &["check-push", "origin"]);
    assert_eq!(push.code, 1, "{}", push.all());
    assert!(push.stderr.contains("[unscanned] binary file"), "{}", push.stderr);
    let v = edition(&work, &["verify"]);
    assert_eq!(v.code, 1);
    assert!(v.stdout.contains("blob.bin [unscanned] binary file"), "{}", v.stdout);

    // An oversize file in the tree is reported as unscanned.
    git(&work, &["reset", "-q", "--hard", "origin/main"]);
    let big = "a".repeat(edition::content::MAX_SCAN_BYTES + 1);
    write(&work, "big.md", &big);
    commit_all(&work, "Big");
    git(&work, &["push", "-q", "origin", "main"]);
    let v2 = edition(&work, &["verify"]);
    assert_eq!(v2.code, 1);
    assert!(v2.stdout.contains("big.md [unscanned] larger than the scan limit"), "{}", v2.stdout);

    // An exempt path is the explicit way to accept such files.
    let cfg = std::fs::read_to_string(work.join("edition.toml"))
        .expect("cfg")
        .replace("allow_paths = [\"allowed/**\"]", "allow_paths = [\"allowed/**\", \"big.md\"]");
    write(&work, "edition.toml", &cfg);
    commit_all(&work, "Exempt big.md");
    git(&work, &["push", "-q", "origin", "main"]);
    let v3 = edition(&work, &["verify"]);
    assert!(!v3.stdout.contains("big.md [unscanned]"), "{}", v3.stdout);
}

#[test]
fn redemption_is_atomic_under_contention() {
    let dir = tmp("redeem-race");
    let receipts = std::sync::Arc::new(edition::steps::Receipts::at(&dir));
    let hash = "a".repeat(64);
    for _ in 0..40 {
        let nonce = receipts.issue(&hash, 5).expect("issue");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let r = receipts.clone();
                let b = barrier.clone();
                let n = nonce.clone();
                let h = hash.clone();
                std::thread::spawn(move || {
                    b.wait();
                    r.redeem(&n, &h, false).is_ok()
                })
            })
            .collect();
        let wins = handles.into_iter().map(|h| h.join().expect("thread")).filter(|w| *w).count();
        assert_eq!(wins, 1, "a receipt was redeemed {wins} times");
    }
}
