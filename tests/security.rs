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
    let ledger = ledger_text(&work);
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
    assert_eq!(apply(&work, &["deploy", "staging-home", "main", "--note", "second"]).code, 0);
    (root, work)
}

fn plumb(dir: &Path, args: &[&str], input: &str) -> String {
    use std::io::Write;
    let mut c = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Other", "-c", "user.email=other@example.invalid"])
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git");
    c.stdin.take().expect("stdin").write_all(input.as_bytes()).expect("write");
    let out = c.wait_with_output().expect("git");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Another clone publishes `text` as the ledger branch, on top of the current tip or
/// (when `rewrite`) on top of `parent` with a force push.
fn publish_ledger(root: &Path, name: &str, text: &str, parent: Option<&str>, force: bool) -> String {
    let other = other_clone(root, name);
    git(&other, &["fetch", "-q", "origin", "edition/ledger:refs/remotes/origin/edition/ledger"]);
    let blob = plumb(&other, &["hash-object", "-w", "--stdin"], text);
    let tree = plumb(&other, &["mktree"], &format!("100644 blob {blob}\tledger.toml\n"));
    let parent = parent.map(str::to_string).unwrap_or_else(|| git(&other, &["rev-parse", "origin/edition/ledger"]));
    let commit = plumb(&other, &["commit-tree", &tree, "-p", &parent, "-m", "edit"], "");
    let mut args = vec!["push", "-q"];
    if force {
        args.push("-f");
    }
    let spec = format!("{commit}:refs/heads/edition/ledger");
    args.extend(["origin", spec.as_str()]);
    git(&other, &args);
    commit
}

#[test]
fn a_recorded_staging_deploy_leaves_main_untouched_and_staging_current() {
    let (_root, work) = project("ledger-main", BASE_CONFIG, "0.6.0-dev");
    let main_before = git(&work, &["ls-remote", "origin", "refs/heads/main"]);
    let ok = apply(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert!(ok.stdout.contains("record the outcome on the edition/ledger branch"), "{}", ok.stdout);
    assert_eq!(git(&work, &["ls-remote", "origin", "refs/heads/main"]), main_before, "main did not move");
    assert!(git(&work, &["status", "--porcelain"]).is_empty(), "the checkout is untouched");
    assert!(ledger_text(&work).contains("tier = \"staging\""));
    // Staging is still current: the same tip is accepted again, and status says so.
    let again = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(again.code, 0, "{}", again.all());
    let status = edition(&work, &["status", "--online"]);
    let tip = git(&work, &["rev-parse", "origin/main"]);
    assert!(status.stdout.contains(&format!("main ({})", &tip[..10])), "{}", status.stdout);
}

#[test]
fn two_concurrent_recorders_both_land() {
    let (root, work) = project("ledger-race", BASE_CONFIG, "0.6.0-dev");
    let other = other_clone(&root, "other");
    git(&other, &["checkout", "-q", "main"]);
    let r1 = edition(&work, &["deploy", "staging-home", "main"]).receipt();
    let r2 = edition(&other, &["deploy", "staging-home", "main"]).receipt();
    let (w, o) = (work.clone(), other.clone());
    let t1 = std::thread::spawn(move || edition(&w, &["deploy", "staging-home", "main", "--apply", "--confirm", &r1]));
    let t2 = std::thread::spawn(move || edition(&o, &["deploy", "staging-home", "main", "--apply", "--confirm", &r2]));
    let (o1, o2) = (t1.join().expect("t1"), t2.join().expect("t2"));
    assert_eq!(o1.code, 0, "{}", o1.all());
    assert_eq!(o2.code, 0, "{}", o2.all());
    let text = ledger_text(&work);
    assert_eq!(text.matches("[[entry]]").count(), 2, "{text}");
    let v = edition(&work, &["verify", "--online"]);
    assert!(v.stdout.contains("hash chain intact (2 entries)"), "{}", v.stdout);
}

#[test]
fn edited_truncated_or_reordered_ledger_history_is_caught() {
    let (root, work) = two_entry_ledger("forgery");
    let v = edition(&work, &["verify", "--online"]);
    assert!(v.stdout.contains("hash chain intact (2 entries)"), "{}", v.stdout);
    let text = ledger_text(&work);

    // An edited entry, appended as a new commit (no force push needed).
    let edited = publish_ledger(&root, "o1", &text.replacen("note = \"first\"", "note = \"forged\"", 1), None, false);
    let forged = edition(&work, &["verify", "--online"]);
    assert_eq!(forged.code, 1, "{}", forged.all());
    assert!(forged.stdout.contains(&format!("invalid commit {}", &edited[..10])), "{}", forged.stdout);
    // Outward planning refuses it too, naming the commit.
    let d = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(d.code, 1, "{}", d.all());
    assert!(d.stderr.contains(&format!("invalid commit {}", &edited[..10])), "{}", d.stderr);
}

#[test]
fn a_truncated_or_reordered_ledger_commit_is_caught() {
    let (root, work) = two_entry_ledger("truncate");
    let text = ledger_text(&work);
    let first_end = text
        .find("\n[[entry]]")
        .and_then(|a| text[a + 1..].find("\n[[entry]]").map(|b| a + 1 + b))
        .expect("two entries");
    let trunc = publish_ledger(&root, "o1", &text[..first_end + 1], None, false);
    let t = edition(&work, &["verify", "--online"]);
    assert_eq!(t.code, 1, "{}", t.all());
    assert!(t.stdout.contains(&format!("invalid commit {}", &trunc[..10])), "{}", t.stdout);

    let (root2, work2) = two_entry_ledger("reorder");
    let text2 = ledger_text(&work2);
    let head = &text2[..text2.find("\n[[entry]]").expect("e")];
    let body = &text2[text2.find("\n[[entry]]").expect("entry")..];
    let second = body[1..].find("\n[[entry]]").map(|i| i + 1).expect("second");
    let re = publish_ledger(&root2, "o2", &format!("{head}{}{}", &body[second..], &body[..second]), None, false);
    let r = edition(&work2, &["verify", "--online"]);
    assert_eq!(r.code, 1, "{}", r.all());
    assert!(r.stdout.contains(&format!("invalid commit {}", &re[..10])), "{}", r.stdout);
}

#[test]
fn a_ledger_branch_rewritten_on_the_remote_is_detected() {
    let (root, work) = two_entry_ledger("rewrite");
    assert_eq!(edition(&work, &["verify", "--online"]).code, 0);
    // Someone force-pushes the branch back to its first commit plus a different one.
    let first = git(&work, &["rev-parse", "refs/edition/ledger~1"]);
    let one = git(&work, &["show", &format!("{first}:ledger.toml")]);
    publish_ledger(&root, "o1", &one, Some(&first), true);
    let v = edition(&work, &["verify", "--online"]);
    assert_eq!(v.code, 1, "{}", v.all());
    assert!(v.stdout.contains("branch was rewritten"), "{}", v.stdout);
    // Outward plans refuse to record onto it too.
    let d = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(d.code, 1, "{}", d.all());
    assert!(d.stderr.contains("branch was rewritten"), "{}", d.stderr);
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
    let ledger = ledger_text(&work);
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
    assert!(dry.stdout.contains("push --atomic --force-with-lease=refs/tags/v0.6.0: "), "{}", dry.stdout);
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

fn mirror_of(root: &Path) -> String {
    let mirror = root.join("mirror.git");
    git(
        root,
        &["clone", "-q", "--bare", root.join("origin.git").to_str().expect("utf8"), mirror.to_str().expect("utf8")],
    );
    mirror.display().to_string()
}

fn tag_ready(name: &str) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let (root, work) = project(name, BASE_CONFIG, "0.7.0-dev");
    line_with_version(&work, "0.6", "0.6.0");
    let mirror = mirror_of(&root);
    (root, work, mirror)
}

#[test]
fn a_remote_redirected_by_config_invalidates_the_receipt() {
    let (root, work, mirror) = tag_ready("redirect");
    let origin = root.join("origin.git").display().to_string();
    let dry = edition(&work, &["tag", "0.6.0"]);
    assert!(dry.stdout.contains(&format!("destination: {origin}")), "{}", dry.stdout);

    // url.<mirror>.insteadOf rewrites origin to an identical mirror.
    let r1 = dry.receipt();
    git(&work, &["config", &format!("url.{mirror}.insteadOf"), &origin]);
    let out = edition(&work, &["tag", "0.6.0", "--apply", "--confirm", &r1]);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(out.stderr.contains("does not match this plan"), "{}", out.stderr);
    git(&work, &["config", "--unset", &format!("url.{mirror}.insteadOf")]);

    // remote.origin.pushurl sends pushes elsewhere.
    let r2 = edition(&work, &["tag", "0.6.0"]).receipt();
    git(&work, &["config", "remote.origin.pushurl", &mirror]);
    let out2 = edition(&work, &["tag", "0.6.0", "--apply", "--confirm", &r2]);
    assert_eq!(out2.code, 2, "{}", out2.all());
    assert!(out2.stderr.contains("does not match this plan"), "{}", out2.stderr);
    git(&work, &["config", "--unset", "remote.origin.pushurl"]);
    assert!(!git(&work, &["ls-remote", "--tags", &mirror]).contains("v0.6.0"));
}

#[test]
fn the_callers_git_and_gh_environment_has_no_effect() {
    let (root, work, mirror) = tag_ready("env-redirect");
    let origin = root.join("origin.git").display().to_string();
    let env = [
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "remote.origin.pushurl"),
        ("GIT_CONFIG_VALUE_0", mirror.as_str()),
        ("GIT_DIR", "/nonexistent"),
        ("GIT_SSH_COMMAND", "false"),
        ("GH_HOST", "attacker.example"),
    ];
    let dry = edition_env(&work, &["tag", "0.6.0"], &env);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains(&format!("destination: {origin}")), "{}", dry.stdout);
    let ok = apply_env(&work, &["tag", "0.6.0"], &env);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert!(git(&work, &["ls-remote", "--tags", &origin]).contains("refs/tags/v0.6.0"));
    assert!(!git(&work, &["ls-remote", "--tags", &mirror]).contains("v0.6.0"), "the push did not go to the mirror");
}

#[test]
fn repository_hooks_do_not_run_during_edition_pushes() {
    let (root, work, _mirror) = tag_ready("hooks-path");
    let marker = root.join("hook-ran");
    executable(&root, "evil-hooks/pre-push", &format!("#!/bin/sh\ntouch '{}'\n", marker.display()));
    git(&work, &["config", "core.hooksPath", root.join("evil-hooks").to_str().expect("utf8")]);
    let ok = apply(&work, &["tag", "0.6.0"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert!(!marker.exists(), "a repository hook ran during Edition's push");
    // The same hook does run for an ordinary push, so the test is meaningful.
    git(&work, &["push", "-q", "origin", "main"]);
    assert!(marker.exists());
}

#[test]
fn the_receipt_file_must_not_be_a_link() {
    let (_root, work) = project("link", BASE_CONFIG, "0.6.0-dev");
    let r = staging_receipt(&work);
    let state = work.join(".edition/receipts");
    let moved = work.join(".edition/real");
    std::fs::rename(&state, &moved).expect("move");
    std::os::unix::fs::symlink(&moved, &state).expect("symlink");
    let out = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &r]);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(out.stderr.contains("not a link"), "{}", out.stderr);
}

fn assert_no_secret(work: &Path, outs: &[&Out], events: &Path) {
    for o in outs {
        assert!(!o.all().contains("SECRET"), "{}", o.all());
    }
    let l = ledger_text(work);
    assert!(!l.contains("SECRET"), "{l}");
    if let Ok(e) = std::fs::read_to_string(events) {
        assert!(!e.contains("SECRET"), "{e}");
    }
}

#[test]
fn credentials_in_remote_urls_are_never_shown_or_recorded() {
    let (root, work, _mirror) = tag_ready("creds");
    let origin = root.join("origin.git").display().to_string();
    let events = root.join("events.jsonl");
    let ev = events.to_str().expect("utf8").to_string();
    let env = [("EDITION_EVENTS", ev.as_str())];

    // Success: a credentialed URL that works.
    git(&work, &["config", "remote.origin.url", &format!("file://user:SECRET@localhost{origin}")]);
    let dry = edition_env(&work, &["tag", "0.6.0"], &env);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains(&format!("destination: file://localhost{origin}")), "{}", dry.stdout);
    let ok = edition_env(&work, &["tag", "0.6.0", "--apply", "--confirm", &dry.receipt()], &env);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert!(git(&work, &["ls-remote", "--tags", &origin]).contains("refs/tags/v0.6.0"));
    let deploy = apply_env(&work, &["deploy", "staging-home", "main"], &env);
    assert_eq!(deploy.code, 0, "{}", deploy.all());

    // Failure while pushing: the push URL is credentialed and wrong.
    git(&work, &["checkout", "-q", "release/0.6"]);
    set_version(&work, "0.6.1");
    commit_all(&work, "0.6.1");
    git(&work, &["push", "-q", &origin, "release/0.6"]);
    git(&work, &["checkout", "-q", "main"]);
    git(&work, &["config", "remote.origin.pushurl", "file://user:SECRET@localhost/nonexistent/repo.git"]);
    let dry2 = edition_env(&work, &["tag", "0.6.1"], &env);
    assert_eq!(dry2.code, 0, "{}", dry2.all());
    let failed = edition_env(&work, &["tag", "0.6.1", "--apply", "--confirm", &dry2.receipt()], &env);
    assert_ne!(failed.code, 0, "{}", failed.all());

    // Failure before planning: the fetch URL is credentialed and unreachable.
    git(&work, &["config", "--unset", "remote.origin.pushurl"]);
    git(&work, &["config", "remote.origin.url", "file://user:SECRET@localhost/nonexistent/repo.git"]);
    let refused = edition_env(&work, &["tag", "0.6.1"], &env);
    assert_ne!(refused.code, 0, "{}", refused.all());
    let verify = edition_env(&work, &["verify", "--online"], &env);

    assert_no_secret(&work, &[&dry, &ok, &deploy, &dry2, &failed, &refused, &verify], &events);
    // The failed push went to an unreachable URL, so its outcome could not be published
    // there either; Edition says so rather than pretending it was recorded.
    assert!(failed.stderr.contains("could not publish the ledger entry"), "{}", failed.stderr);
}

#[test]
fn the_receipt_directory_must_not_be_a_link() {
    use std::os::unix::fs::PermissionsExt;
    let (root, work) = project("dir-link", BASE_CONFIG, "0.6.0-dev");
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("mkdir");
    std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    std::os::unix::fs::symlink(&elsewhere, work.join(".edition")).expect("symlink");
    let out = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(out.stderr.contains("not a link"), "{}", out.stderr);
}

#[test]
fn declared_enterprise_tokens_reach_gh_but_the_host_comes_from_the_url() {
    let (root, work) = project("ghe", BASE_CONFIG, "0.6.0-dev");
    forge_alias(&root, &work);
    let cfg = format!("{BASE_CONFIG}{}tool_env = [\"GH_ENTERPRISE_TOKEN\"]\n", fake_gh(&root));
    write(&work, "edition.toml", &cfg);
    commit_all(&work, "Fake gh with an enterprise token");
    git(&work, &["push", "-q", "origin", "main"]);
    line_with_version(&work, "0.5", "0.5.2");
    git(&work, &["checkout", "-q", "release/0.5"]);
    write(&work, "fix.md", "fix");
    commit_all(&work, "Fix");
    git(&work, &["push", "-q", "origin", "release/0.5"]);
    git(&work, &["checkout", "-q", "main"]);
    let env = [("GH_ENTERPRISE_TOKEN", "t-123"), ("GH_HOST", "attacker.example"), ("GH_REPO", "evil/repo")];
    let out = apply_env(&work, &["forward"], &env);
    assert_eq!(out.code, 0, "{}", out.all());
    let calls = std::fs::read_to_string(root.join("bin/gh-calls.log")).expect("gh ran");
    assert!(calls.contains("GH_HOST=github.com TOKEN=t-123 pr create --repo github.com/example/lib"), "{calls}");

    let bad = format!("{BASE_CONFIG}\n[run]\ntool_env = [\"GH_HOST\"]\n");
    write(&work, "edition.toml", &bad);
    let refused = edition(&work, &["status"]);
    assert_eq!(refused.code, 2);
    assert!(refused.stderr.contains("GH_HOST may not be passed"), "{}", refused.stderr);
}

#[test]
fn credentials_in_any_config_text_never_reach_output_ledger_or_events() {
    let cfg = BASE_CONFIG
        .replace("target = \"https://staging.example.com\"", "target = \"https://user:SECRET@staging.example.com/\"")
        .replace(
            "run = [\"scripts/record.sh\", \"staging-deploys.txt\", \"{commit}\"]\nprobe = [\"true\"]",
            "run = [\"scripts/fail.sh\", \"https://user:SECRET@deploy.example.com/x?token=SECRET\", \"{var.site}\", \"{commit}\"]\nenv = { SITE = \"https://user:SECRET@env.example.com/\" }\nprobe = [\"true\", \"https://user:SECRET@probe.example.com/\"]",
        )
        .replace("[checks.unit]", "[vars]\nsite = \"https://user:SECRET@var.example.com/\"\n\n[checks.unit]");
    let (root, work) = project("creds-config", &cfg, "0.6.0-dev");
    executable(&work, "scripts/fail.sh", "#!/bin/sh\nexit 3\n");
    commit_all(&work, "Failing deploy script");
    git(&work, &["push", "-q", "origin", "main"]);
    let events = root.join("events.jsonl");
    let ev = events.to_str().expect("utf8").to_string();
    let env = [("EDITION_EVENTS", ev.as_str())];
    let note = "see https://user:SECRET@notes.example.com/page";
    let status = edition_env(&work, &["status"], &env);
    assert_eq!(status.code, 0, "{}", status.all());
    assert!(status.stdout.contains("https://staging.example.com/"), "{}", status.stdout);
    let plan = edition_env(&work, &["plan", "0.6.0"], &env);
    let dry = edition_env(&work, &["deploy", "staging-home", "main", "--note", note], &env);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains("https://deploy.example.com/x"), "{}", dry.stdout);
    let failed = edition_env(
        &work,
        &["deploy", "staging-home", "main", "--note", note, "--apply", "--confirm", &dry.receipt()],
        &env,
    );
    assert_eq!(failed.code, 1, "{}", failed.all());
    let verify = edition_env(&work, &["verify", "--online"], &env);
    assert_no_secret(&work, &[&status, &plan, &dry, &failed, &verify], &events);
    let ledger = ledger_text(&work);
    assert!(ledger.contains("result = \"failed\""), "{ledger}");
    assert!(ledger.contains("notes.example.com/page"), "{ledger}");
}

fn home_config(path: &str) -> String {
    format!("{BASE_CONFIG}\n[run]\npath = \"{path}\"\n")
}

#[test]
fn run_path_refuses_an_untrustworthy_home_or_directory() {
    use std::os::unix::fs::PermissionsExt;
    let (root, work) = project("run-path", BASE_CONFIG, "0.6.0-dev");
    write(&work, "edition.toml", &home_config("~/bin:/usr/bin:/bin"));
    let relative = edition_env(&work, &["status"], &[("HOME", "relative/home")]);
    assert_eq!(relative.code, 2, "{}", relative.all());
    assert!(relative.stderr.contains("not an absolute path"), "{}", relative.stderr);
    let colon = edition_env(&work, &["status"], &[("HOME", "/tmp/a:/tmp/b")]);
    assert_eq!(colon.code, 2, "{}", colon.all());
    assert!(colon.stderr.contains("without `:`"), "{}", colon.stderr);
    let fine = edition_env(&work, &["status"], &[("HOME", root.to_str().expect("utf8"))]);
    assert_eq!(fine.code, 0, "{}", fine.all());

    let open = root.join("open-bin");
    std::fs::create_dir(&open).expect("mkdir");
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).expect("chmod");
    write(&work, "edition.toml", &home_config(&format!("{}:/usr/bin:/bin", open.display())));
    let writable = edition(&work, &["status"]);
    assert_eq!(writable.code, 2, "{}", writable.all());
    assert!(writable.stderr.contains("writable by others"), "{}", writable.stderr);
    write(&work, "edition.toml", &home_config("bin:/usr/bin"));
    assert_eq!(edition(&work, &["status"]).code, 2);
}

#[test]
fn run_path_refuses_group_writable_foreign_or_exposed_directories() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let (root, work) = project("run-path-trust", BASE_CONFIG, "0.6.0-dev");

    let group = root.join("group-bin");
    std::fs::create_dir(&group).expect("mkdir");
    std::fs::set_permissions(&group, std::fs::Permissions::from_mode(0o775)).expect("chmod");
    write(&work, "edition.toml", &home_config(&format!("{}:/usr/bin:/bin", group.display())));
    let g = edition(&work, &["status"]);
    assert_eq!(g.code, 2, "{}", g.all());
    assert!(g.stderr.contains("run.path entry") && g.stderr.contains("writable by its group"), "{}", g.stderr);

    // A non-sticky writable ancestor lets the entry be swapped.
    let exposed = root.join("exposed");
    let inner = exposed.join("bin");
    std::fs::create_dir_all(&inner).expect("mkdir");
    std::fs::set_permissions(&exposed, std::fs::Permissions::from_mode(0o777)).expect("chmod");
    write(&work, "edition.toml", &home_config(&format!("{}:/usr/bin:/bin", inner.display())));
    let a = edition(&work, &["status"]);
    assert_eq!(a.code, 2, "{}", a.all());
    assert!(a.stderr.contains("run.path ancestor"), "{}", a.stderr);
    std::fs::set_permissions(&exposed, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_eq!(edition(&work, &["status"]).code, 0);

    // An entry owned by another (non-root) user, if this system has one we can see.
    let me = std::fs::metadata(&work).expect("meta").uid();
    let foreign = ["/private/var/db", "/var/db", "/var/lib", "/usr/local"]
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            std::fs::metadata(p)
                .is_ok_and(|m| m.is_dir() && m.uid() != 0 && m.uid() != me && m.permissions().mode() & 0o022 == 0)
        });
    match foreign {
        Some(dir) => {
            write(&work, "edition.toml", &home_config(&format!("{}:/usr/bin:/bin", dir.display())));
            let f = edition(&work, &["status"]);
            assert_eq!(f.code, 2, "{}", f.all());
            assert!(f.stderr.contains("owned by another user"), "{}", f.stderr);
        }
        None => eprintln!("skipped: no directory owned by another non-root user is visible on this system"),
    }
}

#[test]
fn a_group_writable_directory_is_trusted_only_when_listed() {
    use std::os::unix::fs::PermissionsExt;
    let (root, work) = project("trust-group", BASE_CONFIG, "0.6.0-dev");
    let group = root.join("group-bin");
    std::fs::create_dir(&group).expect("mkdir");
    std::fs::set_permissions(&group, std::fs::Permissions::from_mode(0o775)).expect("chmod");
    let dir = group.display().to_string();
    write(&work, "edition.toml", &home_config(&format!("{dir}:/usr/bin:/bin")));
    let refused = edition(&work, &["status"]);
    assert_eq!(refused.code, 2, "{}", refused.all());
    assert!(refused.stderr.contains("trust_group_writable"), "{}", refused.stderr);

    let cfg = format!("{BASE_CONFIG}\n[run]\npath = \"{dir}:/usr/bin:/bin\"\ntrust_group_writable = [\"{dir}\"]\n");
    write(&work, "edition.toml", &cfg);
    commit_all(&work, "Trust the group directory");
    git(&work, &["push", "-q", "origin", "main"]);
    let dry = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains(&format!("trusting group-writable PATH directories: {dir}")), "{}", dry.stdout);

    // A path that is not canonical is rejected rather than matched loosely.
    let loose = format!("{BASE_CONFIG}\n[run]\npath = \"{dir}:/usr/bin:/bin\"\ntrust_group_writable = [\"{dir}/\"]\n");
    write(&work, "edition.toml", &loose);
    assert_eq!(edition(&work, &["status"]).code, 2);
}

#[test]
fn a_missing_program_names_the_default_directories_skipped_as_group_writable() {
    use std::os::unix::fs::PermissionsExt;
    let skipped: Vec<&str> = edition::steps::DEFAULT_PATH
        .split(':')
        .filter(|d| std::fs::metadata(d).is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o020 != 0))
        .collect();
    let Some(dir) = skipped.first() else {
        eprintln!("skipped: no directory in the default PATH is group-writable on this system");
        return;
    };
    let cfg = BASE_CONFIG.replace(
        "run = [\"scripts/record.sh\", \"staging-deploys.txt\", \"{commit}\"]",
        "run = [\"no-such-edition-tool\", \"{commit}\"]",
    );
    let (_root, work) = project("missing-tool", &cfg, "0.6.0-dev");
    let out = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stderr.contains("skipped for being group-writable"), "{}", out.stderr);
    let canon = std::fs::canonicalize(dir).expect("canon").display().to_string();
    assert!(out.stderr.contains(&canon), "{}", out.stderr);
    assert!(out.stderr.contains("trust_group_writable"), "{}", out.stderr);
}

fn write_local(work: &Path, text: &str) {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = work.join(".edition");
    if !dir.exists() {
        std::fs::DirBuilder::new().mode(0o700).create(&dir).expect("mkdir");
    }
    std::fs::write(dir.join(".gitignore"), "*\n").expect("ignore");
    let p = dir.join("local.toml");
    std::fs::write(&p, text).expect("write");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).expect("chmod");
}

#[test]
fn per_machine_path_entries_follow_the_rules_are_shown_and_bound() {
    use std::os::unix::fs::PermissionsExt;
    let (root, work) = project("local-path", BASE_CONFIG, "0.6.0-dev");
    let bin = root.join("local-bin");
    std::fs::create_dir(&bin).expect("mkdir");
    let canon = std::fs::canonicalize(&bin).expect("canon").display().to_string();

    // Receipt binding: a dry run without the local file, then the file appears.
    let r = staging_receipt(&work);
    write_local(&work, &format!("[run]\npath_prepend = [\"{canon}\"]\n"));
    let changed = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &r]);
    assert_eq!(changed.code, 2, "{}", changed.all());
    assert!(changed.stderr.contains("does not match this plan"), "{}", changed.stderr);

    // Display, in the plan and in status; and the file is never committed.
    let dry = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains(&format!("local path entries: {canon}")), "{}", dry.stdout);
    assert!(edition(&work, &["status"]).stdout.contains(&format!("local path entries: {canon}")));
    assert!(git(&work, &["status", "--porcelain"]).is_empty());

    // The same directory rules: group-writable is refused unless trusted locally.
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o775)).expect("chmod");
    let g = edition(&work, &["status"]);
    assert_eq!(g.code, 2, "{}", g.all());
    assert!(g.stderr.contains(".edition/local.toml") && g.stderr.contains("trust_group_writable"), "{}", g.stderr);
    write_local(&work, &format!("[run]\npath_append = [\"{canon}\"]\ntrust_group_writable = [\"{canon}\"]\n"));
    let trusted = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(trusted.code, 0, "{}", trusted.all());
    assert!(
        trusted.stdout.contains(&format!("trusting group-writable PATH directories: {canon}")),
        "{}",
        trusted.stdout
    );
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o777)).expect("chmod");
    assert_eq!(edition(&work, &["status"]).code, 2, "other-writable is always refused");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // The local file itself must not be writable by others or relative.
    write_local(&work, "[run]\npath_prepend = [\"relative/bin\"]\n");
    assert_eq!(edition(&work, &["status"]).code, 2);
    write_local(&work, &format!("[run]\npath_prepend = [\"{canon}\"]\n"));
    std::fs::set_permissions(work.join(".edition/local.toml"), std::fs::Permissions::from_mode(0o666)).expect("chmod");
    let open = edition(&work, &["status"]);
    assert_eq!(open.code, 2, "{}", open.all());
    assert!(open.stderr.contains("not writable by group or others"), "{}", open.stderr);
}

#[test]
fn an_in_tree_ledger_migrates_to_the_branch_once() {
    let (_root, work) = project("migrate", BASE_CONFIG, "0.6.0-dev");
    let g = edition::git::Git::new(&work);
    let path = work.join("releases/ledger.toml");
    for i in 0..2 {
        let e = edition::ledger::Entry {
            id: format!("legacy-{i}"),
            at: NOW.into(),
            by: "test".into(),
            action: "deploy".into(),
            product: "lib".into(),
            channel: Some("staging-home".into()),
            ref_: "main".into(),
            commit: git(&work, &["rev-parse", "HEAD"]),
            result: "ok".into(),
            ..Default::default()
        };
        edition::ledger::append(&g, &path, &e).expect("append");
    }
    commit_all(&work, "Legacy ledger");
    git(&work, &["push", "-q", "origin", "main"]);
    let v = edition(&work, &["verify"]);
    assert!(v.stdout.contains("edition ledger migrate"), "{}", v.stdout);

    let dry = edition(&work, &["ledger", "migrate"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains("Move 2 ledger entries"), "{}", dry.stdout);
    assert!(ledger_text(&work).is_empty(), "a dry run publishes nothing");
    let ok = apply(&work, &["ledger", "migrate"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let text = ledger_text(&work);
    assert_eq!(text.matches("[[entry]]").count(), 2, "{text}");
    let commits = git(&work, &["rev-list", "--count", "refs/edition/ledger"]);
    assert_eq!(commits, "2", "one commit per entry");
    let listed = edition(&work, &["ledger"]);
    assert_eq!(listed.stdout.matches("deploy").count(), 2, "{}", listed.stdout);
    let v2 = edition(&work, &["verify", "--online"]);
    assert!(v2.stdout.contains("hash chain intact (2 entries)"), "{}", v2.stdout);
    assert_eq!(edition(&work, &["ledger", "migrate"]).code, 1, "a second migration is refused");
}

#[test]
fn version_help_and_the_detached_head_hint() {
    let (_root, work) = project("cli", BASE_CONFIG, "0.6.0-dev");
    let v = edition(&work, &["--version"]);
    assert_eq!(v.code, 0);
    assert!(v.stdout.starts_with("edition "), "{}", v.stdout);
    assert_eq!(edition(&work, &["-V"]).code, 0);
    let h = edition(&work, &["deploy", "--help"]);
    assert_eq!(h.code, 0);
    assert!(h.stdout.contains("USAGE"));
    git(&work, &["checkout", "-q", "--detach", "HEAD"]);
    let d = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(d.code, 1);
    assert!(d.stderr.contains("a branch at the same commit as the remote tip is enough"), "{}", d.stderr);
}

fn entry(id: &str, prev: &str) -> edition::ledger::Entry {
    edition::ledger::Entry {
        id: id.into(),
        at: NOW.into(),
        by: "test".into(),
        action: "deploy".into(),
        product: "lib".into(),
        channel: Some("staging-home".into()),
        ref_: "main".into(),
        commit: "0".repeat(40),
        result: "ok".into(),
        prev: prev.into(),
        ..Default::default()
    }
}

#[test]
fn a_fast_forward_that_replaces_or_deletes_the_ledger_is_refused() {
    // A fresh, internally valid chain published on top of the real one.
    let (root, work) = two_entry_ledger("ff-forgery");
    let forged =
        format!("{}{}", edition::ledger::HEADER, edition::ledger::render(&entry("forged", edition::ledger::GENESIS)));
    let c = publish_ledger(&root, "o1", &forged, None, false);
    let d = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(d.code, 1, "{}", d.all());
    assert!(d.stderr.contains(&format!("invalid commit {}", &c[..10])), "{}", d.stderr);

    // A commit that deletes the file.
    let (root2, work2) = two_entry_ledger("ff-delete");
    let other = other_clone(&root2, "o2");
    git(&other, &["fetch", "-q", "origin", "edition/ledger:refs/remotes/origin/edition/ledger"]);
    let empty = plumb(&other, &["mktree"], "");
    let parent = git(&other, &["rev-parse", "origin/edition/ledger"]);
    let del = plumb(&other, &["commit-tree", &empty, "-p", &parent, "-m", "delete"], "");
    git(&other, &["push", "-q", "origin", &format!("{del}:refs/heads/edition/ledger")]);
    let d2 = edition(&work2, &["deploy", "staging-home", "main"]);
    assert_eq!(d2.code, 1, "{}", d2.all());
    assert!(d2.stderr.contains(&format!("invalid commit {}", &del[..10])), "{}", d2.stderr);

    // A clone that has never seen the branch validates it from its first commit.
    let fresh = other_clone(&root2, "fresh");
    git(&fresh, &["checkout", "-q", "main"]);
    let d3 = edition(&fresh, &["deploy", "staging-home", "main"]);
    assert_eq!(d3.code, 1, "{}", d3.all());
    assert!(d3.stderr.contains("invalid commit"), "{}", d3.stderr);
    let (root3, _work3) = two_entry_ledger("ff-fresh-ok");
    let fresh_ok = other_clone(&root3, "fresh");
    git(&fresh_ok, &["checkout", "-q", "main"]);
    assert_eq!(
        edition(&fresh_ok, &["deploy", "staging-home", "main"]).code,
        0,
        "a valid branch is accepted from scratch"
    );
}

#[test]
fn a_lost_push_answer_does_not_duplicate_the_entry() {
    let (root, work) = project("dedupe", BASE_CONFIG, "0.6.0-dev");
    let origin = root.join("origin.git").display().to_string();
    let g = edition::git::Git::new(&work);
    let store = edition::ledger_store::Store::new(&g, "edition/ledger");
    let first = store.append(&origin, &origin, &entry("one", "")).expect("append");
    let before = git(&work, &["rev-parse", edition::ledger_store::CACHE]);
    store.append(&origin, &origin, &entry("two", "")).expect("append");
    // The push of "two" went through, but this recorder never learned it.
    git(&work, &["update-ref", edition::ledger_store::CACHE, &before]);
    let again = store.append(&origin, &origin, &entry("two", "")).expect("retry");
    assert_eq!(again.id, "two");
    let text = ledger_text(&work);
    assert_eq!(text.matches("[[entry]]").count(), 2, "{text}");
    let _ = first;
}

#[test]
fn an_outcome_survives_a_crash_after_the_outward_step_and_is_reconciled_once() {
    let cfg = BASE_CONFIG.replace(
        "run = [\"scripts/record.sh\", \"staging-deploys.txt\", \"{commit}\"]",
        "run = [\"scripts/deploy-then-die.sh\", \"{commit}\"]",
    );
    let (_root, work) = project("crash", &cfg, "0.6.0-dev");
    executable(&work, "scripts/deploy-then-die.sh", "#!/bin/sh\necho \"$1\" >> staging-deploys.txt\nkill -9 $PPID\n");
    commit_all(&work, "Deploy script that dies");
    git(&work, &["push", "-q", "origin", "main"]);
    let crashed = apply(&work, &["deploy", "staging-home", "main"]);
    assert_ne!(crashed.code, 0, "{}", crashed.all());
    assert!(work.join("staging-deploys.txt").exists(), "the deploy itself happened");
    assert!(ledger_text(&work).is_empty(), "nothing reached the ledger");

    let status = edition(&work, &["status"]);
    assert!(status.stdout.contains("UNRECONCILED"), "{}", status.stdout);
    let v = edition(&work, &["verify"]);
    assert!(v.stdout.contains("UNRECONCILED outcome"), "{}", v.stdout);
    let next = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(next.code, 1, "{}", next.all());
    assert!(next.stderr.contains("edition ledger reconcile"), "{}", next.stderr);

    let dry = edition(&work, &["ledger", "reconcile"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(ledger_text(&work).is_empty(), "a dry run publishes nothing");
    let ok = apply(&work, &["ledger", "reconcile"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let text = ledger_text(&work);
    assert_eq!(text.matches("[[entry]]").count(), 1, "{text}");
    assert!(text.contains("result = \"unknown\""), "{text}");
    assert_eq!(
        std::fs::read_to_string(work.join("staging-deploys.txt")).expect("d").lines().count(),
        1,
        "nothing re-ran"
    );
    assert!(edition(&work, &["ledger", "reconcile"]).stdout.contains("Nothing is pending"));
}

#[test]
fn an_unreachable_remote_after_the_deploy_keeps_the_outcome_and_abandon_is_recorded() {
    let (root, work) = project("unreachable", BASE_CONFIG, "0.6.0-dev");
    let origin = root.join("origin.git");
    let gone = root.join("gone.git");
    executable(
        &work,
        "scripts/deploy-then-cut.sh",
        &format!("#!/bin/sh\necho \"$1\" >> staging-deploys.txt\nmv '{}' '{}'\n", origin.display(), gone.display()),
    );
    let cfg = BASE_CONFIG.replace(
        "run = [\"scripts/record.sh\", \"staging-deploys.txt\", \"{commit}\"]",
        "run = [\"scripts/deploy-then-cut.sh\", \"{commit}\"]",
    );
    write(&work, "edition.toml", &cfg);
    commit_all(&work, "Deploy script that cuts the remote");
    git(&work, &["push", "-q", "origin", "main"]);
    let out = apply(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(out.stderr.contains("kept in .edition/pending"), "{}", out.stderr);
    std::fs::rename(&gone, &origin).expect("restore");
    let ok = apply(&work, &["ledger", "reconcile"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let text = ledger_text(&work);
    assert_eq!(text.matches("[[entry]]").count(), 1, "{text}");
    assert!(text.contains("result = \"ok\""), "{text}");

    // Abandoning is itself recorded.
    let cfg2 = cfg.replace(
        "\"scripts/deploy-then-cut.sh\", \"{commit}\"",
        "\"scripts/deploy-then-cut.sh\", \"{commit}\", \"again\"",
    );
    write(&work, "edition.toml", &cfg2);
    commit_all(&work, "Again");
    git(&work, &["push", "-q", "origin", "main"]);
    assert_eq!(apply(&work, &["deploy", "staging-home", "main"]).code, 2);
    std::fs::rename(&gone, &origin).expect("restore");
    let ab = apply(&work, &["ledger", "reconcile", "--abandon"]);
    assert_eq!(ab.code, 0, "{}", ab.all());
    assert!(ledger_text(&work).contains("result = \"abandoned\""));
}

#[test]
fn a_ledger_entry_bound_for_a_public_remote_is_scanned_when_planned_and_overrides_are_bound() {
    let (_root, work) = public_project("ledger-scan");
    let note = format!("Decisions {} Alex", "for");

    // Found while planning: the dry run is refused and names the finding.
    let dry = edition(&work, &["deploy", "staging-home", "main", "--note", &note]);
    assert_eq!(dry.code, 1, "{}", dry.all());
    assert!(dry.stderr.contains("ledger entry (deploy staging-home)"), "{}", dry.stderr);
    let id = dry
        .stderr
        .split("--allow-finding ")
        .nth(1)
        .map(|s| s.chars().take(10).collect::<String>())
        .expect("finding id");

    // A receipt issued without the override cannot be applied with one.
    let clean = edition(&work, &["deploy", "staging-home", "main"]).receipt();
    let swapped = edition(
        &work,
        &["deploy", "staging-home", "main", "--note", &note, "--allow-finding", &id, "--apply", "--confirm", &clean],
    );
    assert_eq!(swapped.code, 2, "{}", swapped.all());
    assert!(swapped.stderr.contains("does not match this plan"), "{}", swapped.stderr);
    assert!(ledger_text(&work).is_empty());

    // Allowed in the dry run: shown, bound, and recorded.
    let allowed = edition(&work, &["deploy", "staging-home", "main", "--note", &note, "--allow-finding", &id]);
    assert_eq!(allowed.code, 0, "{}", allowed.all());
    assert!(
        allowed.stdout.contains(&format!("allowing content findings in the ledger entry: {id}")),
        "{}",
        allowed.stdout
    );
    let without = edition(
        &work,
        &["deploy", "staging-home", "main", "--note", &note, "--apply", "--confirm", &allowed.receipt()],
    );
    assert_ne!(without.code, 0, "the same receipt without the override is refused: {}", without.all());
    let r = edition(&work, &["deploy", "staging-home", "main", "--note", &note, "--allow-finding", &id]).receipt();
    let ok = edition(
        &work,
        &["deploy", "staging-home", "main", "--note", &note, "--allow-finding", &id, "--apply", "--confirm", &r],
    );
    assert_eq!(ok.code, 0, "{}", ok.all());
    let text = ledger_text(&work);
    assert!(text.contains(&format!("{id} ledger entry (deploy staging-home)")), "{text}");
}

fn snapshot(dir: &Path) -> Vec<(String, u64)> {
    fn walk(p: &Path, out: &mut Vec<(String, u64)>) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let path = e.path();
                let m = std::fs::symlink_metadata(&path).expect("meta");
                out.push((path.display().to_string(), m.len()));
                if m.is_dir() && !m.file_type().is_symlink() {
                    walk(&path, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

#[test]
fn a_hostile_confirm_value_touches_nothing() {
    let (root, work) = project("confirm-path", BASE_CONFIG, "0.6.0-dev");
    staging_receipt(&work);
    let victim = root.join("victim.toml");
    std::fs::write(&victim, "keep me").expect("victim");
    let before = snapshot(&root);
    for bad in [
        "../../victim",
        "../../../victim",
        "/tmp/victim",
        &victim.display().to_string(),
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcde",
        &"a".repeat(4096),
        "0123456789abcdef0123456789abcdeg",
    ] {
        let out = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", bad]);
        assert_eq!(out.code, 2, "{bad}: {}", out.all());
        assert!(out.stderr.contains("32 lowercase hex"), "{bad}: {}", out.stderr);
    }
    // A well-formed receipt that is refused also creates and removes nothing.
    let refused = edition(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &"0".repeat(32)]);
    assert_eq!(refused.code, 2, "{}", refused.all());
    assert_eq!(snapshot(&root), before, "the filesystem is unchanged");
    assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "keep me");
    assert!(
        !work.join(".edition/pending").exists()
            || std::fs::read_dir(work.join(".edition/pending")).expect("dir").next().is_none()
    );
}

#[test]
fn configured_paths_stay_inside_the_repository() {
    let (_root, work) = project("cfg-paths", BASE_CONFIG, "0.6.0-dev");
    for (from, to) in [
        ("path = \"Cargo.toml\"", "path = \"../outside/Cargo.toml\""),
        ("path = \"Cargo.toml\"", "path = \"/etc/hosts\""),
    ] {
        write(&work, "edition.toml", &BASE_CONFIG.replace(from, to));
        let out = edition(&work, &["status"]);
        assert_eq!(out.code, 2, "{to}: {}", out.all());
        assert!(out.stderr.contains("inside the repository"), "{}", out.stderr);
    }
    write(&work, "edition.toml", &format!("ledger = \"../elsewhere.toml\"\n{BASE_CONFIG}"));
    assert_eq!(edition(&work, &["status"]).code, 2);
    write(&work, "edition.toml", &format!("ledger_branch = \"main\"\n{BASE_CONFIG}"));
    assert_eq!(edition(&work, &["status"]).code, 2);
    write(&work, "edition.toml", &format!("ledger_branch = \"../x\"\n{BASE_CONFIG}"));
    assert_eq!(edition(&work, &["status"]).code, 2);
}

#[test]
fn the_events_file_is_never_written_through_a_link() {
    let (root, work) = project("events-link", BASE_CONFIG, "0.6.0-dev");
    let target = root.join("target.txt");
    std::fs::write(&target, "untouched").expect("target");
    let link = root.join("events.jsonl");
    std::os::unix::fs::symlink(&target, &link).expect("link");
    let out =
        apply_env(&work, &["deploy", "staging-home", "main"], &[("EDITION_EVENTS", link.to_str().expect("utf8"))]);
    assert_eq!(out.code, 0, "{}", out.all());
    assert_eq!(std::fs::read_to_string(&target).expect("target"), "untouched");
}

#[test]
fn local_toml_must_be_ignored_and_untracked() {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let (_root, work) = project("local-git", BASE_CONFIG, "0.6.0-dev");
    let dir = work.join(".edition");
    std::fs::DirBuilder::new().mode(0o700).create(&dir).expect("mkdir");
    let p = dir.join("local.toml");
    std::fs::write(&p, "[run]\npath_append = [\"/usr/bin\"]\n").expect("write");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let not_ignored = edition(&work, &["status"]);
    assert_eq!(not_ignored.code, 2, "{}", not_ignored.all());
    assert!(not_ignored.stderr.contains("not ignored by git"), "{}", not_ignored.stderr);
    git(&work, &["add", "-f", ".edition/local.toml"]);
    let tracked = edition(&work, &["status"]);
    assert_eq!(tracked.code, 2, "{}", tracked.all());
    assert!(tracked.stderr.contains("tracked by git"), "{}", tracked.stderr);
    git(&work, &["rm", "-q", "--cached", ".edition/local.toml"]);
    std::fs::write(dir.join(".gitignore"), "*\n").expect("ignore");
    assert_eq!(edition(&work, &["status"]).code, 0);
}

/// Measures first-sync validation of a 5,000-entry ledger branch (run with --ignored).
#[test]
#[ignore]
fn validation_cost_for_5000_entries() {
    use std::io::Write;
    let dir = tmp("ledger-5000");
    git(&dir, &["init", "-q"]);
    let g = edition::git::Git::new(&dir);
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["fast-import", "--quiet"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("fast-import");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut text = edition::ledger::HEADER.to_string();
    let mut prev = edition::ledger::GENESIS.to_string();
    for i in 0..5000 {
        let e = entry(&format!("e{i}"), &prev);
        prev = edition::ledger::entry_hash(&g, &e).expect("hash");
        text.push_str(&edition::ledger::render(&e));
        write!(
            stdin,
            "commit refs/heads/edition/ledger\ncommitter T <t@example.invalid> 0 +0000\ndata 1\nx\nM 100644 inline ledger.toml\ndata {}\n{}\n",
            text.len(),
            text
        )
        .expect("write");
    }
    drop(stdin);
    assert!(child.wait().expect("wait").success());
    let tip = git(&dir, &["rev-parse", "refs/heads/edition/ledger"]);
    let store = edition::ledger_store::Store::new(&g, "edition/ledger");
    let start = std::time::Instant::now();
    store.validate_new(None, &tip).expect("valid");
    eprintln!("validated 5000 ledger commits in {:?}", start.elapsed());
}

#[test]
fn an_apply_only_finding_needs_an_allowance_bound_in_the_receipt() {
    let cfg = format!(
        "{}\n[remotes.origin]\npublic = true\n\n[content]\ndeny = [\"exit 3\"]\n",
        BASE_CONFIG.replace(
            "run = [\"scripts/record.sh\", \"staging-deploys.txt\", \"{commit}\"]",
            "run = [\"scripts/fail3.sh\", \"{commit}\"]",
        )
    );
    let (_root, work) = project("apply-only", &cfg, "0.6.0-dev");
    executable(&work, "scripts/fail3.sh", "#!/bin/sh\nexit 3\n");
    commit_all(&work, "A deploy that exits 3");
    git(&work, &["push", "-q", "origin", "main"]);

    // The finding exists only in the final entry ("...: exit 3"), so the plan cannot see it.
    let first = apply(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(first.code, 2, "{}", first.all());
    assert!(first.stderr.contains("not allowed when the plan was made"), "{}", first.stderr);
    let id: String =
        first.stderr.split("matches the content deny-list: ").nth(1).expect("finding").chars().take(10).collect();
    // Publishing that pending outcome needs the allowance in reconcile's own plan.
    let ok = apply(&work, &["ledger", "reconcile", "--abandon", "--allow-finding", &id]);
    assert_eq!(ok.code, 0, "{}", ok.all());

    // The id on the command line, absent from the receipt, does not authorize it.
    let dry = edition(&work, &["deploy", "staging-home", "main", "--allow-finding", &id]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(!dry.stdout.contains("allowing content findings"), "{}", dry.stdout);
    let again = edition(
        &work,
        &["deploy", "staging-home", "main", "--allow-finding", &id, "--apply", "--confirm", &dry.receipt()],
    );
    assert_eq!(again.code, 2, "{}", again.all());
    assert!(again.stderr.contains("not allowed when the plan was made"), "{}", again.stderr);
    assert!(edition(&work, &["status"]).stdout.contains("UNRECONCILED"));
}

#[test]
fn configured_files_are_never_read_or_written_through_a_link() {
    let (root, work) = project("symlinks", BASE_CONFIG, "0.6.0-dev");
    let outside = root.join("outside");
    std::fs::create_dir(&outside).expect("mkdir");
    std::fs::write(outside.join("Cargo.toml"), "[package]\nname = \"x\"\nversion = \"9.9.9\"\n").expect("write");

    // A symlinked version file.
    std::fs::remove_file(work.join("Cargo.toml")).expect("rm");
    std::os::unix::fs::symlink(outside.join("Cargo.toml"), work.join("Cargo.toml")).expect("link");
    let f = edition(&work, &["bump", "0.7.0", "--apply"]);
    assert_eq!(f.code, 2, "{}", f.all());
    assert!(f.stderr.contains("is a symbolic link"), "{}", f.stderr);
    std::fs::remove_file(work.join("Cargo.toml")).expect("rm");
    set_version(&work, "0.6.0-dev");

    // A symlinked parent directory.
    std::fs::remove_dir_all(work.join("pkg")).expect("rm");
    std::fs::create_dir(outside.join("pkg")).expect("mkdir");
    std::fs::write(outside.join("pkg/package.json"), "{\n  \"version\": \"9.9.9\"\n}\n").expect("write");
    std::os::unix::fs::symlink(outside.join("pkg"), work.join("pkg")).expect("link");
    let d = edition(&work, &["bump", "0.7.0", "--apply"]);
    assert_eq!(d.code, 2, "{}", d.all());
    assert!(d.stderr.contains("is a symbolic link"), "{}", d.stderr);
    std::fs::remove_file(work.join("pkg")).expect("rm");
    set_version(&work, "0.6.0-dev");

    // A link introduced between the dry run and the apply.
    assert_eq!(edition(&work, &["bump", "0.7.0"]).code, 0);
    std::fs::remove_file(work.join("Cargo.toml")).expect("rm");
    std::os::unix::fs::symlink(outside.join("Cargo.toml"), work.join("Cargo.toml")).expect("link");
    assert_eq!(edition(&work, &["bump", "0.7.0", "--apply"]).code, 2);
    assert!(
        std::fs::read_to_string(outside.join("Cargo.toml")).expect("read").contains("9.9.9"),
        "the outside file is untouched"
    );
    assert!(std::fs::read_to_string(outside.join("pkg/package.json")).expect("read").contains("9.9.9"));
}

#[test]
fn a_ledger_commit_that_changes_the_file_mode_is_refused() {
    let (root, work) = two_entry_ledger("mode");
    let text = ledger_text(&work);
    let other = other_clone(&root, "o1");
    git(&other, &["fetch", "-q", "origin", "edition/ledger:refs/remotes/origin/edition/ledger"]);
    let parent = git(&other, &["rev-parse", "origin/edition/ledger"]);
    let entries = edition::ledger::parse(&text).expect("parse");
    let g = edition::git::Git::new(&other);
    let prev = edition::ledger::entry_hash(&g, entries.last().expect("last")).expect("hash");
    let new = format!("{}\n{}", text.trim_end(), edition::ledger::render(&entry("valid-but-executable", &prev)));
    let blob = plumb(&other, &["hash-object", "-w", "--stdin"], &new);
    let tree = plumb(&other, &["mktree"], &format!("100755 blob {blob}\tledger.toml\n"));
    let c = plumb(&other, &["commit-tree", &tree, "-p", &parent, "-m", "exec"], "");
    git(&other, &["push", "-q", "origin", &format!("{c}:refs/heads/edition/ledger")]);
    let d = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(d.code, 1, "{}", d.all());
    assert!(d.stderr.contains(&format!("invalid commit {}", &c[..10])), "{}", d.stderr);
}

const HEADER_V1: &str = "# Edition ledger: what went where. Append-only; written by `edition`, never by hand.\n# A correction is a new entry; see Edition's docs/design.md.\n";

/// A format 1 in-tree ledger, chained with the repository's own object hashes.
fn write_format1_ledger(work: &Path, n: usize) {
    let g = edition::git::Git::new(work);
    let mut text = HEADER_V1.to_string();
    let mut prev = edition::ledger::GENESIS_V1.to_string();
    for i in 0..n {
        let mut e = entry(&format!("legacy-{i}"), &prev);
        e.commit = git(work, &["rev-parse", "HEAD"]);
        prev = edition::ledger::entry_hash_v1(&g, &e).expect("hash");
        text.push_str(&edition::ledger::render(&e));
    }
    write(work, "releases/ledger.toml", &text);
}

fn migrates_format1(work: &Path) {
    write_format1_ledger(work, 3);
    commit_all(work, "Format 1 ledger");
    git(work, &["push", "-q", "origin", "main"]);
    let dry = edition(work, &["ledger", "migrate"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains("re-chained from format 1"), "{}", dry.stdout);
    let ok = apply(work, &["ledger", "migrate"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let text = ledger_text(work);
    assert!(text.contains("format = 2"), "{text}");
    assert_eq!(text.matches("[[entry]]").count(), 4, "three entries and the migration record: {text}");
    assert!(text.contains("action = \"migrate\""));
    assert!(text.contains("re-chained 3 entries from ledger format 1"));
    let v = edition(work, &["verify", "--online"]);
    assert!(v.stdout.contains("hash chain intact (4 entries)"), "{}", v.stdout);
    // The chain continues normally afterwards.
    assert_eq!(apply(work, &["deploy", "staging-home", "main"]).code, 0);
    assert_eq!(ledger_text(work).matches("[[entry]]").count(), 5);
}

#[test]
fn a_format1_ledger_is_re_chained_by_migration() {
    let (_root, work) = project("format1", BASE_CONFIG, "0.6.0-dev");
    migrates_format1(&work);
}

#[test]
fn unknown_ledger_formats_are_refused() {
    assert!(edition::ledger::parse_ledger("format = 3\n").is_err());
    let none = edition::ledger::parse_ledger(&format!("{HEADER_V1}\n[[entry]]\nid = \"x\"\nat = \"a\"\nby = \"b\"\naction = \"deploy\"\nproduct = \"p\"\nref = \"main\"\ncommit = \"c\"\nresult = \"ok\"\n"));
    assert!(none.is_err_and(|e| e.contains("ledger migrate")));
    let (root, work) = project("format3", BASE_CONFIG, "0.6.0-dev");
    let other = other_clone(&root, "o1");
    let text = format!(
        "{}{}",
        edition::ledger::HEADER.replace("format = 2", "format = 3"),
        edition::ledger::render(&entry("x", edition::ledger::GENESIS))
    );
    let blob = plumb(&other, &["hash-object", "-w", "--stdin"], &text);
    let tree = plumb(&other, &["mktree"], &format!("100644 blob {blob}\tledger.toml\n"));
    let c = plumb(&other, &["commit-tree", &tree, "-m", "format 3"], "");
    git(&other, &["push", "-q", "origin", &format!("{c}:refs/heads/edition/ledger")]);
    let d = edition(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(d.code, 1, "{}", d.all());
    assert!(d.stderr.contains(&format!("invalid commit {}", &c[..10])), "{}", d.stderr);
}

fn sha256_project(name: &str) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let root = tmp(name);
    let probe = std::process::Command::new("git")
        .args(["init", "-q", "--bare", "--object-format=sha256"])
        .arg(root.join("origin.git"))
        .output()
        .expect("git");
    if !probe.status.success() {
        eprintln!("skipped: this git cannot create SHA-256 repositories");
        return None;
    }
    git(&root, &["clone", "-q", root.join("origin.git").to_str().expect("utf8"), "work"]);
    let work = root.join("work");
    git(&work, &["checkout", "-q", "-b", "main"]);
    write(&work, "edition.toml", BASE_CONFIG);
    write(&work, ".gitignore", "*deploys*.txt\nreview.txt\n");
    executable(&work, "scripts/record.sh", RECORD_SH);
    set_version(&work, "0.6.0-dev");
    let head = commit_all(&work, "Start");
    assert_eq!(head.len(), 64, "a SHA-256 repository");
    git(&work, &["push", "-q", "origin", "main"]);
    Some((root, work))
}

#[test]
fn the_ledger_works_the_same_in_a_sha256_repository() {
    let Some((_root, work)) = sha256_project("sha256") else { return };
    let ok = apply(&work, &["deploy", "staging-home", "main"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let v = edition(&work, &["verify", "--online"]);
    assert!(v.stdout.contains("hash chain intact (1 entries)"), "{}", v.stdout);
    let text = ledger_text(&work);
    assert!(text.contains(&format!("prev = \"{}\"", edition::ledger::GENESIS)), "{text}");
    eprintln!("ran in a SHA-256 repository");
}

#[test]
fn a_format1_ledger_from_a_sha256_repository_migrates() {
    let Some((_root, work)) = sha256_project("sha256-migrate") else { return };
    migrates_format1(&work);
}

fn system_config(root: &Path, text: &str, mode: u32) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = root.join("system-gitconfig");
    std::fs::write(&p, text).expect("write");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).expect("chmod");
    p.display().to_string()
}

#[test]
fn editions_git_reads_the_system_configuration() {
    // Edition no longer sets GIT_CONFIG_NOSYSTEM, so a credential helper or rewrite in
    // the system file reaches its git. The library's Git is the same command builder the
    // binary uses; the test points it at a system file the way git itself allows.
    let (root, work, _mirror) = tag_ready("system-config");
    let origin = root.join("origin.git").display().to_string();
    let sys = system_config(
        &root,
        &format!("[url \"{origin}\"]\n\tinsteadOf = https://git.example.invalid/lib.git\n[credential]\n\thelper = osxkeychain\n"),
        0o644,
    );
    let mut g = edition::git::Git::new(&work);
    assert!(!g.env.iter().any(|(k, _)| k == "GIT_CONFIG_NOSYSTEM"));
    g.env.push(("GIT_CONFIG_SYSTEM".into(), sys.clone()));
    let settings = g.config_settings().expect("settings");
    assert!(settings.iter().any(|(scope, origin, k, _)| scope == "system"
        && origin.as_deref() == Some(sys.as_str())
        && k == "credential.helper"));
    g.run(&["push", "-q", "https://git.example.invalid/lib.git", "refs/remotes/origin/main:refs/heads/through-system"])
        .expect("push via the system rewrite");
    assert!(git(&work, &["ls-remote", "--heads", &origin]).contains("through-system"));
}

#[test]
fn an_untrustworthy_config_file_is_refused_before_git_contacts_anything() {
    use std::os::unix::fs::PermissionsExt;
    let (root, work) = project("untrusted-config", BASE_CONFIG, "0.6.0-dev");
    let marker = root.join("ssh-ran");
    executable(&root, "evil-ssh", &format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()));
    git(&work, &["config", "core.sshCommand", root.join("evil-ssh").to_str().expect("utf8")]);
    git(&work, &["config", "remote.origin.url", "ssh://git.example.invalid/lib.git"]);
    std::fs::set_permissions(work.join(".git/config"), std::fs::Permissions::from_mode(0o664)).expect("chmod");
    for args in [
        vec!["deploy", "staging-home", "main"],
        vec!["status", "--online"],
        vec!["verify", "--online"],
        vec!["tag", "0.6.0"],
        vec!["forward"],
    ] {
        let out = edition(&work, &args);
        assert_ne!(out.code, 0, "{args:?}: {}", out.all());
        assert!(out.all().contains("is writable by its group or others"), "{args:?}: {}", out.all());
        assert!(!marker.exists(), "{args:?}: git ran the configured ssh command");
    }
    let offline = edition(&work, &["verify"]);
    assert!(offline.stdout.contains("offline, so nothing contacted it"), "{}", offline.stdout);
    assert!(!marker.exists());
}

#[test]
fn a_global_config_edit_between_dry_run_and_apply_invalidates_the_receipt() {
    let (root, work) = project("global-edit", BASE_CONFIG, "0.6.0-dev");
    let home = root.join("home");
    std::fs::create_dir(&home).expect("mkdir");
    write(&home, ".gitconfig", "[user]\n\tname = Test\n\temail = test@example.invalid\n");
    let h = home.display().to_string();
    let env = [("HOME", h.as_str())];
    let dry = edition_env(&work, &["deploy", "staging-home", "main"], &env);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains(&format!("{h}/.gitconfig (global)")), "{}", dry.stdout);
    write(
        &home,
        ".gitconfig",
        "[user]\n\tname = Test\n\temail = test@example.invalid\n[http]\n\tproxy = http://proxy.example.invalid\n",
    );
    let out = edition_env(&work, &["deploy", "staging-home", "main", "--apply", "--confirm", &dry.receipt()], &env);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(out.stderr.contains("does not match this plan"), "{}", out.stderr);
}

#[test]
fn a_callers_git_config_global_is_ignored() {
    let (root, work, mirror) = tag_ready("ignore-global");
    let origin = root.join("origin.git").display().to_string();
    let evil = system_config(&root, &format!("[url \"{mirror}\"]\n\tinsteadOf = {origin}\n"), 0o644);
    let env = [("GIT_CONFIG_GLOBAL", evil.as_str()), ("GIT_CONFIG_SYSTEM", evil.as_str())];
    let dry = edition_env(&work, &["tag", "0.6.0"], &env);
    assert!(dry.stdout.contains(&format!("destination: {origin}")), "{}", dry.stdout);
    let ok = apply_env(&work, &["tag", "0.6.0"], &env);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert!(!git(&work, &["ls-remote", "--tags", &mirror]).contains("v0.6.0"));
}

#[test]
fn a_signing_program_change_invalidates_a_tag_plan() {
    let (_root, work, _mirror) = tag_ready("gpg-program");
    let dry = edition(&work, &["tag", "0.6.0"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    git(&work, &["config", "gpg.program", "/usr/bin/true"]);
    let out = edition(&work, &["tag", "0.6.0", "--apply", "--confirm", &dry.receipt()]);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(out.stderr.contains("does not match this plan"), "{}", out.stderr);
    let again = edition(&work, &["tag", "0.6.0"]);
    assert!(again.stdout.contains("gpg.program (local)"), "{}", again.stdout);
}
