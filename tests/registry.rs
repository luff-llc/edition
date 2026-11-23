//! Registry channels: the build label a library takes, what a probe may see, and the
//! tree a deploy's checks run on. Each protection has a test that fails without it.

mod common;
use common::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Project {
    root: PathBuf,
    work: PathBuf,
    /// Outside the repository: fake tools and what the scripts record.
    out: PathBuf,
}

impl Project {
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.out.join(name)).unwrap_or_default()
    }

    /// Where check trees go in tests (`XDG_CACHE_HOME` is `<root>/cache`).
    fn checks_dir(&self) -> PathBuf {
        self.root.join("cache/edition/checks")
    }

    /// Makes the check tree directories as Edition would (private).
    fn make_checks_dir(&self) {
        use std::os::unix::fs::PermissionsExt;
        for d in [self.root.join("cache/edition"), self.checks_dir()] {
            std::fs::create_dir_all(&d).expect("mkdir");
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        }
    }

    /// Check tree directories, sorted (the global lock file left out).
    fn check_entries(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.checks_dir())
            .map(|rd| {
                rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n != ".lock").collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Runs git with `input` on stdin.
    fn git_in(&self, args: &[&str], input: &str) -> String {
        use std::io::Write;
        let mut c = Command::new("git")
            .arg("-C")
            .arg(&self.work)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("git");
        c.stdin.take().expect("stdin").write_all(input.as_bytes()).expect("write");
        let out = c.wait_with_output().expect("git");
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Moves `release/0.6` and `v0.6.0` to a commit whose tree is v0.6.0's plus `extra`
    /// (`mktree` lines), built without git's own path checks, as a hostile push could.
    fn retag_with(&self, extra: &[String]) {
        let mut lines = git(&self.work, &["ls-tree", "v0.6.0^{tree}"]);
        for e in extra {
            lines.push('\n');
            lines.push_str(e);
        }
        lines.push('\n');
        let tree = self.git_in(&["mktree"], &lines);
        let parent = git(&self.work, &["rev-parse", "release/0.6"]);
        let c = git(&self.work, &["commit-tree", &tree, "-p", &parent, "-m", "Hostile tree"]);
        git(&self.work, &["update-ref", "refs/heads/release/0.6", &c]);
        git(&self.work, &["push", "-q", "origin", "release/0.6"]);
        git(&self.work, &["tag", "-a", "-f", "v0.6.0", &c, "-m", "0.6.0"]);
        git(&self.work, &["push", "-q", "-f", "origin", "v0.6.0"]);
    }

    fn blob(&self, text: &str) -> String {
        self.git_in(&["hash-object", "-w", "--stdin"], text)
    }

    fn subtree(&self, name: &str, oid: &str) -> String {
        self.git_in(&["mktree"], &format!("100644 blob {oid}\t{name}\n"))
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
    let (root, work) = project(name, &cfg, "0.7.0-dev");
    // Records where it ran, the version it saw, every file and link it saw, and what git
    // finds from there.
    executable(
        &work,
        "scripts/which.sh",
        "#!/bin/sh\n{ echo \"dir=$(pwd)\"; grep '^version' Cargo.toml; find . -type f -o -type l | sort; echo \"git=$(git rev-parse --show-toplevel 2>&1)\"; echo end; } >> \"$1/checks.txt\"\n",
    );
    executable(&work, "scripts/probe.sh", "#!/bin/sh\nprintf 'token=%s\\n' \"$TOKEN\" >> \"$1/probe.txt\"\n");
    setup(&work);
    commit_all(&work, "Check and probe scripts");
    git(&work, &["push", "-q", "origin", "main"]);
    let line = line_with_version(&work, "0.6", "0.6.0");
    git(&work, &["tag", "-a", "v0.6.0", &line, "-m", "0.6.0"]);
    git(&work, &["push", "-q", "origin", "v0.6.0"]);
    Project { root, work, out }
}

fn plain(name: &str) -> Project {
    registry(name, "", "", &|_| {})
}

const TOKEN: [(&str, &str); 1] = [("EDITION_VAR_TOKEN", "s3cret-token")];

fn deployed(p: &Project) -> String {
    std::fs::read_to_string(p.work.join("npm-deploys.txt")).unwrap_or_default()
}

/// Dry run, then apply with its receipt; returns (receipt, apply output).
fn dry_then_apply(p: &Project, args: &[&str], between: &dyn Fn(&str)) -> (String, Out) {
    let dry = edition_env(&p.work, args, &TOKEN);
    assert_eq!(dry.code, 0, "{}", dry.all());
    let r = dry.receipt();
    between(&r);
    let mut full = args.to_vec();
    full.extend(["--apply", "--confirm", &r]);
    (r.clone(), edition_env(&p.work, &full, &TOKEN))
}

/// True when this file system treats names differing only in case as the same.
fn case_insensitive(dir: &Path) -> bool {
    let probe = dir.join("CaseProbe");
    std::fs::write(&probe, "x").expect("probe");
    let same = dir.join("caseprobe").exists();
    let _ = std::fs::remove_file(&probe);
    same
}

/// True when this file system treats NFC and NFD spellings as the same name.
fn normalisation_insensitive(dir: &Path) -> bool {
    let probe = dir.join("\u{e9}probe");
    std::fs::write(&probe, "x").expect("probe");
    let same = dir.join("e\u{301}probe").exists();
    let _ = std::fs::remove_file(&probe);
    same
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
    assert!(dry.stdout.contains("each check runs in a fresh copy of the tree of"), "{}", dry.stdout);
    assert!(dry.stdout.contains("not on this checkout"), "{}", dry.stdout);
    assert!(p.check_entries().is_empty(), "a dry run writes no check tree: {:?}", p.check_entries());
    let ran = apply_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let seen = p.read("checks.txt");
    assert!(seen.contains("version = \"0.6.0\""), "{seen}");
    assert!(seen.contains(&format!("dir={}", p.checks_dir().display())), "{seen}");
    assert!(!seen.contains("untracked.txt") && !seen.contains("ignored-deploys.txt"), "{seen}");
    assert!(!seen.contains("./.git/"), "no git metadata: {seen}");
    assert!(p.check_entries().is_empty(), "the check tree is removed: {:?}", p.check_entries());

    // Even when HEAD has the deployed tree, the checks do not run in the checkout.
    git(&p.work, &["switch", "-q", "-c", "run-0.6.0", "v0.6.0"]);
    write(&p.work, "untracked-here.txt", "x");
    let here = apply_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(here.code, 0, "{}", here.all());
    let seen = p.read("checks.txt");
    let last = seen.rsplit("dir=").next().unwrap_or_default();
    assert!(last.starts_with(&p.checks_dir().display().to_string()), "{seen}");
    assert!(!last.contains("untracked-here.txt"), "{seen}");
}

#[test]
fn git_in_a_check_finds_no_repository_even_from_inside_one() {
    let p = plain("tree-ceiling");
    // Put the check trees inside the checkout, where discovery would climb to its .git.
    let inside = p.work.join("inside-cache");
    let cache = inside.to_str().expect("utf8");
    let env = [("EDITION_VAR_TOKEN", "t"), ("XDG_CACHE_HOME", cache)];
    let ran = apply_env(&p.work, &["deploy", "npm", "v0.6.0"], &env);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let seen = p.read("checks.txt");
    assert!(seen.contains(&format!("dir={}", inside.display())), "{seen}");
    let git_line = seen.lines().find(|l| l.starts_with("git=")).unwrap_or_default();
    assert!(git_line.contains("not a git repository"), "git found a repository: {git_line}");
    assert!(!git_line.contains(&p.work.display().to_string()), "{git_line}");
}

#[test]
fn a_check_may_not_declare_git_discovery_variables() {
    let p = registry("tree-gitdir", "", "[checks.sneaky]\nrun = [\"true\"]\nenv = { GIT_DIR = \"/tmp\" }\n", &|_| {});
    let out = edition_env(&p.work, &["status"], &TOKEN);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(out.stderr.contains("env may not set GIT_DIR"), "{}", out.stderr);
}

#[test]
fn a_tree_with_a_git_path_is_refused_in_any_spelling() {
    for (i, name) in [".git", ".GIT", ".git.", "git~1", ".g\u{200c}it"].iter().enumerate() {
        let p = plain(&format!("tree-dotgit-{i}"));
        let blob = p.blob("[core]\n\tworktree = /\n");
        let sub = p.subtree("config", &blob);
        p.retag_with(&[format!("040000 tree {sub}\t{name}")]);
        let out = edition_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
        assert_eq!(out.code, 1, "{name:?}: {}", out.all());
        assert!(out.stderr.contains("git treats as .git"), "{name:?}: {}", out.stderr);
        assert!(p.check_entries().is_empty());
    }
}

#[test]
fn a_link_out_of_the_tree_is_refused() {
    for (i, target) in ["/etc/passwd", "../../outside", "scripts/../../x"].iter().enumerate() {
        let p = plain(&format!("tree-escape-{i}"));
        let blob = p.blob(target);
        p.retag_with(&[format!("120000 blob {blob}\tescape")]);
        let (_, ran) = dry_then_apply(&p, &["deploy", "npm", "v0.6.0"], &|_| {});
        assert_ne!(ran.code, 0, "{target}: {}", ran.all());
        assert!(ran.stderr.contains("outside the tree or upward"), "{target}: {}", ran.stderr);
        assert!(p.read("checks.txt").is_empty() && deployed(&p).is_empty(), "{target}");
        assert!(p.check_entries().is_empty(), "{target}: {:?}", p.check_entries());
    }
}

#[test]
fn paths_that_collide_on_this_file_system_are_refused() {
    for (i, (a, b)) in [("A.txt", "a.txt"), ("\u{e9}.txt", "e\u{301}.txt")].iter().enumerate() {
        let p = plain(&format!("tree-collide-{i}"));
        let collides = if i == 0 { case_insensitive(&p.out) } else { normalisation_insensitive(&p.out) };
        let (x, y) = (p.blob("one"), p.blob("two"));
        p.retag_with(&[format!("100644 blob {x}\t{a}"), format!("100644 blob {y}\t{b}")]);
        let (_, ran) = dry_then_apply(&p, &["deploy", "npm", "v0.6.0"], &|_| {});
        if collides {
            assert_ne!(ran.code, 0, "{a} {b}: {}", ran.all());
            assert!(ran.stderr.contains("two paths that are the same"), "{}", ran.stderr);
            assert!(deployed(&p).is_empty());
        } else {
            assert_eq!(ran.code, 0, "{a} {b}: {}", ran.all());
        }
        assert!(p.check_entries().is_empty(), "{:?}", p.check_entries());
    }
}

#[test]
fn trees_over_the_limits_are_refused_before_anything_is_written() {
    for (i, (limit, hint)) in [
        ("max_file_bytes = 40", "check_tree.max_file_bytes"),
        ("max_total_bytes = 200", "check_tree.max_total_bytes"),
        ("max_entries = 3", "check_tree.max_entries"),
        ("max_listing_bytes = 300", "check_tree.max_listing_bytes"),
    ]
    .iter()
    .enumerate()
    {
        let p = registry(&format!("tree-limits-{i}"), "", &format!("[check_tree]\n{limit}\n"), &|_| {});
        let out = edition_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
        assert_eq!(out.code, 1, "{limit}: {}", out.all());
        assert!(out.stderr.contains(hint), "{limit}: {}", out.stderr);
        assert!(p.check_entries().is_empty());
    }
    // A blob larger than the limit but within the defaults goes through.
    let p = plain("tree-limits-ok");
    let big = "x".repeat(3 << 20);
    let blob = p.blob(&big);
    p.retag_with(&[format!("100644 blob {blob}\tbig.bin")]);
    let ran = apply_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert!(p.read("checks.txt").contains("./big.bin"));
}

#[test]
fn no_filter_hook_or_submodule_runs_on_a_dry_run_or_an_apply() {
    let p = registry("tree-hostile", "", "", &|w| {
        write(w, ".gitattributes", "*.txt filter=evil\n");
        write(w, "data.txt", "raw content\n");
        write(w, ".gitmodules", "[submodule \"sub\"]\n\tpath = sub\n\turl = ./nowhere\n");
    });
    let head = git(&p.work, &["rev-parse", "HEAD"]);
    p.retag_with(&[format!("160000 commit {head}\tsub")]);
    // Installed only now, so the setup above never ran them.
    let marker = p.out.join("filter-ran");
    let smudge = format!("sh -c 'touch {}; cat'", marker.display());
    git(&p.work, &["config", "filter.evil.smudge", &smudge]);
    git(&p.work, &["config", "filter.evil.required", "true"]);
    let hook_marker = p.out.join("hook-ran");
    for hook in ["post-checkout", "post-index-change", "reference-transaction"] {
        executable(&p.work, &format!(".git/hooks/{hook}"), &format!("#!/bin/sh\ntouch {}\n", hook_marker.display()));
    }
    let (_, ran) = dry_then_apply(&p, &["deploy", "npm", "v0.6.0"], &|_| {
        assert!(!marker.exists() && !hook_marker.exists(), "nothing runs on a dry run");
        assert!(p.check_entries().is_empty());
    });
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
    let (_, ran) = dry_then_apply(&p, &["deploy", "npm", "v0.6.0"], &|_| {
        p.make_checks_dir();
        std::fs::remove_dir(p.checks_dir()).expect("rmdir");
        std::os::unix::fs::symlink(&elsewhere, p.checks_dir()).expect("link");
    });
    assert_ne!(ran.code, 0, "{}", ran.all());
    assert!(ran.stderr.contains("the check tree was not written"), "{}", ran.stderr);
    assert!(deployed(&p).is_empty(), "nothing was deployed");
    assert_eq!(std::fs::read_dir(&elsewhere).expect("dir").count(), 0, "nothing written through the link");
    assert!(ledger_text(&p.work).contains("result = \"failed\""));
}

#[test]
fn a_foreign_directory_or_stale_worktree_at_the_path_is_left_and_refused() {
    for (i, how) in ["dir", "worktree", "link"].iter().enumerate() {
        let p = plain(&format!("tree-foreign-{i}"));
        let target = p.out.join("target");
        let (r, ran) = dry_then_apply(&p, &["deploy", "npm", "v0.6.0"], &|r| {
            p.make_checks_dir();
            // The check is step 1 of the plan (after the note), so its tree is `<r>-1`.
            let path = p.checks_dir().join(format!("{r}-1"));
            match *how {
                "dir" => write(&path, "theirs.txt", "keep me"),
                "worktree" => {
                    git(&p.work, &["worktree", "add", "-q", "--detach", path.to_str().expect("utf8"), "v0.6.0"]);
                }
                _ => {
                    std::fs::create_dir_all(&target).expect("mkdir");
                    std::os::unix::fs::symlink(&target, &path).expect("link")
                }
            };
        });
        let path = p.checks_dir().join(format!("{r}-1"));
        assert_ne!(ran.code, 0, "{how}: {}", ran.all());
        assert!(ran.stderr.contains("it is not Edition's"), "{how}: {}", ran.stderr);
        assert!(deployed(&p).is_empty(), "{how}: nothing was deployed");
        assert!(p.read("checks.txt").is_empty(), "{how}: no check ran");
        assert!(std::fs::symlink_metadata(&path).is_ok(), "{how}: the foreign path is left alone");
        if *how == "dir" {
            assert_eq!(std::fs::read_to_string(path.join("theirs.txt")).expect("kept"), "keep me");
        }
        if *how == "link" {
            assert_eq!(std::fs::read_dir(&target).expect("dir").count(), 0);
        }
    }
}

#[test]
fn a_first_check_cannot_plant_anything_for_a_later_one() {
    let p = registry(
        "tree-plant",
        "",
        "[checks.plant]\nrun = [\"scripts/plant.sh\"]\n\n[channels.npm-planted]\ntier = \"prod\"\nkind = \"registry\"\ntarget = \"https://npm.example.com\"\nplatform = \"js\"\nchecks = [\"plant\", \"which\"]\nrun = [\"scripts/record.sh\", \"npm-deploys.txt\", \"{commit}\"]\n",
        &|w| {
            executable(
                w,
                "scripts/plant.sh",
                "#!/bin/sh\necho 'version = \"6.6.6\"' > Cargo.toml\necho x > planted.txt\nln -s /etc planted-link\nmkdir -p .git && echo 'gitdir: /' > .git/HEAD\n",
            )
        },
    );
    let ran = apply_env(&p.work, &["deploy", "npm-planted", "v0.6.0"], &TOKEN);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let seen = p.read("checks.txt");
    assert!(seen.contains("version = \"0.6.0\"") && !seen.contains("6.6.6"), "{seen}");
    assert!(!seen.contains("planted"), "{seen}");
    assert!(!seen.contains("./.git/"), "{seen}");
    assert!(p.check_entries().is_empty(), "{:?}", p.check_entries());
}

fn owner_marker(name: &str) -> String {
    format!("edition-check-tree v1 {name}\n")
}

#[test]
fn a_failed_check_cleans_up_and_only_marked_stale_trees_are_swept() {
    let p = registry(
        "tree-cleanup",
        "",
        "[checks.fail]\nrun = [\"false\"]\n\n[channels.npm-failing]\ntier = \"prod\"\nkind = \"registry\"\ntarget = \"https://npm.example.com\"\nplatform = \"js\"\nchecks = [\"fail\"]\nrun = [\"true\"]\n",
        &|_| {},
    );
    let failed = apply_env(&p.work, &["deploy", "npm-failing", "v0.6.0"], &TOKEN);
    assert_ne!(failed.code, 0, "{}", failed.all());
    assert!(p.check_entries().is_empty(), "{:?}", p.check_entries());

    p.make_checks_dir();
    let base = p.checks_dir();
    // Left by a killed run after its marker was written: Edition's, swept.
    let stale = "0123456789abcdef0123456789abcdef-1";
    write(&base.join(stale), "owner", &owner_marker(stale));
    write(&base.join(stale).join("tree"), "left.txt", "x");
    // Killed between taking the lock and mkdir, in the old layout: a directory that
    // already existed and a lock file beside it. Not Edition's: kept.
    let foreign = "fedcba9876543210fedcba9876543210-1";
    write(&base.join(foreign), "theirs.txt", "x");
    write(&base, "fedcba9876543210fedcba9876543210.lock", "");
    // A marker that names another directory: kept.
    let wrong = "11111111111111111111111111111111-2";
    write(&base.join(wrong), "owner", &owner_marker("22222222222222222222222222222222-2"));
    // A link where a marker should be: kept.
    let linked = "33333333333333333333333333333333-1";
    std::fs::create_dir_all(base.join(linked)).expect("mkdir");
    write(&p.out, "fake-owner", &owner_marker(linked));
    std::os::unix::fs::symlink(p.out.join("fake-owner"), base.join(linked).join("owner")).expect("link");

    let ok = apply_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let mut want = vec![
        foreign.to_string(),
        "fedcba9876543210fedcba9876543210.lock".to_string(),
        wrong.to_string(),
        linked.to_string(),
    ];
    want.sort();
    assert_eq!(p.check_entries(), want);
    assert_eq!(std::fs::read_to_string(base.join(foreign).join("theirs.txt")).expect("kept"), "x");
}

#[test]
fn concurrent_runs_and_sweeps_each_keep_their_own_tree() {
    let p = registry(
        "tree-concurrent",
        "",
        "[checks.slow]\nrun = [\"scripts/slow.sh\", \"{var.out}\"]\n\n[channels.dev-js]\ntier = \"dev\"\nkind = \"home\"\ntarget = \"local\"\nchecks = [\"slow\", \"slow\"]\nrun = [\"true\"]\n",
        &|w| {
            executable(
                w,
                "scripts/slow.sh",
                "#!/bin/sh\nd=$(pwd)\necho \"start $d\" >> \"$1/slow.txt\"\n[ -e mine ] && exit 8\necho x > mine\nsleep 1\n[ -f Cargo.toml ] && [ -f scripts/slow.sh ] || exit 9\necho \"end $d\" >> \"$1/slow.txt\"\n",
            )
        },
    );
    // Stale trees to sweep while the runs create theirs.
    p.make_checks_dir();
    for k in 0..3 {
        let n = format!("{}-1", format!("{k}").repeat(32));
        write(&p.checks_dir().join(&n), "owner", &owner_marker(&n));
    }
    let runs: Vec<_> = (0..4)
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
    assert_eq!(dirs.len(), 8, "four runs, two checks each, eight trees: {log}");
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

#[test]
fn a_path_longer_than_the_limit_stops_the_listing() {
    let p = plain("tree-long-path");
    let blob = p.blob("x");
    let name = "n".repeat(edition::checktree::MAX_PATH_BYTES + 1);
    p.retag_with(&[format!("100644 blob {blob}\t{name}")]);
    let out = edition_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stderr.contains("a path over 4096 bytes"), "{}", out.stderr);
}

#[test]
fn many_entries_stop_the_listing_as_they_arrive() {
    let p = registry("tree-many", "", "[check_tree]\nmax_entries = 50\n", &|_| {});
    let blob = p.blob("x");
    let extra: Vec<String> = (0..500).map(|i| format!("100644 blob {blob}\tf{i:04}")).collect();
    p.retag_with(&extra);
    let out = edition_env(&p.work, &["deploy", "npm", "v0.6.0"], &TOKEN);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stderr.contains("more than 50 entries"), "{}", out.stderr);
}

/// A ledger written by Edition on `main` (f1d981b) before this change: the streaming
/// hash must verify its chain, so the chain format did not change.
#[test]
fn a_ledger_written_before_streaming_hashes_still_verifies() {
    let text = include_str!("fixtures/ledger-format2-main-f1d981b.toml");
    assert_eq!(edition::ledger::format_of(text), Ok(Some(2)));
    let entries = edition::ledger::parse_ledger(text).expect("parses");
    assert_eq!(entries.len(), 3);
    let dir = tmp("ledger-fixture");
    let git = edition::git::Git::new(&dir);
    assert_eq!(edition::ledger::check_chain(&git, &entries), Ok(()));
    // Every link but the first is the SHA-256 of the entry before it.
    for w in entries.windows(2) {
        assert_eq!(edition::ledger::entry_hash(&git, &w[0]).expect("hash"), w[1].prev);
    }
    // A changed entry breaks the chain, so the check is not vacuous.
    let mut broken = entries.clone();
    broken[0].by = "someone-else".into();
    assert!(edition::ledger::check_chain(&git, &broken).is_err());
}
