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

/// Content findings for a ledger entry: the allowed ones as `(id, description)`, and the
/// blocking ones as messages.
type Findings = (Vec<(String, String)>, Vec<String>);

/// A ledger entry to write for the outward step at `index` once the plan has run.
#[derive(Clone)]
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
        argv.extend(crate::git::SAFE_CONFIG.iter().map(|s| s.to_string()));
        argv.extend(args.iter().map(|s| s.to_string()));
        Step::plain(kind, what, argv, &self.l.root)
    }

    /// Binds a plan to the remote's resolved URLs: reads and re-checks go to the fetch
    /// URL, pushes to the push URL, and all three URLs are part of the plan hash.
    fn bind(&self, plan: &mut Plan, view: &RemoteView) -> Result<(), String> {
        let pending = crate::pending::Pending::at(&self.l.root).list()?;
        if let Some((_, _, entries)) = pending.first() {
            let what: Vec<String> = entries
                .iter()
                .map(|e| format!("{} ({}, {})", e.id, e.channel.as_deref().unwrap_or(&e.action), e.result))
                .collect();
            return Err(format!(
                "an earlier run's outcome is not yet on the ledger: {}. Publish it with `edition ledger reconcile` (or record it as abandoned with `edition ledger reconcile --abandon`) before running another outward plan",
                what.join(", ")
            ));
        }
        self.bind_unchecked(plan, view)
    }

    /// Binds the audited git configuration chain in the plan: every file's identity
    /// and every setting that changes how git reaches a remote or signs.
    fn bind_config(&self, plan: &mut Plan, view: &RemoteView) -> Result<(), String> {
        let chain = view.config.clone().ok_or("the git configuration was not audited")?;
        for f in &chain.files {
            plan.fact("git-config-file", &format!("{} {} {}", f.scope, f.path, f.identity));
        }
        for (scope, key, value) in &chain.sensitive {
            plan.fact("git-config", &format!("{scope} {key}={value}"));
        }
        plan.git_config = chain;
        Ok(())
    }

    fn bind_unchecked(&self, plan: &mut Plan, view: &RemoteView) -> Result<(), String> {
        self.bind_config(plan, view)?;
        plan.remote = view.fetch_url.clone();
        plan.fact("fetch-url", &view.fetch_url);
        plan.fact("push-url", &view.push_url);
        plan.fact("configured-url", &view.raw_url);
        // The ledger is read from, and will be pushed to, the same remote.
        self.store().sync(&view.fetch_url)?;
        plan.record_to = Some((self.l.cfg.ledger_branch.clone(), view.push_url.clone()));
        Ok(())
    }

    pub fn store(&self) -> crate::ledger_store::Store<'_> {
        crate::ledger_store::Store::new(&self.git, &self.l.cfg.ledger_branch)
    }

    /// Ledger entries: synchronised with the remote when online, else as last seen.
    fn entries(&self, view: Option<&RemoteView>) -> Result<Vec<Entry>, String> {
        if let Some(v) = view.filter(|v| v.online()) {
            self.store().sync(&v.fetch_url)?;
        }
        self.store().load_local()
    }

    /// Outward plans run from a clean checkout on a branch, so the configuration and
    /// version files are exactly what was committed.
    fn clean_problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if self.git.is_detached() {
            p.push(format!(
                "HEAD is detached; check out a branch first (a branch at the same commit as the remote tip is enough, for example `git switch -c release-run {}/{}`)",
                self.l.cfg.remote, self.l.cfg.main
            ));
        }
        let dirty: Vec<String> = self.git.dirty_paths();
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
        let found = match crate::content::Scanner::new(&self.l).scan_publish(&self.git, tips, &view.tips()) {
            Ok(f) => f,
            Err(e) => return (vec![format!("{e}; the plan is refused (the scan fails closed)")], Vec::new()),
        };
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

    fn record(&self, e: &Entry, plan: &Plan) -> Result<Entry, String> {
        // Nothing that reaches the ledger may carry credentials from a URL.
        let r = crate::util::redact;
        let mut e = e.clone();
        for v in [&mut e.command, &mut e.evidence, &mut e.steps, &mut e.pins, &mut e.overrides] {
            for x in v.iter_mut() {
                *x = r(x);
            }
        }
        e.target = e.target.as_deref().map(r);
        e.note = e.note.as_deref().map(r);
        let e = &e;
        let Some((branch, push_url)) = &plan.record_to else {
            return Err("this plan has no ledger destination".into());
        };
        // The ledger branch goes to the same remote: a public one gets the same scan.
        // Only findings allowed when the plan was made (and so bound in its receipt) may
        // pass; anything new blocks and the outcome stays pending.
        // Every finding in the final entry must have been allowed in the plan the receipt
        // was issued for; an id given on this command line counts for nothing here.
        let blocked: Vec<String> = self
            .ledger_finding_list(std::slice::from_ref(e))
            .into_iter()
            .filter(|f| !plan.allowed_findings.contains(&f.id))
            .map(|f| format!("{} {} [{}] {} (not allowed when the plan was made)", f.id, f.place, f.pattern, f.excerpt))
            .collect();
        if !blocked.is_empty() {
            return Err(format!("the ledger entry matches the content deny-list: {}", blocked.join("; ")));
        }
        let written = self.store().append(&plan.remote, push_url, e)?;
        let _ = branch;
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
        crate::say!("recorded {} ({}) on the {} branch", e.id, e.result, self.l.cfg.ledger_branch);
        Ok(written)
    }

    /// Scans ledger entries bound for a public remote. Returns the findings allowed by
    /// `--allow-finding` as `(id, description)` and the blocking ones as messages. The
    /// finding's place names the action and channel, not the entry id, so it is the
    /// same when planned and when applied.
    /// Every content finding in ledger entries bound for a public remote, with no
    /// allowance applied.
    fn ledger_finding_list(&self, entries: &[Entry]) -> Vec<crate::content::Finding> {
        if !self.l.is_public(&self.l.cfg.remote) {
            return Vec::new();
        }
        let scanner = crate::content::Scanner::new(&self.l);
        let mut found = Vec::new();
        for e in entries {
            let place = format!("ledger entry ({} {})", e.action, e.channel.as_deref().unwrap_or(&e.ref_));
            // `overrides` is Edition's own record of the findings allowed; it quotes the
            // patterns, so it is not scanned itself.
            let mut scanned = ledger::redacted(e);
            scanned.overrides.clear();
            for line in ledger::render(&scanned).lines() {
                scanner.scan_line(&place, line, &mut found);
            }
        }
        crate::content::identify(&self.git, found)
    }

    fn ledger_findings(&self, plan: &Plan, entries: &[Entry]) -> Result<Findings, String> {
        let _ = plan;
        let mut allowed = Vec::new();
        let mut blocked = Vec::new();
        for f in self.ledger_finding_list(entries) {
            if self.allow_findings.contains(&f.id) {
                allowed.push((f.id.clone(), format!("{} {} [{}]", f.id, f.place, f.pattern)));
            } else {
                blocked.push(format!(
                    "{} {} [{}] {} (allow it for this run with --allow-finding {})",
                    f.id, f.place, f.pattern, f.excerpt, f.id
                ));
            }
        }
        Ok((allowed, blocked))
    }

    fn receipt_used(&self) -> impl Fn(&str) -> bool {
        let entries = self.store().load_local().unwrap_or_default();
        move |r: &str| entries.iter().any(|e| e.receipt.as_deref() == Some(r))
    }

    /// Runs a plan and writes one ledger entry per record. An entry is `ok` when its
    /// step and everything after it succeeded, `partial` when its step succeeded and a
    /// later one failed, and `failed` when its step failed or never ran.
    fn run(&self, plan: &Plan, records: Vec<Record>, overrides: &[String]) -> Res {
        let digest = self.config_digest()?;
        let used = self.receipt_used();
        let mut plan = plan.clone();
        plan.run_path = self.l.run_path();
        plan.tool_env = self.git.env.clone();
        plan.trusted_dirs = self.l.trusted_dirs.clone();
        plan.skipped_dirs = self.l.skipped_dirs.clone();
        plan.local_dirs = self.l.local_dirs.clone();
        if let Err(e) = plan.resolve() {
            return refuse(&[e]);
        }
        // The prospective ledger entries are scanned while the plan is made, so every
        // override is part of the receipt.
        let mut overrides = overrides.to_vec();
        if plan.record_to.is_some() && !records.is_empty() {
            let prospective = finalize(&plan, &records, &[], None, None, &overrides, true);
            match self.ledger_findings(&plan, &prospective) {
                Ok((allowed, blocked)) => {
                    if !blocked.is_empty() {
                        return refuse(&blocked);
                    }
                    for (id, text) in allowed {
                        plan.fact("allowed-ledger-finding", &id);
                        plan.allowed_findings.push(id);
                        overrides.push(text);
                    }
                }
                Err(e) => return refuse(&[e]),
            }
        }
        let overrides = &overrides[..];
        let plan = &plan;
        let pending = crate::pending::Pending::at(&self.l.root);
        // Run state is created only once the receipt has been validated and spent (the
        // first call of `on_step`), and only from that validated receipt. Then it is
        // rewritten after every step, so an outcome survives a crash or an outage.
        let records_apply = self.mode.apply && !records.is_empty();
        let created = std::cell::RefCell::new(None::<String>);
        let mut on_step = |outcomes: &[steps::Outcome]| -> Result<(), String> {
            if !records_apply {
                return Ok(());
            }
            let Some(k) = self.mode.confirm.as_deref().filter(|k| crate::util::valid_receipt(k)) else {
                return Err("no validated receipt for the pending record".into());
            };
            pending.write(k, &finalize(plan, &records, outcomes, None, Some(k), overrides, true))?;
            *created.borrow_mut() = Some(k.to_string());
            Ok(())
        };
        let ran = steps::execute(
            plan,
            &self.mode,
            &self.git,
            &self.l.root,
            &digest,
            self.l.receipt_ttl(),
            &used,
            &mut on_step,
        );
        let key = created.borrow().clone();
        let (outcomes, receipt, stopped) = match ran {
            Err(e) => {
                // A refused receipt created nothing; a later failure keeps the record.
                return Err(e);
            }
            Ok(Ran::DryRun) => return Ok(0),
            Ok(Ran::Ran { outcomes, receipt, stopped }) => (outcomes, receipt, stopped),
        };
        let entries = finalize(plan, &records, &outcomes, stopped.as_deref(), receipt.as_deref(), overrides, false);
        if let Some(k) = &key {
            pending.write(k, &entries)?;
        }
        for e in &entries {
            if let Err(err) = self.record(e, plan) {
                return Err(format!(
                    "{err}\nThe outcome is kept in .edition/pending and nothing will be re-run: publish it with `edition ledger reconcile`."
                ));
            }
        }
        if let Some(k) = &key {
            pending.remove(k)?;
        }
        let all_ok = stopped.is_none() && outcomes.iter().all(|o| o.ok());
        Ok(if all_ok { 0 } else { 1 })
    }
}

/// The ledger entries for a run so far. An entry is `ok` when its step and everything
/// after it succeeded, `partial` when its step succeeded and a later one failed, and
/// `failed` when its step failed or never ran. While the run is still going
/// (`in_progress`), an entry whose step has not finished is `unknown`.
fn finalize(
    plan: &Plan,
    records: &[Record],
    outcomes: &[steps::Outcome],
    stopped: Option<&str>,
    receipt: Option<&str>,
    overrides: &[String],
    in_progress: bool,
) -> Vec<Entry> {
    let ran: Vec<String> = outcomes.iter().map(|o| o.describe()).collect();
    records
        .iter()
        .map(|r| {
            let mut e = r.entry.clone();
            let own = outcomes.iter().find(|o| o.index == r.index);
            let later_failed = outcomes.iter().any(|o| o.index > r.index && !o.ok());
            e.result = match own {
                None if in_progress => "unknown".into(),
                Some(o) if o.ok() && (later_failed || stopped.is_some()) => "partial".into(),
                Some(o) if o.ok() => "ok".into(),
                _ => "failed".into(),
            };
            e.command = vec![plan.steps[r.index].description()];
            e.steps = ran.clone();
            if let Some(why) = stopped {
                e.steps.push(format!("stopped: {why}"));
            }
            if let Some(rc) = receipt {
                e.id = format!("{}/{}", e.id, &rc[..8.min(rc.len())]);
            }
            e.receipt = receipt.map(str::to_string);
            e.overrides = overrides.to_vec();
            e
        })
        .collect()
}

fn refuse(problems: &[String]) -> Res {
    let problems: Vec<String> = problems.iter().map(|p| crate::util::redact(p)).collect();
    for p in &problems {
        crate::warn!("refused: {p}");
    }
    crate::util::emit_event("refused", &[("reason", &problems.join("; "))]);
    Ok(1)
}
// ---------------------------------------------------------------- status

pub fn status(c: &Ctx) -> Res {
    let l = &c.l;
    let view = c.view()?;
    crate::say!("Edition: {} ({})", l.cfg.product, l.root.display());
    for (_, _, entries) in crate::pending::Pending::at(&l.root).list()? {
        for e in entries {
            crate::say!(
                "UNRECONCILED  {} ({}, {}): not yet on the ledger; run `edition ledger reconcile`",
                e.id,
                e.channel.as_deref().unwrap_or(&e.action),
                e.result
            );
        }
    }
    if !l.local_dirs.is_empty() {
        crate::say!("local path entries: {}", l.local_dirs.join(", "));
    }
    let main = &l.cfg.main;
    match view.branch(main) {
        Some(tip) => {
            let v = policy::agreed_version(l, &c.git, Some(tip))
                .map(|v| v.unwrap_or_else(|| "-".into()))
                .unwrap_or_else(|e| e);
            crate::say!("main   {}/{main} {} builds as {v}", l.cfg.remote, short(tip));
        }
        None => crate::say!("main   {}/{main} unknown (fetch first)", l.cfg.remote),
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
        crate::say!(
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
        crate::say!("line   none");
    }
    let entries = c.entries(Some(&view))?;
    let current = ledger::current_by_channel(&entries);
    crate::say!("\nchannels:");
    for (name, ch) in &l.cfg.channels {
        let held = current
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, e)| {
                let freshness = match (ch.tier, view.branch(&l.cfg.main)) {
                    (Tier::Staging, Some(tip)) if tip == e.commit => ", current",
                    (Tier::Staging, Some(_)) => ", behind main",
                    _ => "",
                };
                format!("{} ({}) {}{freshness}", e.ref_, short(&e.commit), e.at)
            })
            .unwrap_or_else(
                || if ch.tier == Tier::Dev { "not recorded (dev)".into() } else { "nothing recorded".into() },
            );
        crate::say!("  {name:<22} {:<8} {:<10} {:<40} {held}", ch.tier.name(), ch.kind, ch.target);
    }
    if !l.cfg.platforms.is_empty() {
        let names: Vec<&String> = l.cfg.platforms.keys().collect();
        crate::say!("\nplatforms (version, build) per tier, from the ledger:");
        crate::say!("  {:<10} {}", "", names.iter().map(|n| format!("{n:<24}")).collect::<String>());
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
            crate::say!("  {:<10} {cells}", tier.name());
        }
    }
    if !l.todos.is_empty() {
        crate::say!("\n{} open to-do marker(s) in {}:", l.todos.len(), crate::config::FILE_NAME);
        for t in &l.todos {
            crate::say!("  {t}");
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
    crate::say!("Release {} {v}", l.cfg.product);
    if !l.trusted_dirs.is_empty() {
        crate::say!("  trusting group-writable PATH directories: {}", l.trusted_dirs.join(", "));
    }
    let tip = view.branch(&branch).map(str::to_string);
    match &tip {
        Some(t) => {
            let fv = policy::agreed_version(l, &c.git, Some(t)).map(|x| x.unwrap_or_default()).unwrap_or_else(|e| e);
            crate::say!("  line {branch}: cut, tip {} builds as {fv}", short(t));
        }
        None => crate::say!("  line {branch}: not cut (edition cut {line})"),
    }
    let final_name = l.final_tag_name(&v.to_string());
    crate::say!("  final tag {final_name}: {}", if c.tag_exists(&view, &final_name) { "exists" } else { "not yet" });
    let next = c.next_pre(&view, &v);
    crate::say!(
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
            crate::say!("  {}/{} is {n} commit(s) ahead of {prev}", l.cfg.remote, l.cfg.main);
        }
    }
    match &l.cfg.concordance {
        None => {
            crate::say!(
                "  carries: Edition keeps no feature list; no [concordance] catalog is configured for this repo"
            )
        }
        Some(_) => match crate::concordance::releases(l) {
            Ok(rs) => {
                let rid = crate::concordance::release_id(l, &v.to_string()).unwrap_or_default();
                match rs.iter().find(|r| r.id == rid) {
                    Some(r) => {
                        let ready = r.carries.iter().filter(|x| x.ready).count();
                        crate::say!(
                            "  Concordance {rid}: {} \"{}\", {ready}/{} carried items ready, builds {:?}",
                            r.state,
                            r.title.clone().unwrap_or_default(),
                            r.carries.len(),
                            r.builds
                        );
                        for x in r.carries.iter().filter(|x| !x.ready) {
                            crate::say!("    not ready: {}", x.id);
                        }
                    }
                    None => {
                        crate::say!("  Concordance: no release {rid} in the tracker (add it to tracker/releases.yaml)")
                    }
                }
            }
            Err(e) => crate::say!("  Concordance: {e}"),
        },
    }
    crate::say!("\n  channels:");
    for (name, ch) in &l.cfg.channels {
        let takes = match ch.tier {
            Tier::Dev => "any ref (local only)".to_string(),
            Tier::Staging => format!("the tip of {}/{}", l.cfg.remote, l.cfg.main),
            Tier::Prod if ch.allow_prerelease => format!("{final_name} or a pre-release tag of it"),
            Tier::Prod => final_name.clone(),
        };
        crate::say!("    {name:<22} {:<8} takes {takes}", ch.tier.name());
    }
    crate::say!("\n  steps:");
    if tip.is_none() {
        crate::say!("    edition cut {line}");
    }
    crate::say!("    edition rc {v}        # optional, for store review builds");
    crate::say!("    edition tag {v}");
    crate::say!("    edition deploy <prod channel> {final_name}");
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
        let text = crate::safefs::read(&l.root, &vf.path)?;
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
    crate::say!("Set version {v} in:");
    for (p, _, n) in &changes {
        crate::say!("  [local] {} ({n} occurrence(s))", p.strip_prefix(&l.root).unwrap_or(p).display());
    }
    if !c.mode.apply {
        crate::say!("\nDry run: nothing was written. To write it: --apply");
        return Ok(0);
    }
    for (p, out, _) in &changes {
        let rel = p.strip_prefix(&l.root).map(|r| r.display().to_string()).unwrap_or_default();
        crate::safefs::replace(&l.root, &rel, out)?;
    }
    crate::say!("written; review with git diff, then commit (lock files may need regenerating by their tool)");
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
        Ok(Some(v)) => crate::say!("warning: main builds as {v}, not {want_dev}; check you are cutting the right line"),
        Ok(None) => {}
        Err(e) => problems.push(e),
    }
    let mut plan = Plan::new(&format!("Cut the {line} line from {}", short(&from_commit)), &l.cfg.remote);
    if let Err(e) = c.bind(&mut plan, &view) {
        return refuse(&[e]);
    }
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
            &[
                "push",
                "--atomic",
                &format!("--force-with-lease=refs/heads/{branch}:"),
                &view.push_url,
                &format!("{from_commit}:refs/heads/{branch}"),
            ],
        )
        .to(&view.push_url)
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

fn push_tag_steps(c: &Ctx, t: &TagParts, push_url: &str) -> Vec<Step> {
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
            &[
                "push",
                "--atomic",
                &format!("--force-with-lease=refs/tags/{}:", t.name),
                push_url,
                &format!("refs/tags/{}", t.name),
            ],
        )
        .to(push_url)
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
    if let Err(e) = c.bind(&mut plan, &view) {
        return refuse(&[e]);
    }
    let (content, overrides) = c.content_gate(&view, std::slice::from_ref(&t.commit), &mut plan);
    problems.extend(content);
    if !problems.is_empty() {
        return refuse(&problems);
    }
    plan.fact("action", "tag");
    plan.fact("tag", &t.name);
    plan.fact("commit", &t.commit);
    plan.steps = push_tag_steps(c, &t, &view.push_url);
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
    if let Err(e) = c.bind(&mut plan, &view) {
        return refuse(&[e]);
    }
    let (content, overrides) = c.content_gate(&view, std::slice::from_ref(&t.commit), &mut plan);
    problems.extend(content);
    if !problems.is_empty() {
        return refuse(&problems);
    }
    plan.fact("action", "hotfix-finish");
    plan.fact("tag", &t.name);
    plan.fact("commit", &t.commit);
    plan.steps = push_tag_steps(c, &t, &view.push_url);
    let tag_index = plan.steps.len() - 1;
    let pr = match forward_step(c, &view, &t.line_branch, &t.line_tip, &main_tip) {
        Ok(step) => step.expecting(&format!("refs/tags/{}", t.name), Some(&t.commit)),
        Err(e) => return refuse(&[e]),
    };
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

/// The pull request goes to the repository named by the remote's configured URL, on
/// that host, explicitly (`--repo host/owner/name`, `GH_HOST`).
fn forward_step(c: &Ctx, view: &RemoteView, branch: &str, tip: &str, main_tip: &str) -> Result<Step, String> {
    let l = &c.l;
    // The effective URL first (it is where pushes go); the configured URL covers a
    // remote that git rewrites to a local path.
    let Some((host, repo)) = crate::git::forge_repo(&view.fetch_url).or_else(|| crate::git::forge_repo(&view.raw_url))
    else {
        return Err(format!(
            "cannot tell which repository gh should open the pull request in from {}'s URL ({})",
            view.remote,
            crate::util::redact(&view.fetch_url)
        ));
    };
    let title = format!("Merge {branch} forward into {}", l.cfg.main);
    let body = format!(
        "Brings the commits shipped on {branch} into {}, as of {tip}. Release lines merge forward only; {} is never merged into {branch}.\n\nOpened by edition.",
        l.cfg.main, l.cfg.main
    );
    let mut step = Step::plain(
        Kind::Outward,
        &format!("open a pull request merging {branch} into {}", l.cfg.main),
        vec![
            "gh".into(),
            "pr".into(),
            "create".into(),
            "--repo".into(),
            format!("{host}/{repo}"),
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
    .to(&format!("https://{host}/{repo}"));
    step.env.push(("GH_HOST".into(), host.clone()));
    Ok(step)
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
    if let Err(e) = c.bind(&mut plan, &view) {
        return refuse(&[e]);
    }
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
            crate::say!("{}/{name}: merged forward", l.cfg.remote);
            continue;
        }
        crate::say!("{}/{name}: {} commit(s) not in {}:", l.cfg.remote, missing.len(), l.cfg.main);
        for m in &missing {
            crate::say!("    {m}");
        }
        match forward_step(c, &view, name, tip, &main_tip) {
            Ok(step) => plan.steps.push(step),
            Err(e) => return refuse(&[e]),
        }
        let mut e = c.base_entry("open-pr", name, tip);
        e.note = Some(format!("forward merge of {name} into {}", l.cfg.main));
        records.push(Record { index: plan.steps.len() - 1, entry: e });
    }
    if !problems.is_empty() {
        return refuse(&problems);
    }
    if plan.steps.is_empty() {
        crate::say!("Every release line is merged forward.");
        return Ok(0);
    }
    crate::say!();
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
        isolated: true,
        program: std::path::PathBuf::new(),
        dest: None,
    })
}
/// Where a deploy's checks run, and how the plan names that place. When HEAD has the
/// deployed commit's tree, that is this checkout; otherwise a detached checkout of the
/// commit at `.edition/check-tree`, made afresh for every plan so nothing a previous
/// check left there carries over. Its path is fixed, so the dry run and the apply hash
/// to the same plan.
fn check_tree(c: &Ctx, commit: &str) -> Result<(std::path::PathBuf, String), String> {
    let root = &c.l.root;
    let want = c.git.tree(commit).ok_or_else(|| format!("cannot read the tree of {}", short(commit)))?;
    let head = c.git.resolve("HEAD");
    if head.as_deref().and_then(|h| c.git.tree(h)).as_deref() == Some(want.as_str()) {
        let at = head.as_deref().map(short).unwrap_or_default();
        return Ok((root.clone(), format!("this checkout (HEAD {at} has the tree of {})", short(commit))));
    }
    let state = root.join(".edition");
    crate::steps::ensure_private_dir(&state)?;
    let ignore = state.join(".gitignore");
    if std::fs::symlink_metadata(&ignore).is_err() {
        let _ = std::fs::write(&ignore, "*\n");
    }
    let dir = state.join("check-tree");
    let path = dir.to_str().ok_or("the repository path is not UTF-8")?;
    if std::fs::symlink_metadata(&dir).is_ok() {
        c.git.run(&["worktree", "remove", "--force", "--force", path]).map_err(|e| {
            format!("{} is in the way and is not Edition's check checkout ({e}); remove it", dir.display())
        })?;
    }
    let _ = c.git.run(&["worktree", "prune"]);
    c.git.run(&["worktree", "add", "--quiet", "--detach", "--force", path, commit])?;
    let at = head.as_deref().map(short).unwrap_or("no commit");
    Ok((
        dir.clone(),
        format!(
            "a checkout of {} at {}, not this checkout (HEAD {at} has a different tree)",
            short(commit),
            dir.strip_prefix(root).unwrap_or(&dir).display()
        ),
    ))
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
    let mut build =
        c.build.clone().or_else(|| ch.platform.as_deref().and_then(|p| platform_build(l, &c.git, p, &commit)));
    if let Some(b) = &build {
        if !valid_build(b) {
            problems.push(format!("build label {b:?} read from the build file may use only letters, digits and dots"));
        }
    }
    // A library published to a registry has no build number of its own: its version is
    // the build, unless --build or a build file says otherwise.
    if build.is_none() && ch.is_registry() && !dev {
        if let Some(v) = verdict.version.clone().filter(|v| valid_build(v)) {
            verdict.notes.push(format!("build: {v} (a registry channel's build is its version)"));
            build = Some(v);
        }
    }
    if ch.platform.is_some() && l.cfg.concordance.is_some() && !dev && build.is_none() {
        problems.push(format!(
            "channel {channel} ships the {} platform: pass --build <label> (or configure its build_file{})",
            ch.platform.clone().unwrap_or_default(),
            if ch.is_registry() { "; a registry channel uses the version only when it is a final one" } else { "" }
        ));
    }
    // Readiness gate: channels that must already hold this commit, per the ledger.
    if !ch.requires.is_empty() {
        let entries = c.entries(Some(&view))?;
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
        crate::say!("policy: {n}");
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
        ("version", &version),
        ("build", &build),
        ("channel", channel),
        ("target", &ch.target),
    ];
    let title = format!("Deploy {r} ({}) to {channel} [{} tier, {}]", short(&commit), ch.tier.name(), ch.target);
    let mut plan = Plan::new(&title, &l.cfg.remote);
    if !dev {
        if let Err(e) = c.bind(&mut plan, &view) {
            return refuse(&[e]);
        }
    }
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
    // Checks run on the deployed commit's tree: in this checkout when it has that tree,
    // else in a detached checkout of the commit that Edition makes for them.
    let has_command_checks = ch.checks.iter().any(|n| l.cfg.checks.get(n).is_some_and(|ck| ck.kind == "command"));
    let (check_dir, check_place) = if has_command_checks {
        match check_tree(c, &commit) {
            Ok((dir, place)) => {
                plan.fact("checks-tree", &c.git.tree(&commit).unwrap_or_default());
                plan.fact("checks-cwd", &dir.display().to_string());
                (dir, Some(place))
            }
            Err(e) => return refuse(&[format!("channel {channel}: {e}")]),
        }
    } else {
        (l.root.clone(), None)
    };
    let built: Result<Vec<Step>, String> = (|| {
        let mut steps = Vec::new();
        if let Some(place) = &check_place {
            steps.push(Step::note(&format!("the checks run on the tree of {} in {place}", short(&commit))));
        }
        for name in &ch.checks {
            if let Some(ck) = l.cfg.checks.get(name).filter(|ck| ck.kind == "command") {
                let mut s = cmd_step(c, Kind::Check, &format!("check {name}"), &ck.run, &ck.env, &vals)?;
                s.cwd = check_dir.clone();
                steps.push(s);
            }
        }
        let kind = if dev { Kind::Local } else { Kind::Outward };
        let mut run =
            cmd_step(c, kind, &format!("{} {} to {}", ch.kind, short(&commit), ch.target), &ch.run, &ch.env, &vals)?;
        run.dest = Some(ch.target.clone());
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
            steps.push(cmd_step(c, Kind::After, "probe", &ch.probe, ch.probe_env(), &vals)?);
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
        steps_list.push(Step::note(&format!("append an entry to the {} branch", l.cfg.ledger_branch)));
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
    let view = c.view()?;
    let entries = c.entries(Some(&view))?;
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
    crate::say!("promote: {from} holds {} ({}); deploying {r} to {to}\n", held.ref_, short(&held.commit));
    deploy(c, to, &r)
}

// ---------------------------------------------------------------- ledger

pub fn ledger_cmd(c: &Ctx, channel: Option<&str>) -> Res {
    let view = c.view()?;
    let entries = c.entries(Some(&view))?;
    let mut n = 0;
    for e in entries.iter().filter(|e| channel.is_none_or(|ch| e.channel.as_deref() == Some(ch))) {
        n += 1;
        crate::say!(
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
        crate::say!("(no entries on the {} branch)", c.l.cfg.ledger_branch);
    }
    Ok(0)
}

// ---------------------------------------------------------------- hooks

const HOOK: &str = "#!/bin/sh\n# Installed by `edition hooks install`: content, version and pin checks before a push.\nexec edition check-push \"$1\" \"$2\"\n";

pub fn hooks_install(c: &Ctx) -> Res {
    let dir = c.git.hooks_dir()?;
    let dir = c.l.root.join(dir);
    let path = dir.join("pre-push");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if existing == HOOK {
            crate::say!("pre-push hook already installed: {}", path.display());
            return Ok(0);
        }
        return refuse(&[format!(
            "{} exists and was not written by Edition; add `edition check-push \"$1\" \"$2\"` to it by hand",
            path.display()
        )]);
    }
    crate::say!(
        "Install the pre-push hook:\n  [local] write {}\n{}",
        path.display(),
        HOOK.lines().map(|l| format!("      {l}\n")).collect::<String>()
    );
    if !c.mode.apply {
        crate::say!("Dry run: nothing was written. To write it: --apply");
        return Ok(0);
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(&path, HOOK).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    crate::say!("installed");
    Ok(0)
}

// ---------------------------------------------------------------- ledger migrate

/// Moves an in-tree ledger file (from before the ledger branch) to the ledger branch:
/// one commit per entry, published with a push that requires the branch to be absent.
pub fn ledger_migrate(c: &Ctx) -> Res {
    let l = &c.l;
    let file = l.root.join(&l.cfg.ledger);
    let _ = &file;
    let text = crate::safefs::read(&l.root, &l.cfg.ledger)
        .map_err(|e| format!("there is no in-tree ledger to migrate at {} ({e})", l.cfg.ledger))?;
    let format = ledger::format_of(&text).map_err(|e| format!("{}: {e}", l.cfg.ledger))?;
    let entries = ledger::parse(&text).map_err(|e| format!("{}: {e}", l.cfg.ledger))?;
    let checked = match format {
        None => ledger::check_chain_v1(&c.git, &entries),
        Some(_) => ledger::check_chain(&c.git, &entries),
    };
    if let Err((_, m)) = checked {
        return refuse(&[format!("{}: the hash chain is broken ({m}); it cannot be migrated as it is", l.cfg.ledger)]);
    }
    // Format 1 chained entries with git object hashes; format 2 uses SHA-256. Entries
    // keep everything but `prev`, which is re-derived, and a final entry records that.
    let mut entries = entries;
    let mut prev = ledger::GENESIS.to_string();
    for e in entries.iter_mut() {
        e.prev = prev.clone();
        prev = ledger::entry_hash(&c.git, e)?;
    }
    if format.is_none() && !entries.is_empty() {
        let at = now_utc();
        entries.push(Entry {
            id: format!("{at}/migrate/{}", &crate::util::sha256_hex(text.as_bytes())[..10]),
            at,
            by: c.by.clone(),
            action: "migrate".into(),
            product: l.cfg.product.clone(),
            ref_: l.cfg.ledger_branch.clone(),
            commit: c.git.resolve("HEAD").unwrap_or_default(),
            result: "ok".into(),
            prev: prev.clone(),
            note: Some(format!(
                "re-chained {} entries from ledger format 1 (git object hashes) to format 2 (SHA-256); in-tree ledger sha256 {}",
                entries.len(),
                crate::util::sha256_hex(text.as_bytes())
            )),
            ..Entry::default()
        });
    }
    let mut problems = c.input_problems();
    problems.extend(c.clean_problems());
    let view = c.live_view()?;
    let branch = l.cfg.ledger_branch.clone();
    if view.branch(&branch).is_some() {
        problems.push(format!("{} already has a {branch} branch; nothing to migrate onto", view.remote));
    }
    if !problems.is_empty() {
        return refuse(&problems);
    }
    let staging = "refs/edition/ledger-migrate";
    let mut plan = Plan::new(
        &format!("Move {} ledger entries from {} to the {branch} branch", entries.len(), l.cfg.ledger),
        &l.cfg.remote,
    );
    plan.remote = view.fetch_url.clone();
    plan.fact("fetch-url", &view.fetch_url);
    plan.fact("push-url", &view.push_url);
    if let Err(e) = c.bind_config(&mut plan, &view) {
        return refuse(&[e]);
    }
    plan.fact("ledger-sha256", &crate::util::sha256_hex(text.as_bytes()));
    plan.steps = vec![
        Step::note(&format!(
            "build {branch} locally: one commit per entry{}",
            if format.is_none() {
                ", re-chained from format 1 with SHA-256, plus an entry recording the migration"
            } else {
                ""
            }
        )),
        c.git_step(
            Kind::Outward,
            &format!("publish {branch}"),
            &[
                "push",
                "--atomic",
                &format!("--force-with-lease=refs/heads/{branch}:"),
                &view.push_url,
                &format!("{staging}:refs/heads/{branch}"),
            ],
        )
        .expecting(&format!("refs/heads/{branch}"), None)
        .to(&view.push_url),
        Step::note(&format!("then remove {} in an ordinary commit", l.cfg.ledger)),
    ];
    if c.mode.apply {
        c.store().build_history(&entries, staging)?;
    }
    let code = c.run(&plan, Vec::new(), &[])?;
    if c.mode.apply && code == 0 {
        if let Some(tip) = c.git.resolve(staging) {
            c.git.run(&["update-ref", crate::ledger_store::CACHE, &tip])?;
        }
        crate::say!("migrated; remove {} from the tree in an ordinary commit", l.cfg.ledger);
    }
    Ok(code)
}

// ---------------------------------------------------------------- ledger reconcile

/// Publishes outcomes kept in `.edition/pending` without running anything again. With
/// `abandon`, they are published with the result `abandoned`.
pub fn ledger_reconcile(c: &Ctx, abandon: bool) -> Res {
    let l = &c.l;
    let pending = crate::pending::Pending::at(&l.root);
    let runs = pending.list()?;
    if runs.is_empty() {
        crate::say!("Nothing is pending.");
        return Ok(0);
    }
    let mut problems = c.input_problems();
    problems.extend(c.clean_problems());
    let view = c.live_view()?;
    if !problems.is_empty() {
        return refuse(&problems);
    }
    let count: usize = runs.iter().map(|(_, _, e)| e.len()).sum();
    let verb = if abandon { "Record as abandoned" } else { "Publish" };
    let mut plan = Plan::new(&format!("{verb} {count} pending ledger outcome(s)"), &l.cfg.remote);
    if let Err(e) = c.bind_unchecked(&mut plan, &view) {
        return refuse(&[e]);
    }
    plan.fact("abandon", if abandon { "yes" } else { "no" });
    // What will be published, decided now so the scan and its overrides are part of
    // the receipt.
    let mut publish: Vec<(String, Vec<Entry>)> = Vec::new();
    for (key, _, entries) in &runs {
        let mut out = Vec::new();
        for e in entries {
            let mut e = e.clone();
            if abandon {
                e.note = Some(format!("abandoned: the outcome was {} when Edition stopped", e.result));
                e.result = "abandoned".into();
                e.id = format!("{}/abandoned", e.id);
            } else if e.result == "unknown" {
                e.note = Some("Edition stopped before this step finished; its effect is unknown".into());
            }
            out.push(e);
        }
        publish.push((key.clone(), out));
    }
    let all: Vec<Entry> = publish.iter().flat_map(|(_, es)| es.clone()).collect();
    let (allowed, blocked) = match c.ledger_findings(&plan, &all) {
        Ok(x) => x,
        Err(e) => return refuse(&[e]),
    };
    if !blocked.is_empty() {
        return refuse(&blocked);
    }
    let override_text: Vec<String> = allowed.iter().map(|(_, t)| t.clone()).collect();
    for (id, _) in &allowed {
        plan.fact("allowed-ledger-finding", id);
        plan.allowed_findings.push(id.clone());
    }
    for (key, text, entries) in &runs {
        plan.fact("pending", &format!("{key} {}", crate::util::sha256_hex(text.as_bytes())));
        for e in entries {
            plan.steps.push(Step::note(&format!(
                "{} {} ({}, {})",
                if abandon { "abandon" } else { "publish" },
                e.id,
                e.channel.as_deref().unwrap_or(&e.action),
                e.result
            )));
        }
    }
    let digest = c.config_digest()?;
    let used = c.receipt_used();
    plan.run_path = l.run_path();
    plan.tool_env = c.git.env.clone();
    let mut no_steps = |_: &[steps::Outcome]| -> Result<(), String> { Ok(()) };
    match steps::execute(&plan, &c.mode, &c.git, &l.root, &digest, l.receipt_ttl(), &used, &mut no_steps)? {
        Ran::DryRun => Ok(0),
        Ran::Ran { .. } => {
            for (key, entries) in &publish {
                for e in entries {
                    let mut e = e.clone();
                    for t in &override_text {
                        if !e.overrides.contains(t) {
                            e.overrides.push(t.clone());
                        }
                    }
                    c.record(&e, &plan)?;
                }
                pending.remove(key)?;
            }
            Ok(0)
        }
    }
}
