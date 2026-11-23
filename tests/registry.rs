//! Registry channels: the build label a library takes, what a probe may see, and the
//! tree a deploy's checks run on. Each protection has a test that fails without it.

mod common;
use common::*;
use std::path::{Path, PathBuf};

struct Project {
    work: PathBuf,
    /// Outside the repository: fake tools and what the scripts record.
    out: PathBuf,
}

impl Project {
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.out.join(name)).unwrap_or_default()
    }

    fn checks_dir(&self) -> PathBuf {
        self.work.join(".edition/checks")
    }

    /// Makes `.edition/checks` as Edition would (private), so a test can put things in it.
    fn make_checks_dir(&self) {
        use std::os::unix::fs::PermissionsExt;
        for d in [self.work.join(".edition"), self.checks_dir()] {
            std::fs::create_dir_all(&d).expect("mkdir");
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        }
    }

    /// Entries under `.edition/checks`, sorted.
    fn check_entries(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.checks_dir())
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }
}

/// A library at 0.7.0-dev on main with `v0.6.0` tagged on `release/0.6`, a registry
/// channel `npm`, and the extra configuration and files given. Check scripts record to
/// `<out>/checks.txt`; the probe records the token it sees to `<out>/probe.txt`.
fn registry(name: &str, channel_extra: &str, config_extra: &str, setup: &dyn Fn(&Path)) -> Project {
    let out = tmp(&format!("{name}-out"));
    executable(&out, "concordance", "#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/log-calls.log\"\n");
    let cfg = format!(
        r#"{BASE_CONFIG}
[platforms.js]

[concordance]
catalog = "spec/lib"
bin = "{bin}"

[vars]
token = ""
out = "{out}"

[checks.which]
run = ["scripts/which.sh", "{{var.out}}"]

[channels.npm]
tier = "prod"
kind = "registry"
target = "https://npm.example.com"
platform = "js"
checks = ["which"]
run = ["scripts/record.sh", "npm-deploys.txt", "{{commit}}"]
env = {{ TOKEN = "{{var.token}}" }}
probe = ["scripts/probe.sh", "{{var.out}}"]
{channel_extra}
{config_extra}
"#,
        bin = out.join("concordance").display(),
        out = out.display(),
    );
    let (_root, work) = project(name, &cfg, "0.7.0-dev");
    // Records where it ran, the version it saw and every file name it saw.
    executable(
        &work,
        "scripts/which.sh",
        "#!/bin/sh\n{ echo \"dir=$(pwd)\"; grep '^version' Cargo.toml; find . -type f | sort; echo end; } >> \"$1/checks.txt\"\n",
    );
    executable(&work, "scripts/probe.sh", "#!/bin/sh\nprintf 'token=%s\\n' \"$TOKEN\" >> \"$1/probe.txt\"\n");
    setup(&work);
    commit_all(&work, "Check and probe scripts");
    git(&work, &["push", "-q", "origin", "main"]);
    let line = line_with_version(&work, "0.6", "0.6.0");
    git(&work, &["tag", "-a", "v0.6.0", &line, "-m", "0.6.0"]);
    git(&work, &["push", "-q", "origin", "v0.6.0"]);
    Project { work, out }
}

fn plain(name: &str) -> Project {
    registry(name, "", "", &|_| {})
}

const TOKEN: [(&str, &str); 1] = [("EDITION_VAR_TOKEN", "s3cret-token")];

fn deployed(p: &Project) -> String {
    std::fs::read_to_string(p.work.join("npm-deploys.txt")).unwrap_or_default()
}

// ---------------------------------------------------------------- build labels

#[test]
fn only_a_well_formed_final_version_becomes_a_build_label() {
    use edition::commands::registry_build;
    assert_eq!(registry_build("1.4.0").as_deref(), Some("1.4.0"));
    for bad in ["abc", "01.2.3", "1.2.3.4", "1.2", "1.4.0-rc.1", "1.4.0-dev", "", "1.4.0 "] {
        assert_eq!(registry_build(bad), None, "{bad:?}");
    }
}

#[test]
fn a_registry_channel_needs_no_build_label() {
    let p = plain("reg-build");
    let dry = edition_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains("a registry channel's build is its version"), "{}", dry.stdout);
    assert!(dry.stdout.contains("log uploaded 0.6.0 --platform js --build 0.6.0"), "{}", dry.stdout);
    let ran = apply_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert!(ledger_text(&p.work).contains("build = \"0.6.0\""));
    assert!(p.read("log-calls.log").contains("--build 0.6.0"));
}

#[test]
fn build_and_build_file_take_precedence_over_the_version() {
    let p = registry(
        "reg-precedence",
        "",
        "[platforms.js-numbered]\nbuild_file = \"js/build.txt\"\nbuild_pattern = \"build={build}\"\n\n[channels.npm-numbered]\ntier = \"prod\"\nkind = \"registry\"\ntarget = \"https://npm.example.com\"\nplatform = \"js-numbered\"\nrun = [\"true\"]\n",
        &|w| write(w, "js/build.txt", "build=77\n"),
    );
    let explicit = edition_env(&p.work, &["deploy", "npm", "v0.6.0", "--build", "7"], &TOKEN);
    assert_eq!(explicit.code, 0, "{}", explicit.all());
    assert!(explicit.stdout.contains("--platform js --build 7 "), "{}", explicit.stdout);
    assert!(!explicit.stdout.contains("a registry channel's build is its version"));
    let from_file = edition_env(&p.work, &["deploy", "npm-numbered", "v0.6.0"], &TOKEN);
    assert_eq!(from_file.code, 0, "{}", from_file.all());
    assert!(from_file.stdout.contains("--platform js-numbered --build 77 "), "{}", from_file.stdout);
}

#[test]
fn other_kinds_and_prerelease_versions_still_need_a_build_label() {
    let p = registry(
        "reg-kinds",
        "",
        "[channels.home-js]\ntier = \"prod\"\nkind = \"home\"\ntarget = \"https://home.example.com\"\nplatform = \"js\"\nrun = [\"true\"]\n\n[channels.npm-next]\ntier = \"staging\"\nkind = \"registry\"\ntarget = \"https://npm.example.com\"\nplatform = \"js\"\nrun = [\"true\"]\n",
        &|_| {},
    );
    let home = edition_env(&p.work, &["deploy", "home-js", "v0.6.0"], &TOKEN);
    assert_eq!(home.code, 1, "{}", home.all());
    assert!(home.stderr.contains("pass --build <label>"), "{}", home.stderr);
    // Staging takes main, which builds as 0.7.0-dev: not a final version.
    let next = edition_env(&p.work, &["deploy", "npm-next", "main"], &TOKEN);
    assert_eq!(next.code, 1, "{}", next.all());
    assert!(next.stderr.contains("pass --build <label>"), "{}", next.stderr);
}

// ---------------------------------------------------------------- probes

#[test]
fn a_legacy_probe_gets_no_credentials() {
    let p = plain("probe-legacy");
    let ran = apply_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert_eq!(p.read("probe.txt").trim(), "token=", "a probe without probe_env sees nothing");
}

#[test]
fn probe_env_channel_shares_the_channel_env_visibly_and_never_records_it() {
    let p = registry("probe-channel", "probe_env = \"channel\"", "", &|_| {});
    let dry = edition_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains("[after] probe (with the channel's env)"), "{}", dry.stdout);
    let ran = apply_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(ran.code, 0, "{}", ran.all());
    for text in [dry.all(), ran.all(), ledger_text(&p.work)] {
        assert!(!text.contains("s3cret-token"), "{text}");
    }
    assert_eq!(p.read("probe.txt").trim(), "token=s3cret-token");
}

#[test]
fn a_probe_env_table_is_all_the_probe_gets() {
    let p = registry("probe-own", "probe_env = { OTHER = \"x\" }", "", &|_| {});
    let ran = apply_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert_eq!(p.read("probe.txt").trim(), "token=");
}

#[test]
fn probe_env_accepts_only_a_table_or_channel() {
    let p = registry("probe-bad", "probe_env = \"everything\"", "", &|_| {});
    let out = edition_env(&p.work, &["status"], &TOKEN);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(out.stderr.contains("probe_env is a table of variables or \"channel\""), "{}", out.stderr);
}

// ---------------------------------------------------------------- the check tree

#[test]
fn checks_see_the_deployed_tree_and_nothing_of_this_checkout() {
    let p = plain("tree-basic");
    // Untracked and ignored files in the checkout must not reach the checks.
    write(&p.work, "untracked.txt", "x");
    write(&p.work, "ignored-deploys.txt", "x");
    let dry = edition_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(dry.stdout.contains("the checks run on the tree of"), "{}", dry.stdout);
    assert!(dry.stdout.contains("not on this checkout"), "{}", dry.stdout);
    assert!(p.check_entries().is_empty(), "a dry run writes no check tree: {:?}", p.check_entries());
    let ran = apply_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let seen = p.read("checks.txt");
    assert!(seen.contains("version = \"0.6.0\""), "{seen}");
    assert!(seen.contains("/.edition/checks/"), "{seen}");
    assert!(!seen.contains("untracked.txt") && !seen.contains("ignored-deploys.txt"), "{seen}");
    assert!(!seen.contains("./.git/"), "no git metadata: {seen}");
    assert!(p.check_entries().is_empty(), "the check tree is removed: {:?}", p.check_entries());
    assert!(
        git(&p.work, &["worktree", "list", "--porcelain"]).lines().filter(|l| l.starts_with("worktree ")).count() == 1
    );

    // Even when HEAD has the deployed tree, the checks do not run in the checkout.
    git(&p.work, &["switch", "-q", "-c", "run-0.6.0", "v0.6.0"]);
    write(&p.work, "untracked-here.txt", "x");
    let here = apply_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(here.code, 0, "{}", here.all());
    let seen = p.read("checks.txt");
    let last = seen.rsplit("dir=").next().unwrap_or_default();
    assert!(last.contains("/.edition/checks/") && !last.contains("untracked-here.txt"), "{seen}");
}

#[test]
fn no_filter_hook_or_submodule_runs_on_a_dry_run_or_an_apply() {
    let p = registry("tree-hostile", "", "", &|w| {
        write(w, ".gitattributes", "*.txt filter=evil\n");
        write(w, "data.txt", "raw content\n");
        write(w, ".gitmodules", "[submodule \"sub\"]\n\tpath = sub\n\turl = ./nowhere\n");
    });
    // A gitlink, as a submodule records it.
    let head = git(&p.work, &["rev-parse", "HEAD"]);
    git(&p.work, &["checkout", "-q", "release/0.6"]);
    git(&p.work, &["update-index", "--add", "--cacheinfo", &format!("160000,{head},sub")]);
    git(&p.work, &["commit", "-q", "-m", "Add a submodule"]);
    git(&p.work, &["push", "-q", "origin", "release/0.6"]);
    git(&p.work, &["tag", "-a", "-f", "v0.6.0", "HEAD", "-m", "0.6.0"]);
    git(&p.work, &["push", "-q", "-f", "origin", "v0.6.0"]);
    git(&p.work, &["checkout", "-q", "main"]);
    // Installed only now, so the setup above never ran them.
    let marker = p.out.join("filter-ran");
    let smudge = format!("sh -c 'touch {}; cat'", marker.display());
    git(&p.work, &["config", "filter.evil.smudge", &smudge]);
    git(&p.work, &["config", "filter.evil.required", "true"]);
    let hook_marker = p.out.join("hook-ran");
    for hook in ["post-checkout", "post-index-change", "reference-transaction"] {
        executable(&p.work, &format!(".git/hooks/{hook}"), &format!("#!/bin/sh\ntouch {}\n", hook_marker.display()));
    }
    let dry = edition_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(!marker.exists() && !hook_marker.exists(), "nothing runs on a dry run");
    assert!(p.check_entries().is_empty());
    let mut args = vec!["deploy", "npm", "v0.6.0", "--apply", "--confirm"];
    let r = dry.receipt();
    args.push(&r);
    let ran = edition_env(&p.work, &args, &TOKEN);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert!(!marker.exists(), "no filter runs while the check tree is written");
    assert!(!hook_marker.exists(), "no hook runs while the check tree is written");
    let seen = p.read("checks.txt");
    assert!(seen.contains("./data.txt"), "{seen}");
    assert!(!seen.contains("./sub/"), "a submodule is never checked out: {seen}");
}

#[test]
fn a_link_at_the_check_directory_refuses_the_run() {
    let p = plain("tree-link");
    let elsewhere = p.out.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("mkdir");
    let dry = edition_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(dry.code, 0, "{}", dry.all());
    let r = dry.receipt();
    std::os::unix::fs::symlink(&elsewhere, p.checks_dir()).expect("link");
    let ran = edition_env(&p.work, &["deploy", "npm", "v0.6.0", "--apply", "--confirm", &r], &TOKEN);
    assert_ne!(ran.code, 0, "{}", ran.all());
    assert!(ran.stderr.contains("the check tree was not written"), "{}", ran.stderr);
    assert!(deployed(&p).is_empty(), "nothing was deployed");
    assert_eq!(std::fs::read_dir(&elsewhere).expect("dir").count(), 0, "nothing written through the link");
    assert!(ledger_text(&p.work).contains("result = \"failed\""));
}

#[test]
fn a_foreign_directory_or_stale_worktree_at_the_path_is_left_and_refused() {
    for (i, how) in ["dir", "worktree"].iter().enumerate() {
        let p = plain(&format!("tree-foreign-{i}"));
        let dry = edition_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
        assert_eq!(dry.code, 0, "{}", dry.all());
        let r = dry.receipt();
        let path = p.checks_dir().join(&r);
        p.make_checks_dir();
        if *how == "dir" {
            write(&path, "theirs.txt", "keep me");
        } else {
            git(&p.work, &["worktree", "add", "-q", "--detach", path.to_str().expect("utf8"), "v0.6.0"]);
        }
        let ran = edition_env(&p.work, &["deploy", "npm", "v0.6.0", "--apply", "--confirm", &r], &TOKEN);
        assert_ne!(ran.code, 0, "{how}: {}", ran.all());
        assert!(ran.stderr.contains("it is not Edition's"), "{how}: {}", ran.stderr);
        assert!(deployed(&p).is_empty(), "{how}: nothing was deployed");
        assert!(p.read("checks.txt").is_empty(), "{how}: no check ran");
        assert!(path.exists(), "{how}: the foreign directory is left alone");
        if *how == "dir" {
            assert_eq!(std::fs::read_to_string(path.join("theirs.txt")).expect("kept"), "keep me");
        }
        assert!(!p.checks_dir().join(format!("{r}.lock")).exists());
    }
}

#[test]
fn a_check_that_changes_the_tree_stops_the_later_checks_and_the_deploy() {
    let p = registry(
        "tree-tamper",
        "",
        "[checks.tamper]\nrun = [\"scripts/tamper.sh\"]\n\n[channels.npm-tampered]\ntier = \"prod\"\nkind = \"registry\"\ntarget = \"https://npm.example.com\"\nplatform = \"js\"\nchecks = [\"tamper\", \"which\"]\nrun = [\"scripts/record.sh\", \"npm-deploys.txt\", \"{commit}\"]\n",
        &|w| executable(w, "scripts/tamper.sh", "#!/bin/sh\necho changed >> Cargo.toml\n"),
    );
    let ran = apply_env(&p.work, &["deploy", "npm-tampered", "v0.6.0"], &TOKEN);
    assert_ne!(ran.code, 0, "{}", ran.all());
    assert!(ran.stderr.contains("the check tree no longer matches"), "{}", ran.stderr);
    assert!(ran.stderr.contains("Cargo.toml was changed"), "{}", ran.stderr);
    assert!(p.read("checks.txt").is_empty(), "the later check did not run");
    assert!(deployed(&p).is_empty());
    assert!(p.check_entries().is_empty(), "cleaned up after a failure: {:?}", p.check_entries());
}

#[test]
fn a_failed_check_cleans_up_and_a_killed_run_is_swept_later() {
    let p = registry(
        "tree-cleanup",
        "",
        "[checks.fail]\nrun = [\"false\"]\n\n[channels.npm-failing]\ntier = \"prod\"\nkind = \"registry\"\ntarget = \"https://npm.example.com\"\nplatform = \"js\"\nchecks = [\"fail\"]\nrun = [\"true\"]\n",
        &|_| {},
    );
    let failed = apply_env(&p.work, &["deploy", "npm-failing", "v0.6.0"], &TOKEN);
    assert_ne!(failed.code, 0, "{}", failed.all());
    assert!(p.check_entries().is_empty(), "{:?}", p.check_entries());

    // What a killed run leaves: its directory and an unlocked Edition lock file. A
    // directory without a lock file is not Edition's and stays.
    p.make_checks_dir();
    let stale = "0123456789abcdef0123456789abcdef";
    write(&p.checks_dir().join(stale), "left.txt", "x");
    write(&p.checks_dir(), &format!("{stale}.lock"), "");
    let foreign = "fedcba9876543210fedcba9876543210";
    write(&p.checks_dir().join(foreign), "theirs.txt", "x");
    let ok = apply_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert_eq!(p.check_entries(), vec![foreign.to_string()]);
}

#[test]
fn concurrent_plans_each_get_their_own_check_tree() {
    let p = registry(
        "tree-concurrent",
        "",
        "[checks.slow]\nrun = [\"scripts/slow.sh\", \"{var.out}\"]\n\n[channels.dev-js]\ntier = \"dev\"\nkind = \"home\"\ntarget = \"local\"\nchecks = [\"slow\"]\nrun = [\"true\"]\n",
        &|w| {
            executable(
                w,
                "scripts/slow.sh",
                "#!/bin/sh\nd=$(pwd)\necho \"start $d\" >> \"$1/slow.txt\"\nsleep 1\n[ -f Cargo.toml ] || exit 9\necho \"end $d\" >> \"$1/slow.txt\"\n",
            )
        },
    );
    let runs: Vec<_> = (0..2)
        .map(|_| {
            let w = p.work.clone();
            std::thread::spawn(move || edition_env(&w, &["deploy", "dev-js", "v0.6.0", "--apply"], &TOKEN))
        })
        .collect();
    for r in runs {
        let out = r.join().expect("thread");
        assert_eq!(out.code, 0, "{}", out.all());
    }
    let log = p.read("slow.txt");
    let dirs: std::collections::BTreeSet<&str> = log.lines().filter_map(|l| l.strip_prefix("end ")).collect();
    assert_eq!(dirs.len(), 2, "two runs, two directories: {log}");
    assert!(p.check_entries().is_empty(), "{:?}", p.check_entries());
}

#[test]
fn a_check_program_must_be_in_the_deployed_tree() {
    let p = registry(
        "tree-program",
        "",
        "[checks.newer]\nrun = [\"scripts/only-on-main.sh\"]\n\n[channels.npm-newer]\ntier = \"prod\"\nkind = \"registry\"\ntarget = \"https://npm.example.com\"\nplatform = \"js\"\nchecks = [\"newer\"]\nrun = [\"true\"]\n",
        &|_| {},
    );
    executable(&p.work, "scripts/only-on-main.sh", "#!/bin/sh\nexit 0\n");
    commit_all(&p.work, "A script the tag does not have");
    git(&p.work, &["push", "-q", "origin", "main"]);
    let out = edition_env(&p.work, &["deploy", "npm-newer", "v0.6.0"], &TOKEN);
    assert_ne!(out.code, 0, "{}", out.all());
    assert!(out.all().contains("is not an executable file in the tree of"), "{}", out.all());
}
