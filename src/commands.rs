//! The commands. Every mutating command builds a plan, prints it, and runs it only with
//! `--apply`; a plan with outward steps also needs the receipt its dry run issued.

use crate::config::{whole_placeholder, Approval, Loaded, Tier};
use crate::git::Git;
use crate::ledger::{self, Entry};
use crate::policy::{self, RemoteView};
use crate::steps::{self, Kind, Mode, Plan, Ran, Step};
use crate::util::{now_utc, short, valid_actor, valid_build, valid_commit, valid_ref};
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
    /// Content-check findings explicitly allowed for this run (`--allow-finding`).
    pub allow_findings: Vec<String>,
}

pub type Res = Result<i32, String>;

/// A ledger entry to write for the outward step at `index` once the plan has run.
struct Record {
    index: usize,
    entry: Entry,
}

impl Ctx {
    /// The remote as remote-tracking refs say (read-only commands; unverified).
    fn view(&self) -> Result<RemoteView, String> {
        RemoteView::build(&self.git, &self.l.cfg.remote, self.online)
    }

    /// The remote's live heads and tags. Every plan with outward steps uses this; if the
    /// remote cannot be read, the plan is refused.
    fn live_view(&self) -> Result<RemoteView, String> {
        RemoteView::build(&self.git, &self.l.cfg.remote, true)
            .map_err(|e| format!("the remote state cannot be verified ({e}); outward steps are refused while offline"))
    }

    fn config_digest(&self) -> Result<String, String> {
        self.git.hash(&self.l.raw)
    }

    fn git_step(&self, kind: Kind, what: &str, args: &[&str]) -> Step {
        let mut argv = vec!["git".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        Step::plain(kind, what, argv, &self.l.root)
    }

    /// Outward plans run from a clean checkout on a branch, so the ledger is committed
    /// on top of exactly what was checked.
    fn clean_problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if self.git.is_detached() {
            p.push("HEAD is detached; check out a branch before running an outward plan".into());
        }
        let ledger = self.l.cfg.ledger.clone();
        let dirty: Vec<String> = self.git.dirty_paths().into_iter().filter(|d| *d != ledger).collect();
        if !dirty.is_empty() {
            p.push(format!("the working tree has uncommitted changes: {}", dirty.join(", ")));
        }
        p
    }

    fn input_problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if !valid_actor(&self.by) {
            p.push("the actor name (--by or EDITION_ACTOR) must be one printable line".into());
        }
        if let Some(a) = &self.approved_by {
            if !valid_actor(a) {
                p.push("--approved-by must be one printable line".into());
            }
        }
        if let Some(b) = &self.build {
            if !valid_build(b) {
                p.push(format!("build label {b:?} may use only letters, digits and dots"));
            }
        }
        if let Some(a) = &self.at {
            if !valid_ref(a) {
                p.push(format!("--at {a:?} is not a valid ref name"));
            }
        }
        p
    }

    /// Blocks a push to a public remote while what it would publish matches the
    /// deny-list, unless each finding is named with `--allow-finding`.
    fn content_gate(&self, view: &RemoteView, tips: &[String], plan: &mut Plan) -> (Vec<String>, Vec<String>) {
        if !self.l.is_public(&view.remote) {
            return (Vec::new(), Vec::new());
        }
        let found = crate::content::Scanner::new(&self.l).scan_publish(&self.git, tips, &view.tips());
        let mut problems = Vec::new();
        let mut overrides = Vec::new();
        for f in found {
            if self.allow_findings.contains(&f.id) {
                overrides.push(format!("{} {} [{}]", f.id, f.place, f.pattern));
                plan.fact("allowed-finding", &f.id);
            } else {
                problems.push(format!(
                    "content {}: {} [{}] {} (allow it for this run with --allow-finding {})",
                    f.id, f.place, f.pattern, f.excerpt, f.id
                ));
            }
        }
        if problems.is_empty() {
            plan.fact("content-check", &format!("{} clean", view.remote));
        }
        (problems, overrides)
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

    /// The next candidate number, compared numerically (rc.10 follows rc.9).
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
            result: "failed".into(),
            note: self.note.clone(),
            ..Entry::default()
        }
    }

    fn record(&self, e: &Entry) -> Result<Entry, String> {
        let written = ledger::append(&self.git, &self.l.ledger_path(), e)?;
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
        println!("recorded {} ({}) in {}", e.id, e.result, self.l.cfg.ledger);
        Ok(written)
    }

    fn receipt_used(&self) -> impl Fn(&str) -> bool {
        let entries = ledger::load(&self.l.ledger_path()).unwrap_or_default();
        move |r: &str| entries.iter().any(|e| e.receipt.as_deref() == Some(r))
    }

    /// Runs a plan and writes one ledger entry per record. An entry is `ok` when its
    /// step and everything after it succeeded, `partial` when its step succeeded and a
    /// later one failed, and `failed` when its step failed or never ran.
    fn run(&self, plan: &Plan, records: Vec<Record>, overrides: &[String]) -> Res {
        let digest = self.config_digest()?;
        let used = self.receipt_used();
        match steps::execute(plan, &self.mode, &self.git, &self.l.root, &digest, &used)? {
            Ran::DryRun => Ok(0),
            Ran::Ran { outcomes, receipt, stopped } => {
                let ran: Vec<String> = outcomes.iter().map(|o| o.describe()).collect();
                for r in records {
                    let mut e = r.entry;
                    let own = outcomes.iter().find(|o| o.index == r.index);
                    let later_failed = outcomes.iter().any(|o| o.index > r.index && !o.ok());
                    e.result = match own {
                        Some(o) if o.ok() && (later_failed || stopped.is_some()) => "partial".into(),
                        Some(o) if o.ok() => "ok".into(),
                        _ => "failed".into(),
                    };
                    e.command = vec![plan.steps[r.index].description()];
                    e.steps = ran.clone();
                    if let Some(why) = &stopped {
                        e.steps.push(format!("stopped: {why}"));
                    }
                    e.receipt = receipt.clone();
                    e.overrides = overrides.to_vec();
                    self.record(&e)?;
                }
                let all_ok = stopped.is_none() && outcomes.iter().all(|o| o.ok());
                Ok(if all_ok { 0 } else { 1 })
            }
        }
    }
}

fn refuse(problems: &[String]) -> Res {
    for p in problems {
        eprintln!("refused: {p}");
    }
    crate::util::emit_event("refused", &[("reason", &problems.join("; "))]);
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
    let mut problems = c.input_problems();
    problems.extend(c.clean_problems());
    let view = c.live_view()?;
    let branch = l.line_branch_name(line);
    if view.branch(&branch).is_some() {
        problems.push(format!("{}/{branch} already exists", l.cfg.remote));
    }
    if c.git.local_branch_exists(&branch) {
        problems.push(format!("a local branch {branch} already exists"));
    }
    let Some(main_tip) = view.branch(&l.cfg.main).map(str::to_string) else {
        return refuse(&[format!("{} has no {} branch", l.cfg.remote, l.cfg.main)]);
    };
    if c.git.resolve(&main_tip).is_none() {
        return refuse(&[format!(
            "{}/{} is at {}, which is not in this clone; fetch first",
            l.cfg.remote,
            l.cfg.main,
            short(&main_tip)
        )]);
    }
    let from_commit = match &c.at {
        Some(a) => match c.git.resolve(a) {
            Some(x) => x,
            None => return refuse(&[format!("{a} is not a commit")]),
        },
        None => main_tip.clone(),
    };
    if !c.git.is_ancestor(&from_commit, &main_tip) {
        problems.push(format!(
            "{} is not on {}/{}: a line is cut from main",
            short(&from_commit),
            l.cfg.remote,
            l.cfg.main
        ));
    }
    let want_dev = format!("{line}.0-{}", l.cfg.lines.dev_suffix);
    match policy::agreed_version(l, &c.git, Some(&from_commit)) {
        Ok(Some(v)) if v == want_dev => {}
        Ok(Some(v)) => println!("warning: main builds as {v}, not {want_dev}; check you are cutting the right line"),
        Ok(None) => {}
        Err(e) => problems.push(e),
    }
    let mut plan = Plan::new(&format!("Cut the {line} line from {}", short(&from_commit)), &l.cfg.remote);
    let (content, overrides) = c.content_gate(&view, std::slice::from_ref(&from_commit), &mut plan);
    problems.extend(content);
    if !problems.is_empty() {
        return refuse(&problems);
    }
    plan.fact("action", "cut");
    plan.fact("commit", &from_commit);
    let next_dev = format!("{ma}.{}.0-{}", mi + 1, l.cfg.lines.dev_suffix);
    plan.steps = vec![
        c.git_step(
            Kind::Local,
            &format!("create {branch} at {}", short(&from_commit)),
            &["branch", "--no-track", &branch, &from_commit],
        ),
        Step::note(&format!("on {branch}: edition bump {line}.0 --apply, commit, and land it on {branch}")),
        Step::note(&format!("on {}: edition bump {next_dev} --apply and land it through the usual review", l.cfg.main)),
        c.git_step(
            Kind::Outward,
            &format!("publish {branch}"),
            &["push", &l.cfg.remote, &format!("{from_commit}:refs/heads/{branch}")],
        )
        .expecting(&format!("refs/heads/{branch}"), None)
        .expecting(&format!("refs/heads/{}", l.cfg.main), Some(&main_tip)),
    ];
    let mut e = c.base_entry("push-branch", &branch, &from_commit);
    e.note = Some(c.note.clone().unwrap_or_else(|| format!("cut the {line} line")));
    c.run(&plan, vec![Record { index: 3, entry: e }], &overrides)
}

// ---------------------------------------------------------------- rc / tag

/// The steps that make and publish a tag, or the reasons they cannot be built.
struct TagParts {
    name: String,
    commit: String,
    line_branch: String,
    line_tip: String,
}

fn tag_parts(c: &Ctx, view: &RemoteView, version: &str, pre: bool, problems: &mut Vec<String>) -> Option<TagParts> {
    let l = &c.l;
    let v = match Version::parse(version) {
        Ok(v) if v.is_final() => v,
        Ok(v) => {
            problems.push(format!("give the release version (for example {}), not a pre-release", v.base()));
            return None;
        }
        Err(e) => {
            problems.push(e);
            return None;
        }
    };
    let line = v.line();
    let branch = l.line_branch_name(&line);
    let Some(line_tip) = view.branch(&branch).map(str::to_string) else {
        problems.push(format!("{}/{branch} does not exist (edition cut {line})", l.cfg.remote));
        return None;
    };
    if c.git.resolve(&line_tip).is_none() {
        problems.push(format!(
            "{}/{branch} is at {}, which is not in this clone; fetch first",
            l.cfg.remote,
            short(&line_tip)
        ));
        return None;
    }
    let commit = match &c.at {
        Some(a) => match c.git.resolve(a) {
            Some(x) => x,
            None => {
                problems.push(format!("{a} is not a commit"));
                return None;
            }
        },
        None => line_tip.clone(),
    };
    if !c.git.is_ancestor(&commit, &line_tip) {
        problems.push(format!(
            "{} is not on {}/{branch}: tags are made on the release line",
            short(&commit),
            l.cfg.remote
        ));
    }
    let (name, want) = if pre {
        let n = c.next_pre(view, &v);
        let want = if l.cfg.tags.prerelease_files == "full" { format!("{v}-rc.{n}") } else { v.to_string() };
        (l.pre_tag_name(&v.to_string(), n), want)
    } else {
        (l.final_tag_name(&v.to_string()), v.to_string())
    };
    if c.tag_exists(view, &name) {
        problems.push(format!("tag {name} already exists"));
    }
    if !pre {
        if let Some((newest, nv)) = c.finals_on_line(view, &line).first() {
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
    Some(TagParts { name, commit, line_branch: branch, line_tip })
}

fn push_tag_steps(c: &Ctx, t: &TagParts) -> Vec<Step> {
    let msg = format!("{} {}", c.l.cfg.product, t.name);
    vec![
        c.git_step(
            Kind::Local,
            &format!("tag {} as {}", short(&t.commit), t.name),
            &["tag", "-a", &t.name, &t.commit, "-m", &msg],
        ),
        c.git_step(
            Kind::Outward,
            &format!("publish {}", t.name),
            &["push", &c.l.cfg.remote, &format!("refs/tags/{}", t.name)],
        )
        .expecting(&format!("refs/tags/{}", t.name), None)
        .expecting(&format!("refs/heads/{}", t.line_branch), Some(&t.line_tip)),
    ]
}

fn tag_plan(c: &Ctx, version: &str, pre: bool) -> Res {
    let mut problems = c.input_problems();
    problems.extend(c.clean_problems());
    let view = c.live_view()?;
    let Some(t) = tag_parts(c, &view, version, pre, &mut problems) else { return refuse(&problems) };
    let title = format!("{} {} at {}", if pre { "Pre-release" } else { "Release" }, t.name, short(&t.commit));
    let mut plan = Plan::new(&title, &c.l.cfg.remote);
    let (content, overrides) = c.content_gate(&view, std::slice::from_ref(&t.commit), &mut plan);
    problems.extend(content);
    if !problems.is_empty() {
        return refuse(&problems);
    }
    plan.fact("action", "tag");
    plan.fact("tag", &t.name);
    plan.fact("commit", &t.commit);
    plan.steps = push_tag_steps(c, &t);
    let e = c.base_entry("push-tag", &t.name, &t.commit);
    c.run(&plan, vec![Record { index: 1, entry: e }], &overrides)
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
    let mut plan = Plan::new(&format!("Hotfix {next} on the {line} line"), &l.cfg.remote);
    plan.steps = vec![
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
    c.run(&plan, Vec::new(), &[])
}

/// One plan, one receipt: tag the hotfix on its line, publish the tag, and open the
/// forward merge into main. Every step's remote expectations are checked before it runs.
pub fn hotfix_finish(c: &Ctx, version: &str) -> Res {
    let l = &c.l;
    let v = Version::parse(version)?;
    if v.patch == 0 {
        return Err(format!("{v} is not a hotfix version (its patch number is 0)"));
    }
    let mut problems = c.input_problems();
    problems.extend(c.clean_problems());
    let view = c.live_view()?;
    let Some(t) = tag_parts(c, &view, version, false, &mut problems) else { return refuse(&problems) };
    let Some(main_tip) = view.branch(&l.cfg.main).map(str::to_string) else {
        return refuse(&[format!("{} has no {} branch", l.cfg.remote, l.cfg.main)]);
    };
    let mut plan = Plan::new(&format!("Finish hotfix {} at {}", t.name, short(&t.commit)), &l.cfg.remote);
    let (content, overrides) = c.content_gate(&view, std::slice::from_ref(&t.commit), &mut plan);
    problems.extend(content);
    if !problems.is_empty() {
        return refuse(&problems);
    }
    plan.fact("action", "hotfix-finish");
    plan.fact("tag", &t.name);
    plan.fact("commit", &t.commit);
    plan.steps = push_tag_steps(c, &t);
    let tag_index = plan.steps.len() - 1;
    let pr = forward_step(c, &t.line_branch, &t.line_tip, &main_tip)
        .expecting(&format!("refs/tags/{}", t.name), Some(&t.commit));
    plan.steps.push(pr);
    let pr_index = plan.steps.len() - 1;
    let tag_entry = c.base_entry("push-tag", &t.name, &t.commit);
    let mut pr_entry = c.base_entry("open-pr", &t.line_branch, &t.line_tip);
    pr_entry.note = Some(format!("forward merge of {} into {}", t.line_branch, l.cfg.main));
    c.run(
        &plan,
        vec![Record { index: tag_index, entry: tag_entry }, Record { index: pr_index, entry: pr_entry }],
        &overrides,
    )
}

// ---------------------------------------------------------------- forward

fn forward_step(c: &Ctx, branch: &str, tip: &str, main_tip: &str) -> Step {
    let l = &c.l;
    let title = format!("Merge {branch} forward into {}", l.cfg.main);
    let body = format!(
        "Brings the commits shipped on {branch} into {}. Release lines merge forward only; {} is never merged into {branch}.\n\nOpened by edition.",
        l.cfg.main, l.cfg.main
    );
    Step::plain(
        Kind::Outward,
        &format!("open a pull request merging {branch} into {}", l.cfg.main),
        vec![
            "gh".into(),
            "pr".into(),
            "create".into(),
            "--base".into(),
            l.cfg.main.clone(),
            "--head".into(),
            branch.to_string(),
            "--title".into(),
            title,
            "--body".into(),
            body,
        ],
        &l.root,
    )
    .expecting(&format!("refs/heads/{branch}"), Some(tip))
    .expecting(&format!("refs/heads/{}", l.cfg.main), Some(main_tip))
}

pub fn forward(c: &Ctx) -> Res {
    let l = &c.l;
    let mut problems = c.input_problems();
    problems.extend(c.clean_problems());
    let view = c.live_view()?;
    let Some(main_tip) = view.branch(&l.cfg.main).map(str::to_string) else {
        return refuse(&[format!("{} has no {} branch", l.cfg.remote, l.cfg.main)]);
    };
    let mut plan = Plan::new("Forward merges", &l.cfg.remote);
    plan.fact("action", "forward");
    let mut records = Vec::new();
    for (name, tip) in &view.branches {
        let Some(caps) = l.line_branch.matches(name) else { continue };
        if crate::version::Template::get(&caps, "line").is_none_or(|x| parse_line(x).is_err()) {
            continue;
        }
        if c.git.resolve(tip).is_none() || c.git.resolve(&main_tip).is_none() {
            problems.push(format!(
                "{}/{name} or {}/{} is not in this clone; fetch first",
                l.cfg.remote, l.cfg.remote, l.cfg.main
            ));
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
        plan.steps.push(forward_step(c, name, tip, &main_tip));
        let mut e = c.base_entry("open-pr", name, tip);
        e.note = Some(format!("forward merge of {name} into {}", l.cfg.main));
        records.push(Record { index: plan.steps.len() - 1, entry: e });
    }
    if !problems.is_empty() {
        return refuse(&problems);
    }
    if plan.steps.is_empty() {
        println!("Every release line is merged forward.");
        return Ok(0);
    }
    println!();
    c.run(&plan, records, &[])
}

// ---------------------------------------------------------------- deploy / promote

/// Expands a configured command. A placeholder is always a whole argument, so no value
/// is ever spliced into other text; values from the environment are printed by name.
fn expand_cmd(c: &Ctx, template: &[String], vals: &[(&str, &str)]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut argv = Vec::new();
    let mut shown = Vec::new();
    for a in template {
        match whole_placeholder(a) {
            Some(n) if n.starts_with("var.") => {
                let name = &n[4..];
                let Some((v, secret)) = c.l.var(name) else {
                    return Err(format!(
                        "needs EDITION_VAR_{} set for this machine",
                        name.to_ascii_uppercase().replace('-', "_")
                    ));
                };
                if v.starts_with('-') || v.chars().any(|ch| ch.is_control()) {
                    return Err(format!(
                        "the value of var.{name} must not start with `-` or contain control characters"
                    ));
                }
                let v = c.l.expand(&v);
                shown.push(if secret { format!("<var.{name}>") } else { v.clone() });
                argv.push(v);
            }
            Some(n) => {
                let v = vals.iter().find(|(k, _)| *k == n).map(|(_, v)| *v).unwrap_or("");
                if v.is_empty() {
                    return Err(format!("{{{n}}} has no value for this deploy"));
                }
                argv.push(v.to_string());
                shown.push(v.to_string());
            }
            None => {
                let v = c.l.expand(a);
                argv.push(v.clone());
                shown.push(v);
            }
        }
    }
    Ok((argv, shown))
}

fn cmd_step(
    c: &Ctx,
    kind: Kind,
    what: &str,
    template: &[String],
    env: &std::collections::BTreeMap<String, String>,
    vals: &[(&str, &str)],
) -> Result<Step, String> {
    let (argv, shown) = expand_cmd(c, template, vals)?;
    let mut env_out = Vec::new();
    for (k, v) in env {
        let (vs, _) = expand_cmd(c, std::slice::from_ref(v), vals)?;
        env_out.push((k.clone(), vs.into_iter().next().unwrap_or_default()));
    }
    Ok(Step {
        kind,
        what: what.to_string(),
        argv,
        shown,
        template: template.to_vec(),
        env: env_out,
        cwd: c.l.root.clone(),
        expects: Vec::new(),
    })
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
    let mut problems = c.input_problems();
    if !valid_ref(r) {
        return refuse(&[format!("{r:?} is not a valid ref name")]);
    }
    let dev = ch.tier == Tier::Dev;
    if !dev {
        problems.extend(c.clean_problems());
    }
    // Every recorded tier reads the remote live; dev never needs the remote.
    let view = if dev { c.view()? } else { c.live_view()? };
    let commit = match ch.tier {
        Tier::Staging if r == l.cfg.main => view.branch(&l.cfg.main).map(str::to_string),
        _ => c.git.resolve(r),
    };
    let Some(commit) = commit else { return refuse(&[format!("{r} is not a commit here")]) };
    if !valid_commit(&commit) || c.git.resolve(&commit).is_none() {
        return refuse(&[format!("{} is not in this clone; fetch first", short(&commit))]);
    }
    let mut verdict = policy::check_tier(l, &c.git, &view, ch.tier, ch.allow_prerelease, r, &commit);
    problems.extend(verdict.violations.clone());
    if [ch.run.join(" "), ch.probe.join(" "), ch.target.clone()].iter().any(|t| t.contains(crate::config::TODO_MARK)) {
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
    if let Some(b) = &build {
        if !valid_build(b) {
            problems.push(format!("build label {b:?} read from the build file may use only letters, digits and dots"));
        }
    }
    if ch.platform.is_some() && l.cfg.concordance.is_some() && !dev && build.is_none() {
        problems.push(format!(
            "channel {channel} ships the {} platform: pass --build <label> (or configure its build_file)",
            ch.platform.clone().unwrap_or_default()
        ));
    }
    // Readiness gate: channels that must already hold this commit, per the ledger.
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
    if !view.online() {
        verdict.notes.push(format!("remote state unverified: {} was not read (dev tier only)", l.cfg.remote));
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
    let vals: Vec<(&str, &str)> = vec![
        ("commit", &commit),
        ("ref", r),
        ("version", &version),
        ("tag", &tag),
        ("build", &build),
        ("channel", channel),
        ("target", &ch.target),
    ];
    let title = format!("Deploy {r} ({}) to {channel} [{} tier, {}]", short(&commit), ch.tier.name(), ch.target);
    let mut plan = Plan::new(&title, &l.cfg.remote);
    for (k, v) in [
        ("action", "deploy"),
        ("channel", channel),
        ("tier", ch.tier.name()),
        ("ref", r),
        ("commit", &commit),
        ("tag", &tag),
        ("version", &version),
        ("build", &build),
    ] {
        plan.fact(k, v);
    }
    if let Some(a) = &c.approved_by {
        plan.fact("approved-by", a);
    }
    let pins = policy::pins(l, &c.git, Some(&commit), view.online());
    for p in &pins {
        plan.fact("pin", &format!("{}={}", p.name, p.commit.clone().unwrap_or_default()));
    }
    let built: Result<Vec<Step>, String> = (|| {
        let mut steps = Vec::new();
        for name in &ch.checks {
            if let Some(ck) = l.cfg.checks.get(name).filter(|ck| ck.kind == "command") {
                steps.push(cmd_step(c, Kind::Check, &format!("check {name}"), &ck.run, &ck.env, &vals)?);
            }
        }
        let kind = if dev { Kind::Local } else { Kind::Outward };
        let mut run =
            cmd_step(c, kind, &format!("{} {} to {}", ch.kind, short(&commit), ch.target), &ch.run, &ch.env, &vals)?;
        match ch.tier {
            Tier::Staging => run
                .expects
                .push(steps::Expect { refname: format!("refs/heads/{}", l.cfg.main), oid: Some(commit.clone()) }),
            Tier::Prod => {
                run.expects.push(steps::Expect { refname: format!("refs/tags/{tag}"), oid: Some(commit.clone()) });
                if let Ok(v) = Version::parse(&version) {
                    let line = l.line_branch_name(&v.line());
                    if let Some(tip) = view.branch(&line) {
                        run.expects
                            .push(steps::Expect { refname: format!("refs/heads/{line}"), oid: Some(tip.to_string()) });
                    }
                }
            }
            Tier::Dev => {}
        }
        steps.push(run);
        if !ch.probe.is_empty() {
            steps.push(cmd_step(c, Kind::After, "probe", &ch.probe, &std::collections::BTreeMap::new(), &vals)?);
        }
        Ok(steps)
    })();
    let mut steps_list = match built {
        Ok(s) => s,
        Err(e) => return refuse(&[format!("channel {channel}: {e}")]),
    };
    let run_index =
        steps_list.iter().position(|s| s.kind == Kind::Outward || (dev && s.kind == Kind::Local)).unwrap_or(0);
    let concordance_platform = ch
        .platform
        .as_ref()
        .filter(|_| l.cfg.concordance.is_some())
        .map(|p| l.cfg.platforms.get(p).and_then(|x| x.concordance.clone()).unwrap_or_else(|| p.clone()));
    if let (Some(platform), false) = (&concordance_platform, dev) {
        let base = Version::parse(&version).map(|v| v.base().to_string()).unwrap_or(version.clone());
        if let Some(rid) = crate::concordance::release_id(l, &base) {
            let key = format!("edition:{channel}:{}:{build}", short(&commit));
            if let Some(argv) = crate::concordance::log_uploaded(l, &rid, platform, &build, &commit, &key, &c.by) {
                steps_list.push(Step::plain(
                    Kind::After,
                    &format!("record build {build} for Concordance release {rid}"),
                    argv,
                    &l.root,
                ));
            }
        }
    }
    if !dev {
        steps_list.push(Step::note(&format!("append an entry to {}", l.cfg.ledger)));
    }
    plan.steps = steps_list;
    if dev {
        return c.run(&plan, Vec::new(), &[]);
    }
    let pin_text: Vec<String> = pins
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
    let mut e = c.base_entry("deploy", if tag.is_empty() { r } else { &tag }, &commit);
    e.id = format!("{}/{channel}/{}", e.at, short(&commit));
    e.channel = Some(channel.into());
    e.tier = Some(ch.tier.name().into());
    e.target = Some(ch.target.clone());
    e.version = (!version.is_empty()).then(|| version.clone());
    e.platform = ch.platform.clone();
    e.build = build_label;
    e.approved_by = c.approved_by.clone();
    e.evidence = gate_evidence;
    e.pins = pin_text;
    c.run(&plan, vec![Record { index: run_index, entry: e }], &[])
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
            let view = c.live_view()?;
            let tags = c.all_tags(&view);
            let at = policy::tags_at(l, &tags, &held.commit);
            let pick = at.finals.first().map(|(n, _)| n.clone()).or_else(|| {
                if to_ch.allow_prerelease {
                    at.pres.first().map(|(n, _, _)| n.clone())
                } else {
                    None
                }
            });
            match pick {
                Some(t) => format!("refs/tags/{t}"),
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
