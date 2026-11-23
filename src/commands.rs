//! The commands. Every mutating command builds a plan, prints it, and runs it only
//! with `--apply` (and `--confirm <token>` when it has outward steps).

use crate::config::{Approval, Loaded, Tier};
use crate::git::Git;
use crate::ledger::{self, Entry};
use crate::policy::{self, RemoteView};
use crate::steps::{self, Kind, Mode, Ran, Step};
use crate::util::{now_utc, short};
use crate::version::{parse_line, FilePattern, Version};

pub struct Ctx {
    pub l: Loaded,
    pub git: Git,
    pub mode: Mode,
    pub by: String,
    pub online: bool,
    pub build: Option<String>,
    pub approved_by: Option<String>,
    pub at: Option<String>,
    pub note: Option<String>,
}

pub type Res = Result<i32, String>;

impl Ctx {
    fn view(&self) -> Result<RemoteView, String> {
        RemoteView::build(&self.git, &self.l.cfg.remote, self.online)
    }

    fn remote_ref(&self, branch: &str) -> String {
        format!("refs/remotes/{}/{branch}", self.l.cfg.remote)
    }

    fn git_step(&self, kind: Kind, what: &str, args: &[&str]) -> Step {
        let mut argv = vec!["git".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        Step::new(kind, what, argv, &self.l.root)
    }

    fn all_tags(&self, view: &RemoteView) -> Vec<(String, String)> {
        let mut tags = view.local_tags.clone();
        if let Some(rt) = &view.remote_tags {
            for t in rt {
                if !tags.iter().any(|(n, _)| n == &t.0) {
                    tags.push(t.clone());
                }
            }
        }
        tags
    }

    /// Final tags on a line, newest first.
    fn finals_on_line(&self, view: &RemoteView, line: &str) -> Vec<(String, Version)> {
        let mut out: Vec<(String, Version)> = self
            .all_tags(view)
            .iter()
            .filter_map(|(n, _)| match policy::classify_tag(&self.l.final_tag, &self.l.pre_tag, n) {
                Some((v, None)) if v.line() == line => Some((n.clone(), v)),
                _ => None,
            })
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1));
        out
    }

    fn next_pre(&self, view: &RemoteView, version: &Version) -> u32 {
        self.all_tags(view)
            .iter()
            .filter_map(|(n, _)| match policy::classify_tag(&self.l.final_tag, &self.l.pre_tag, n) {
                Some((v, Some(k))) if v == *version => Some(k),
                _ => None,
            })
            .max()
            .map_or(1, |k| k + 1)
    }

    fn tag_exists(&self, view: &RemoteView, name: &str) -> bool {
        self.all_tags(view).iter().any(|(n, _)| n == name)
    }

    fn record(&self, e: &Entry) -> Result<(), String> {
        ledger::append(&self.l.ledger_path(), e)?;
        let kind = if e.action == "deploy" { "channel-updated" } else { "ref-published" };
        crate::util::emit_event(
            kind,
            &[
                ("id", &e.id),
                ("channel", e.channel.as_deref().unwrap_or("")),
                ("ref", &e.ref_),
                ("commit", &e.commit),
                ("result", &e.result),
                ("by", &e.by),
            ],
        );
        println!("recorded {} in {}", e.id, self.l.cfg.ledger);
        Ok(())
    }

    fn base_entry(&self, action: &str, r: &str, commit: &str) -> Entry {
        let at = now_utc();
        Entry {
            id: format!("{at}/{action}/{}", short(commit)),
            at,
            by: self.by.clone(),
            action: action.into(),
            product: self.l.cfg.product.clone(),
            ref_: r.into(),
            commit: commit.into(),
            tree: self.git.tree(commit),
            result: "ok".into(),
            note: self.note.clone(),
            ..Entry::default()
        }
    }

    /// Runs a plan whose outward steps are pushes or PRs, recording each that ran.
    fn run_and_record(&self, title: &str, steps: &[Step], token: &str, records: &[(usize, Entry)]) -> Res {
        steps::print(title, steps);
        match steps::execute(steps, &self.mode, token)? {
            Ran::DryRun => Ok(0),
            Ran::Ran(outcomes) => {
                let mut failed = false;
                let ran: Vec<&steps::Outcome> = outcomes.iter().collect();
                for (idx, entry) in records {
                    let step = &steps[*idx];
                    if let Some(o) = ran.iter().find(|o| o.argv == step.argv) {
                        let mut e = entry.clone();
                        e.result = if o.ok() { "ok".into() } else { "failed".into() };
                        e.command = step.argv.clone();
                        self.record(&e)?;
                    }
                }
                if outcomes.iter().any(|o| !o.ok()) {
                    failed = true;
                }
                Ok(if failed { 1 } else { 0 })
            }
        }
    }
}

fn refuse(problems: &[String]) -> Res {
    for p in problems {
        eprintln!("refused: {p}");
    }
    Ok(1)
}

// ---------------------------------------------------------------- status

pub fn status(c: &Ctx) -> Res {
    let l = &c.l;
    let view = c.view()?;
    println!("Edition: {} ({})", l.cfg.product, l.root.display());
    let main = &l.cfg.main;
    match view.branch(main) {
        Some(tip) => {
            let v = policy::agreed_version(l, &c.git, Some(tip))
                .map(|v| v.unwrap_or_else(|| "-".into()))
                .unwrap_or_else(|e| e);
            println!("main   {}/{main} {} builds as {v}", l.cfg.remote, short(tip));
        }
        None => println!("main   {}/{main} unknown (fetch first)", l.cfg.remote),
    }
    let mut any = false;
    for (name, tip) in &view.branches {
        let Some(caps) = l.line_branch.matches(name) else { continue };
        let Some(line) = crate::version::Template::get(&caps, "line") else { continue };
        if parse_line(line).is_err() {
            continue;
        }
        any = true;
        let last =
            c.finals_on_line(&view, line).first().map(|(n, _)| n.clone()).unwrap_or_else(|| "no final tag".into());
        let unmerged = view.branch(main).map(|m| c.git.log_range(&format!("{m}..{tip}")).len()).unwrap_or(0);
        println!(
            "line   {}/{name} {} latest {last}; {}",
            l.cfg.remote,
            short(tip),
            if unmerged == 0 {
                "merged forward".to_string()
            } else {
                format!("{unmerged} commit(s) not merged forward")
            }
        );
    }
    if !any {
        println!("line   none");
    }
    let entries = ledger::load(&l.ledger_path())?;
    let current = ledger::current_by_channel(&entries);
    println!("\nchannels:");
    for (name, ch) in &l.cfg.channels {
        let held = current
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, e)| format!("{} ({}) {}", e.ref_, short(&e.commit), e.at))
            .unwrap_or_else(
                || if ch.tier == Tier::Dev { "not recorded (dev)".into() } else { "nothing recorded".into() },
            );
        println!("  {name:<22} {:<8} {:<10} {:<40} {held}", ch.tier.name(), ch.kind, ch.target);
    }
    if !l.cfg.platforms.is_empty() {
        let names: Vec<&String> = l.cfg.platforms.keys().collect();
        println!("\nplatforms (version, build) per tier, from the ledger:");
        println!("  {:<10} {}", "", names.iter().map(|n| format!("{n:<24}")).collect::<String>());
        for tier in [Tier::Staging, Tier::Prod] {
            let cells: String = names
                .iter()
                .map(|p| {
                    let latest = entries.iter().rev().find(|e| {
                        e.action == "deploy"
                            && e.result == "ok"
                            && e.tier.as_deref() == Some(tier.name())
                            && e.platform.as_deref() == Some(p.as_str())
                    });
                    let cell = latest
                        .map(|e| {
                            format!(
                                "{} ({})",
                                e.version.clone().unwrap_or_else(|| "-".into()),
                                e.build.clone().unwrap_or_else(|| "-".into())
                            )
                        })
                        .unwrap_or_else(|| "-".into());
                    format!("{cell:<24}")
                })
                .collect();
            println!("  {:<10} {cells}", tier.name());
        }
    }
    if !l.todos.is_empty() {
        println!("\n{} open to-do marker(s) in {}:", l.todos.len(), crate::config::FILE_NAME);
        for t in &l.todos {
            println!("  {t}");
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------- plan

pub fn plan(c: &Ctx, version: &str) -> Res {
    let l = &c.l;
    let v = Version::parse(version)?;
    if !v.is_final() {
        return Err("plan takes a final version such as 0.6.0".into());
    }
    let view = c.view()?;
    let line = v.line();
    let branch = l.line_branch_name(&line);
    println!("Release {} {v}", l.cfg.product);
    let tip = view.branch(&branch).map(str::to_string);
    match &tip {
        Some(t) => {
            let fv = policy::agreed_version(l, &c.git, Some(t)).map(|x| x.unwrap_or_default()).unwrap_or_else(|e| e);
            println!("  line {branch}: cut, tip {} builds as {fv}", short(t));
        }
        None => println!("  line {branch}: not cut (edition cut {line})"),
    }
    let final_name = l.final_tag_name(&v.to_string());
    println!("  final tag {final_name}: {}", if c.tag_exists(&view, &final_name) { "exists" } else { "not yet" });
    let next = c.next_pre(&view, &v);
    println!(
        "  pre-release tags: {}",
        if next == 1 {
            "none".to_string()
        } else {
            format!("{} so far, next {}", next - 1, l.pre_tag_name(&v.to_string(), next))
        }
    );
    if let Some(m) = view.branch(&l.cfg.main) {
        let since = c.finals_on_line(&view, &format!("{}.{}", v.major, v.minor.saturating_sub(1)));
        if let Some((prev, _)) = since.first() {
            let n = c.git.log_range(&format!("{prev}..{m}")).len();
            println!("  {}/{} is {n} commit(s) ahead of {prev}", l.cfg.remote, l.cfg.main);
        }
    }
    match &l.cfg.concordance {
        None => {
            println!("  carries: Edition keeps no feature list; no [concordance] catalog is configured for this repo")
        }
        Some(_) => match crate::concordance::releases(l) {
            Ok(rs) => {
                let rid = crate::concordance::release_id(l, &v.to_string()).unwrap_or_default();
                match rs.iter().find(|r| r.id == rid) {
                    Some(r) => {
                        let ready = r.carries.iter().filter(|x| x.ready).count();
                        println!(
                            "  Concordance {rid}: {} \"{}\", {ready}/{} carried items ready, builds {:?}",
                            r.state,
                            r.title.clone().unwrap_or_default(),
                            r.carries.len(),
                            r.builds
                        );
                        for x in r.carries.iter().filter(|x| !x.ready) {
                            println!("    not ready: {}", x.id);
                        }
                    }
                    None => {
                        println!("  Concordance: no release {rid} in the tracker (add it to tracker/releases.yaml)")
                    }
                }
            }
            Err(e) => println!("  Concordance: {e}"),
        },
    }
    println!("\n  channels:");
    for (name, ch) in &l.cfg.channels {
        let takes = match ch.tier {
            Tier::Dev => "any ref (local only)".to_string(),
            Tier::Staging => format!("the tip of {}/{}", l.cfg.remote, l.cfg.main),
            Tier::Prod if ch.allow_prerelease => format!("{final_name} or a pre-release tag of it"),
            Tier::Prod => final_name.clone(),
        };
        println!("    {name:<22} {:<8} takes {takes}", ch.tier.name());
    }
    println!("\n  steps:");
    if tip.is_none() {
        println!("    edition cut {line}");
    }
    println!("    edition rc {v}        # optional, for store review builds");
    println!("    edition tag {v}");
    println!("    edition deploy <prod channel> {final_name}");
    Ok(0)
}

// ---------------------------------------------------------------- bump

pub fn bump(c: &Ctx, version: &str) -> Res {
    let l = &c.l;
    let v = Version::parse(version)?;
    if l.cfg.version_files.is_empty() {
        return Err("no version_files configured".into());
    }
    let mut changes: Vec<(std::path::PathBuf, String, usize)> = Vec::new();
    for vf in &l.cfg.version_files {
        let path = l.root.join(&vf.path);
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let pat = FilePattern::parse(&vf.pattern, &["version"])?;
        let base = changes.iter().rev().find(|(p, _, _)| *p == path).map(|(_, t, _)| t.clone()).unwrap_or(text);
        let (out, n) = pat.replace_all(&base, vf.after.as_deref(), &v.to_string());
        if n == 0 {
            return Err(format!("{}: pattern `{}` not found", vf.path, vf.pattern));
        }
        let before = changes.iter().find(|(p, _, _)| *p == path).map_or(0, |(_, _, k)| *k);
        changes.retain(|(p, _, _)| *p != path);
        changes.push((path, out, before + n));
    }
    println!("Set version {v} in:");
    for (p, _, n) in &changes {
        println!("  [local] {} ({n} occurrence(s))", p.strip_prefix(&l.root).unwrap_or(p).display());
    }
    if !c.mode.apply {
        println!("\nDry run: nothing was written. To write it: --apply");
        return Ok(0);
    }
    for (p, out, _) in &changes {
        std::fs::write(p, out).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    println!("written; review with git diff, then commit (lock files may need regenerating by their tool)");
    Ok(0)
}

// ---------------------------------------------------------------- cut

pub fn cut(c: &Ctx, line: &str) -> Res {
    let l = &c.l;
    let (ma, mi) = parse_line(line)?;
    let view = c.view()?;
    let branch = l.line_branch_name(line);
    let mut problems = Vec::new();
    if view.branch(&branch).is_some() {
        problems.push(format!("{}/{branch} already exists", l.cfg.remote));
    }
    if c.git.local_branch_exists(&branch) {
        problems.push(format!("a local branch {branch} already exists"));
    }
    let Some(main_tip) = view.branch(&l.cfg.main).map(str::to_string) else {
        return refuse(&[format!("{}/{} is unknown (fetch first)", l.cfg.remote, l.cfg.main)]);
    };
    let from = c.at.clone().unwrap_or_else(|| c.remote_ref(&l.cfg.main));
    let Some(from_commit) = c.git.resolve(&from) else {
        return refuse(&[format!("{from} is not a commit")]);
    };
    if !c.git.is_ancestor(&from_commit, &main_tip) {
        problems.push(format!("{from} is not on {}/{}: a line is cut from main", l.cfg.remote, l.cfg.main));
    }
    let want_dev = format!("{line}.0-{}", l.cfg.lines.dev_suffix);
    let mut notes = Vec::new();
    match policy::agreed_version(l, &c.git, Some(&from_commit)) {
        Ok(Some(v)) if v == want_dev => {}
        Ok(Some(v)) => notes.push(format!("main builds as {v}, not {want_dev}; check you are cutting the right line")),
        Ok(None) => {}
        Err(e) => problems.push(e),
    }
    if !problems.is_empty() {
        return refuse(&problems);
    }
    for n in &notes {
        println!("warning: {n}");
    }
    let next_dev = format!("{ma}.{}.0-{}", mi + 1, l.cfg.lines.dev_suffix);
    let steps = vec![
        c.git_step(
            Kind::Local,
            &format!("create {branch} at {}", short(&from_commit)),
            &["branch", "--no-track", &branch, &from_commit],
        ),
        Step::note(&format!(
            "on {branch}: edition bump {line}.0 --apply, commit \"Start the {line} line\", and land it on {branch}"
        )),
        Step::note(&format!("on {}: edition bump {next_dev} --apply and land it through the usual review", l.cfg.main)),
        c.git_step(
            Kind::Outward,
            &format!("publish {branch}"),
            &["push", &l.cfg.remote, &format!("refs/heads/{branch}:refs/heads/{branch}")],
        ),
    ];
    let mut e = c.base_entry("push-branch", &branch, &from_commit);
    e.note = Some(c.note.clone().unwrap_or_else(|| format!("cut the {line} line")));
    c.run_and_record(&format!("Cut the {line} line from {}", short(&from_commit)), &steps, "push", &[(3, e)])
}

// ---------------------------------------------------------------- rc / tag

fn tag_plan(c: &Ctx, version: &str, pre: bool) -> Res {
    let l = &c.l;
    let v = Version::parse(version)?;
    if !v.is_final() {
        return Err(format!("give the release version (for example {}), not a pre-release", v.base()));
    }
    let view = c.view()?;
    let line = v.line();
    let branch = l.line_branch_name(&line);
    let mut problems = Vec::new();
    let Some(line_tip) = view.branch(&branch).map(str::to_string) else {
        return refuse(&[format!("{}/{branch} does not exist (edition cut {line})", l.cfg.remote)]);
    };
    let at = c.at.clone().unwrap_or_else(|| line_tip.clone());
    let Some(commit) = c.git.resolve(&at) else { return refuse(&[format!("{at} is not a commit")]) };
    if !c.git.is_ancestor(&commit, &line_tip) {
        problems.push(format!(
            "{} is not on {}/{branch}: tags are made on the release line",
            short(&commit),
            l.cfg.remote
        ));
    }
    let (name, want) = if pre {
        let n = c.next_pre(&view, &v);
        let name = l.pre_tag_name(&v.to_string(), n);
        let want = if l.cfg.tags.prerelease_files == "full" { format!("{v}-rc.{n}") } else { v.to_string() };
        (name, want)
    } else {
        (l.final_tag_name(&v.to_string()), v.to_string())
    };
    if c.tag_exists(&view, &name) {
        problems.push(format!("tag {name} already exists"));
    }
    if !pre {
        if let Some((newest, nv)) = c.finals_on_line(&view, &line).first() {
            if *nv >= v {
                problems.push(format!("{newest} already exists on this line; {v} must be newer"));
            }
        }
    }
    match policy::agreed_version(l, &c.git, Some(&commit)) {
        Ok(Some(fv)) if fv == want => {}
        Ok(Some(fv)) => {
            problems.push(format!("the version files at {} say {fv}, not {want} (edition bump {want})", short(&commit)))
        }
        Ok(None) => {}
        Err(e) => problems.push(e),
    }
    if !problems.is_empty() {
        return refuse(&problems);
    }
    let msg = format!("{} {}", l.cfg.product, name);
    let steps = vec![
        c.git_step(
            Kind::Local,
            &format!("tag {} as {name}", short(&commit)),
            &["tag", "-a", &name, &commit, "-m", &msg],
        ),
        c.git_step(Kind::Outward, &format!("publish {name}"), &["push", &l.cfg.remote, &format!("refs/tags/{name}")]),
    ];
    let e = c.base_entry("push-tag", &name, &commit);
    let title = format!("{} {name} at {}", if pre { "Pre-release" } else { "Release" }, short(&commit));
    c.run_and_record(&title, &steps, "push", &[(1, e)])
}

pub fn rc(c: &Ctx, version: &str) -> Res {
    tag_plan(c, version, true)
}

pub fn tag(c: &Ctx, version: &str) -> Res {
    tag_plan(c, version, false)
}

// ---------------------------------------------------------------- hotfix

pub fn hotfix_start(c: &Ctx, line: &str) -> Res {
    let l = &c.l;
    parse_line(line)?;
    let view = c.view()?;
    let branch = l.line_branch_name(line);
    let Some(tip) = view.branch(&branch).map(str::to_string) else {
        return refuse(&[format!("{}/{branch} does not exist", l.cfg.remote)]);
    };
    let next = match c.finals_on_line(&view, line).first() {
        Some((_, v)) => Version { patch: v.patch + 1, ..v.clone() },
        None => return refuse(&[format!("no final tag on {line} yet; tag {line}.0 before hotfixing it")]),
    };
    let work = l.hotfix_branch.render(&[("version", &next.to_string())])?;
    if c.git.local_branch_exists(&work) {
        return refuse(&[format!("a local branch {work} already exists")]);
    }
    let steps = vec![
        c.git_step(
            Kind::Local,
            &format!("start {work} from {}/{branch}", l.cfg.remote),
            &["branch", "--no-track", &work, &tip],
        ),
        Step::note(&format!(
            "fix on {work}; edition bump {next} --apply; open a pull request into {branch} (never into {})",
            l.cfg.main
        )),
        Step::note(&format!("once it is merged into {branch}: edition hotfix finish {next}")),
    ];
    steps::print(&format!("Hotfix {next} on the {line} line"), &steps);
    match steps::execute(&steps, &c.mode, "none")? {
        Ran::DryRun => Ok(0),
        Ran::Ran(o) => Ok(if o.iter().all(|x| x.ok()) { 0 } else { 1 }),
    }
}

pub fn hotfix_finish(c: &Ctx, version: &str) -> Res {
    let v = Version::parse(version)?;
    if v.patch == 0 {
        return Err(format!("{v} is not a hotfix version (its patch number is 0)"));
    }
    // A hotfix is finished by tagging the release line, then merging the line forward.
    let code = tag(c, version)?;
    if code != 0 {
        return Ok(code);
    }
    println!();
    forward(c)
}

// ---------------------------------------------------------------- forward

pub fn forward(c: &Ctx) -> Res {
    let l = &c.l;
    let view = c.view()?;
    let Some(main_tip) = view.branch(&l.cfg.main).map(str::to_string) else {
        return refuse(&[format!("{}/{} is unknown", l.cfg.remote, l.cfg.main)]);
    };
    let mut steps = Vec::new();
    let mut records = Vec::new();
    for (name, tip) in &view.branches {
        let Some(caps) = l.line_branch.matches(name) else { continue };
        if crate::version::Template::get(&caps, "line").is_none_or(|x| parse_line(x).is_err()) {
            continue;
        }
        let missing = c.git.log_range(&format!("{main_tip}..{tip}"));
        if missing.is_empty() {
            println!("{}/{name}: merged forward", l.cfg.remote);
            continue;
        }
        println!("{}/{name}: {} commit(s) not in {}:", l.cfg.remote, missing.len(), l.cfg.main);
        for m in &missing {
            println!("    {m}");
        }
        let title = format!("Merge {name} forward into {}", l.cfg.main);
        let body = format!(
            "Brings {} commit(s) shipped on {name} into {}. Release lines merge forward only; never merge {} into {name}.\n\nOpened by edition forward.",
            missing.len(),
            l.cfg.main,
            l.cfg.main
        );
        steps.push(Step::new(
            Kind::Outward,
            &format!("open a pull request merging {name} into {}", l.cfg.main),
            vec![
                "gh".into(),
                "pr".into(),
                "create".into(),
                "--base".into(),
                l.cfg.main.clone(),
                "--head".into(),
                name.clone(),
                "--title".into(),
                title,
                "--body".into(),
                body,
            ],
            &l.root,
        ));
        let mut e = c.base_entry("open-pr", name, tip);
        e.note = Some(format!("forward merge of {name} into {}", l.cfg.main));
        records.push((steps.len() - 1, e));
    }
    if steps.is_empty() {
        println!("Every release line is merged forward.");
        return Ok(0);
    }
    println!();
    c.run_and_record("Forward merges", &steps, "pr", &records)
}

// ---------------------------------------------------------------- deploy / promote

fn expand_argv(c: &Ctx, argv: &[String], vars: &[(&str, &str)]) -> Vec<String> {
    argv.iter()
        .map(|a| {
            let mut s = c.l.expand(a);
            for (k, v) in vars {
                s = s.replace(&format!("{{{k}}}"), v);
            }
            s
        })
        .collect()
}

/// A platform's build number at a commit, from its configured build file.
fn platform_build(l: &Loaded, git: &Git, platform: &str, commit: &str) -> Option<String> {
    let p = l.cfg.platforms.get(platform)?;
    let text = git.show(commit, p.build_file.as_deref()?)?;
    let pat = FilePattern::parse(p.build_pattern.as_deref()?, &["build"]).ok()?;
    pat.find_all(&text, None).into_iter().next().map(|(_, b)| b)
}

/// The parity gate: Concordance must report every item the release carries as ready on
/// the platforms the release builds. Accepted differences are recorded in Concordance
/// (`not-applicable`, or the item moved out of `carries`), never in Edition.
fn parity_gate(l: &Loaded, version: &str) -> Result<String, String> {
    let base = Version::parse(version).map(|v| v.base().to_string()).map_err(|e| format!("no release version: {e}"))?;
    let rid = crate::concordance::release_id(l, &base).ok_or("no [concordance] release template")?;
    let releases = crate::concordance::releases(l)?;
    let rel = releases.iter().find(|r| r.id == rid).ok_or_else(|| format!("Concordance has no release {rid}"))?;
    let gaps: Vec<&str> = rel.carries.iter().filter(|x| !x.ready).map(|x| x.id.as_str()).collect();
    if gaps.is_empty() {
        Ok(format!(
            "Concordance {rid}: {}/{} carried items ready on every platform",
            rel.carries.len(),
            rel.carries.len()
        ))
    } else {
        for g in &gaps {
            crate::util::emit_event("parity-gap", &[("release", &rid), ("item", g)]);
        }
        Err(format!(
            "Concordance {rid}: {} of {} carried items not ready: {}",
            gaps.len(),
            rel.carries.len(),
            gaps.join(", ")
        ))
    }
}

pub fn deploy(c: &Ctx, channel: &str, r: &str) -> Res {
    let l = &c.l;
    let ch = l.cfg.channels.get(channel).ok_or_else(|| {
        format!(
            "no channel `{channel}` (configured: {})",
            l.cfg.channels.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    })?;
    // Prod deploys read the remote's tags; an outward run never trusts a stale view.
    let online = c.online || (c.mode.apply && ch.tier == Tier::Prod);
    let view = RemoteView::build(&c.git, &l.cfg.remote, online)?;
    let commit = match ch.tier {
        Tier::Staging if r == l.cfg.main => view.branch(&l.cfg.main).map(str::to_string),
        _ => c.git.resolve(r),
    };
    let Some(commit) = commit else { return refuse(&[format!("{r} is not a commit here")]) };
    let mut verdict = policy::check_tier(l, &c.git, &view, ch.tier, ch.allow_prerelease, r, &commit);
    let mut problems = verdict.violations.clone();
    let configured_text = format!("{} {} {}", ch.run.join(" "), ch.probe.join(" "), ch.target);
    if configured_text.contains(crate::config::TODO_MARK) {
        problems
            .push(format!("channel {channel} is not fully configured (a to-do marker is in its run, probe or target)"));
    }
    if ch.approval != Approval::None && c.approved_by.is_none() {
        problems.push(format!(
            "channel {channel} needs an approval: pass --approved-by <name>{}",
            if ch.approval == Approval::Touch { " (the command will also ask for a hardware key touch)" } else { "" }
        ));
    }
    let build = c.build.clone().or_else(|| ch.platform.as_deref().and_then(|p| platform_build(l, &c.git, p, &commit)));
    if ch.platform.is_some() && l.cfg.concordance.is_some() && ch.tier != Tier::Dev && build.is_none() {
        problems.push(format!(
            "channel {channel} ships the {} platform: pass --build <label> (or configure its build_file)",
            ch.platform.clone().unwrap_or_default()
        ));
    }
    // Readiness gates: channels that must already hold this commit, per the ledger.
    if !ch.requires.is_empty() {
        let entries = ledger::load(&l.ledger_path())?;
        for req in &ch.requires {
            let held = entries.iter().any(|e| {
                e.action == "deploy"
                    && e.result == "ok"
                    && e.commit == commit
                    && e.channel.as_deref() == Some(req.as_str())
            });
            if held {
                verdict.notes.push(format!("gate: {req} already holds {} (ledger)", short(&commit)));
            } else {
                problems.push(format!("gate: {req} has not held {} yet; deploy it there first", short(&commit)));
            }
        }
    }
    let version_now = verdict.version.clone().unwrap_or_default();
    let mut gate_evidence = Vec::new();
    for name in &ch.checks {
        let Some(ck) = l.cfg.checks.get(name) else { continue };
        if ck.kind != "parity" {
            continue;
        }
        match parity_gate(l, &version_now) {
            Ok(msg) => {
                verdict.notes.push(format!("gate {name}: {msg}"));
                gate_evidence.push(format!("gate {name}: {msg}"));
            }
            Err(msg) => {
                crate::util::emit_event("gate-failed", &[("gate", name), ("channel", channel), ("reason", &msg)]);
                problems.push(format!("gate {name}: {msg}"));
            }
        }
    }
    for n in &verdict.notes {
        println!("policy: {n}");
    }
    if !problems.is_empty() {
        return refuse(&problems);
    }
    let version = verdict.version.take().unwrap_or_default();
    let tag = verdict.tag.clone().unwrap_or_default();
    let build_label = build.clone();
    let build = build.unwrap_or_default();
    let vars: Vec<(&str, &str)> = vec![
        ("commit", &commit),
        ("ref", r),
        ("version", &version),
        ("tag", &tag),
        ("build", &build),
        ("channel", channel),
        ("target", &ch.target),
    ];
    let mut steps = Vec::new();
    for name in &ch.checks {
        if let Some(ck) = l.cfg.checks.get(name).filter(|ck| ck.kind == "command") {
            let mut s = Step::new(Kind::Check, &format!("check {name}"), expand_argv(c, &ck.run, &vars), &l.root);
            s.env = ck.env.iter().map(|(k, v)| (k.clone(), l.expand(v))).collect();
            steps.push(s);
        }
    }
    let unresolved: Vec<String> = steps
        .iter()
        .flat_map(|s: &Step| s.argv.iter().chain(s.env.iter().map(|(_, v)| v)))
        .chain(expand_argv(c, &ch.run, &vars).iter())
        .chain(ch.env.values().map(|v| l.expand(v)).collect::<Vec<_>>().iter())
        .filter_map(|a| a.find("{var.").map(|i| a[i + 5..].split('}').next().unwrap_or("").to_string()))
        .collect();
    if !unresolved.is_empty() {
        let names: Vec<String> =
            unresolved.iter().map(|n| format!("EDITION_VAR_{}", n.to_ascii_uppercase().replace('-', "_"))).collect();
        return refuse(&[format!("channel {channel} needs {} set for this machine", names.join(", "))]);
    }
    let deploy_kind = if ch.tier == Tier::Dev { Kind::Local } else { Kind::Outward };
    let mut run = Step::new(
        deploy_kind,
        &format!("{} {} to {}", ch.kind, short(&commit), ch.target),
        expand_argv(c, &ch.run, &vars),
        &l.root,
    );
    run.env = ch.env.iter().map(|(k, v)| (k.clone(), l.expand(v))).collect();
    let run_idx = steps.len();
    steps.push(run);
    if !ch.probe.is_empty() {
        steps.push(Step::new(Kind::After, "probe", expand_argv(c, &ch.probe, &vars), &l.root));
    }
    let at = now_utc();
    let id = format!("{at}/{channel}/{}", short(&commit));
    let concordance_platform = ch
        .platform
        .as_ref()
        .filter(|_| l.cfg.concordance.is_some())
        .map(|p| l.cfg.platforms.get(p).and_then(|x| x.concordance.clone()).unwrap_or_else(|| p.clone()));
    if let (Some(platform), false) = (&concordance_platform, ch.tier == Tier::Dev) {
        let base = Version::parse(&version).map(|v| v.base().to_string()).unwrap_or(version.clone());
        if let Some(rid) = crate::concordance::release_id(l, &base) {
            if let Some(argv) =
                crate::concordance::log_uploaded(l, &rid, platform, &build, &commit, &format!("edition:{id}"), &c.by)
            {
                steps.push(Step::new(
                    Kind::After,
                    &format!("record build {build} for Concordance release {rid}"),
                    argv,
                    &l.root,
                ));
            }
        }
    }
    if ch.tier != Tier::Dev {
        steps.push(Step::note(&format!("append {id} to {}", l.cfg.ledger)));
    }
    let title = format!("Deploy {r} ({}) to {channel} [{} tier, {}]", short(&commit), ch.tier.name(), ch.target);
    steps::print(&title, &steps);
    let token = if ch.tier == Tier::Dev { "none".to_string() } else { channel.to_string() };
    let outcomes = match steps::execute(&steps, &c.mode, &token)? {
        Ran::DryRun => return Ok(0),
        Ran::Ran(o) => o,
    };
    if ch.tier == Tier::Dev {
        return Ok(if outcomes.iter().all(|o| o.ok()) { 0 } else { 1 });
    }
    let deploy_outcome = outcomes.iter().find(|o| o.argv == steps[run_idx].argv && o.kind == deploy_kind);
    let Some(d) = deploy_outcome else {
        eprintln!("edition: checks failed; nothing was deployed and nothing is recorded");
        return Ok(1);
    };
    let pins: Vec<String> = policy::pins(l, &c.git, Some(&commit), view.online())
        .iter()
        .map(|p| {
            format!(
                "{}={}{}",
                p.name,
                p.commit.clone().unwrap_or_else(|| p.value.clone().unwrap_or_default()),
                p.release_tag.as_ref().map(|t| format!(" ({t})")).unwrap_or_default()
            )
        })
        .collect();
    let mut evidence = gate_evidence;
    evidence.extend(
        outcomes
            .iter()
            .filter(|o| o.argv != steps[run_idx].argv)
            .map(|o| format!("{}: exit {}", o.what, o.code.map(|x| x.to_string()).unwrap_or_else(|| "none".into()))),
    );
    let e = Entry {
        id,
        at,
        by: c.by.clone(),
        action: "deploy".into(),
        product: l.cfg.product.clone(),
        channel: Some(channel.into()),
        tier: Some(ch.tier.name().into()),
        target: Some(ch.target.clone()),
        ref_: if tag.is_empty() { r.to_string() } else { tag.clone() },
        commit: commit.clone(),
        tree: c.git.tree(&commit),
        version: (!version.is_empty()).then(|| version.clone()),
        platform: ch.platform.clone(),
        build: build_label,
        result: if d.ok() { "ok".into() } else { "failed".into() },
        approved_by: c.approved_by.clone(),
        command: d.argv.clone(),
        evidence,
        pins,
        note: c.note.clone(),
    };
    c.record(&e)?;
    Ok(if outcomes.iter().all(|o| o.ok()) { 0 } else { 1 })
}

/// Deploys what `from` holds (per the ledger) to `to`. For a prod target the held
/// commit must carry a release tag; promote never tags anything itself.
pub fn promote(c: &Ctx, from: &str, to: &str) -> Res {
    let l = &c.l;
    let entries = ledger::load(&l.ledger_path())?;
    let current = ledger::current_by_channel(&entries);
    let Some((_, held)) = current.iter().find(|(n, _)| n == from) else {
        return refuse(&[format!("the ledger records nothing on {from}")]);
    };
    let to_ch = l.cfg.channels.get(to).ok_or_else(|| format!("no channel `{to}`"))?;
    let r = match to_ch.tier {
        Tier::Prod => {
            let view = RemoteView::build(&c.git, &l.cfg.remote, c.online)?;
            let tags = c.all_tags(&view);
            let at = policy::tags_at(l, &tags, &held.commit);
            match at.finals.first().map(|(n, _)| n.clone()).or_else(|| {
                if to_ch.allow_prerelease {
                    at.pres.first().map(|(n, _, _)| n.clone())
                } else {
                    None
                }
            }) {
                Some(t) => t,
                None => {
                    return refuse(&[format!(
                        "{from} holds {} ({}), which carries no release tag; tag it on its release line first (edition tag X.Y.Z)",
                        held.ref_,
                        short(&held.commit)
                    )])
                }
            }
        }
        _ => held.commit.clone(),
    };
    println!("promote: {from} holds {} ({}); deploying {r} to {to}\n", held.ref_, short(&held.commit));
    deploy(c, to, &r)
}

// ---------------------------------------------------------------- ledger

pub fn ledger_cmd(c: &Ctx, channel: Option<&str>) -> Res {
    let entries = ledger::load(&c.l.ledger_path())?;
    let mut n = 0;
    for e in entries.iter().filter(|e| channel.is_none_or(|ch| e.channel.as_deref() == Some(ch))) {
        n += 1;
        println!(
            "{}  {:<11} {:<20} {:<16} {} {} by {}",
            e.at,
            e.action,
            e.channel.clone().unwrap_or_else(|| "-".into()),
            e.ref_,
            short(&e.commit),
            e.result,
            e.by
        );
    }
    if n == 0 {
        println!("(no entries in {})", c.l.cfg.ledger);
    }
    Ok(0)
}

// ---------------------------------------------------------------- hooks

const HOOK: &str = "#!/bin/sh\n# Installed by `edition hooks install`: content, version and pin checks before a push.\nexec edition check-push \"$1\" \"$2\"\n";

pub fn hooks_install(c: &Ctx) -> Res {
    let dir = c.git.run(&["rev-parse", "--git-path", "hooks"])?;
    let dir = c.l.root.join(dir);
    let path = dir.join("pre-push");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if existing == HOOK {
            println!("pre-push hook already installed: {}", path.display());
            return Ok(0);
        }
        return refuse(&[format!(
            "{} exists and was not written by Edition; add `edition check-push \"$1\" \"$2\"` to it by hand",
            path.display()
        )]);
    }
    println!(
        "Install the pre-push hook:\n  [local] write {}\n{}",
        path.display(),
        HOOK.lines().map(|l| format!("      {l}\n")).collect::<String>()
    );
    if !c.mode.apply {
        println!("Dry run: nothing was written. To write it: --apply");
        return Ok(0);
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(&path, HOOK).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    println!("installed");
    Ok(0)
}
