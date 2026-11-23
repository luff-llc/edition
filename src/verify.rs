//! `edition verify`: the invariants. Each check reports ok, a warning or an error;
//! any error makes the command exit 1.

use crate::config::{Loaded, Tier};
use crate::git::Git;
use crate::ledger;
use crate::policy::{self, Reach, RemoteView};
use crate::util::short;
use crate::version::Version;

#[derive(Default, Debug)]
pub struct Report {
    pub ok: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl Report {
    pub fn print(&self) {
        for s in &self.ok {
            crate::say!("ok    {s}");
        }
        for s in &self.warnings {
            crate::say!("warn  {s}");
        }
        for s in &self.errors {
            crate::say!("ERROR {s}");
        }
        crate::say!("\n{} ok, {} warning(s), {} error(s)", self.ok.len(), self.warnings.len(), self.errors.len());
    }
}

pub fn run(l: &Loaded, online: bool) -> Report {
    let mut r = Report::default();
    let git = Git::with_env(&l.root, &l.run_path(), &l.cfg.run.tool_env);
    if !git.is_repo() {
        r.errors.push(format!("{} is not a git repository", l.root.display()));
        return r;
    }
    for t in &l.todos {
        r.warnings.push(format!("unanswered {t}"));
    }
    let view = match RemoteView::build(&git, &l.cfg.remote, online) {
        Ok(v) => v,
        Err(e) => {
            r.errors.push(format!("cannot read remote {}: {e}", l.cfg.remote));
            return r;
        }
    };
    if !online {
        if let Err(e) = crate::gitcfg::audit(&git) {
            r.warnings.push(format!("{e} (offline, so nothing contacted it)"));
        }
        r.warnings.push(format!(
            "offline: tags are not checked against {} (pass --online to read it with git ls-remote)",
            l.cfg.remote
        ));
    }
    main_version(l, &git, &view, &mut r);
    lines(l, &git, &view, &mut r);
    tag_versions(l, &git, &view, &mut r);
    pins(l, &git, online, &mut r);
    ledger_checks(l, &git, &view, &mut r);
    crate::content::verify(l, &git, &mut r);
    r
}

fn release_lines(l: &Loaded, view: &RemoteView) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for (name, tip) in &view.branches {
        if let Some(caps) = l.line_branch.matches(name) {
            if let Some(line) = crate::version::Template::get(&caps, "line") {
                if crate::version::parse_line(line).is_ok() {
                    out.push((line.to_string(), name.clone(), tip.clone()));
                }
            }
        }
    }
    out.sort_by_key(|(line, _, _)| crate::version::parse_line(line).unwrap_or((0, 0)));
    out
}

fn main_version(l: &Loaded, git: &Git, view: &RemoteView, r: &mut Report) {
    if l.cfg.version_files.is_empty() {
        r.warnings.push("no version_files configured; tag/version agreement is not checked".into());
        return;
    }
    let main_ref = format!("{}/{}", l.cfg.remote, l.cfg.main);
    let Some(tip) = view.branch(&l.cfg.main) else {
        r.errors.push(format!("{main_ref} is not known (fetch first)"));
        return;
    };
    match policy::agreed_version(l, git, Some(tip)) {
        Err(e) => r.errors.push(format!("{main_ref}: {e}")),
        Ok(None) => {}
        Ok(Some(v)) => {
            let parsed = Version::parse(&v);
            let dev = &l.cfg.lines.dev_suffix;
            let newest_line = release_lines(l, view).last().map(|(line, _, _)| line.clone());
            match parsed {
                Err(e) => r.errors.push(format!("{main_ref}: {e}")),
                Ok(p) if p.pre.as_deref() != Some(dev.as_str()) => r.errors.push(format!(
                    "{main_ref} builds as {v}; main should build as the next version with -{dev} (for example {}.{}.0-{dev})",
                    p.major,
                    p.minor + 1
                )),
                Ok(p) => {
                    let ahead = newest_line
                        .as_deref()
                        .and_then(|nl| crate::version::parse_line(nl).ok())
                        .is_none_or(|(ma, mi)| (p.major, p.minor) > (ma, mi));
                    if ahead {
                        r.ok.push(format!("{main_ref} builds as {v}"));
                    } else {
                        r.errors.push(format!(
                            "{main_ref} builds as {v}, which is not ahead of the newest release line {}",
                            newest_line.unwrap_or_default()
                        ));
                    }
                }
            }
        }
    }
}

fn lines(l: &Loaded, git: &Git, view: &RemoteView, r: &mut Report) {
    let Some(main_tip) = view.branch(&l.cfg.main).map(str::to_string) else { return };
    for (line, name, tip) in release_lines(l, view) {
        let range = format!("{main_tip}..{tip}");
        let missing = git.log_range(&range);
        if missing.is_empty() {
            r.ok.push(format!("{}/{name} is merged forward into {}", view.remote, l.cfg.main));
        } else {
            r.errors.push(format!(
                "{}/{name} has {} commit(s) not merged forward into {} (run `edition forward`): {}",
                view.remote,
                missing.len(),
                l.cfg.main,
                missing.iter().take(8).cloned().collect::<Vec<_>>().join("; ")
            ));
        }
        if !l.cfg.version_files.is_empty() {
            match policy::agreed_version(l, git, Some(&tip)) {
                Ok(Some(v)) => match Version::parse(&v) {
                    Ok(p) if p.line() == line && p.is_final() => {
                        r.ok.push(format!("{}/{name} builds as {v}", view.remote))
                    }
                    _ => r.errors.push(format!(
                        "{}/{name} builds as {v}; a release line carries a final {line}.Z version",
                        view.remote
                    )),
                },
                Ok(None) => {}
                Err(e) => r.errors.push(format!("{}/{name}: {e}", view.remote)),
            }
        }
    }
}

fn tag_versions(l: &Loaded, git: &Git, view: &RemoteView, r: &mut Report) {
    if l.cfg.version_files.is_empty() {
        return;
    }
    let since = l.cfg.verify.tags_since.as_deref().and_then(|s| Version::parse(s).ok());
    let tags = view.remote_tags.clone().unwrap_or_else(|| view.local_tags.clone());
    let mut checked = 0;
    for (name, commit) in &tags {
        let Some((ver, n)) = policy::classify_tag(&l.final_tag, &l.pre_tag, name) else { continue };
        if since.as_ref().is_some_and(|s| ver < *s) {
            continue;
        }
        if git.resolve(commit).is_none() {
            r.warnings.push(format!("tag {name} is on the remote but its commit is not fetched here"));
            continue;
        }
        let want = match n {
            Some(n) if l.cfg.tags.prerelease_files == "full" => format!("{ver}-rc.{n}"),
            _ => ver.to_string(),
        };
        checked += 1;
        match policy::agreed_version(l, git, Some(commit)) {
            Ok(Some(v)) if v == want => {}
            Ok(Some(v)) => r.errors.push(format!("tag {name} carries version {v}, expected {want}")),
            Ok(None) => {}
            Err(e) => r.errors.push(format!("tag {name}: {e}")),
        }
        let line = l.line_branch_name(&ver.line());
        if let Some(tip) = view.branch(&line) {
            if !git.is_ancestor(commit, tip) {
                r.warnings.push(format!("tag {name} is not on {}/{line}", view.remote));
            }
        }
    }
    if checked > 0 && r.errors.iter().all(|e| !e.starts_with("tag ")) {
        r.ok.push(format!("{checked} release tag(s) agree with the version files"));
    }
}

fn pins(l: &Loaded, git: &Git, online: bool, r: &mut Report) {
    for p in policy::pins(l, git, None, online) {
        match p.prod_violation() {
            Some(v) => r.errors.push(v),
            None => {
                let tag = p.release_tag.clone().unwrap_or_default();
                let where_ = match &p.reach {
                    Some(Reach::Branch(b)) => format!("on {b}"),
                    Some(Reach::Tag(t)) => format!("under {t}"),
                    Some(Reach::LocalTagOnly(t)) => format!("under local tag {t}"),
                    _ => String::new(),
                };
                let msg = format!("pin {} is release tag {tag} ({where_})", p.name);
                if p.tag_on_remote.is_none() {
                    r.warnings.push(format!("{msg}; tag presence on the remote not checked offline"));
                } else {
                    r.ok.push(msg);
                }
            }
        }
    }
}

fn ledger_checks(l: &Loaded, git: &Git, view: &RemoteView, r: &mut Report) {
    let store = crate::ledger_store::Store::new(git, &l.cfg.ledger_branch);
    let branch = l.cfg.ledger_branch.clone();
    match crate::pending::Pending::at(&l.root).list() {
        Ok(runs) => {
            for (_, _, entries) in runs {
                for e in entries {
                    r.errors.push(format!(
                        "UNRECONCILED outcome {} ({}, {}) is not on the {branch} branch; run `edition ledger reconcile`",
                        e.id,
                        e.channel.as_deref().unwrap_or(&e.action),
                        e.result
                    ));
                }
            }
        }
        Err(e) => r.errors.push(e),
    }
    if l.root.join(&l.cfg.ledger).is_file() {
        r.warnings.push(format!(
            "an in-tree ledger exists at {}; move it to the {branch} branch with `edition ledger migrate`",
            l.cfg.ledger
        ));
    }
    // The remote anchors the ledger: its branch must descend from everything seen
    // before. Whoever can rewrite that branch on the remote can rewrite the ledger.
    if view.online() {
        match store.sync(&view.fetch_url) {
            Ok(Some(tip)) => {
                r.ok.push(format!("{branch} on {} is at {} and extends what was seen", view.remote, short(&tip)))
            }
            Ok(None) => r.ok.push(format!("{branch}: not on {} yet; nothing recorded", view.remote)),
            Err(e) => {
                r.errors.push(e);
                return;
            }
        }
    } else {
        r.warnings.push(format!("{branch}: the remote anchor is unverified offline (pass --online)"));
    }
    let entries = match store.load_local() {
        Ok(e) => e,
        Err(e) => {
            r.errors.push(e);
            return;
        }
    };
    match ledger::check_chain(git, &entries) {
        Ok(()) if !entries.is_empty() => r.ok.push(format!("{branch}: hash chain intact ({} entries)", entries.len())),
        Ok(()) => {}
        Err((_, m)) => r.errors.push(format!("{branch}: hash chain broken: {m}")),
    }
    // The whole branch, commit by commit: each one an exact one-entry extension.
    if let Some(tip) = store.cached() {
        match store.validate_new(None, &tip) {
            Ok(()) => r.ok.push(format!("{branch} is append-only, one valid entry per commit")),
            Err(e) => r.errors.push(e),
        }
    }
    let mut ids: Vec<&str> = Vec::new();
    for e in &entries {
        if ids.contains(&e.id.as_str()) {
            r.errors.push(format!("ledger id {} appears twice", e.id));
        }
        ids.push(&e.id);
        if e.product != l.cfg.product && e.action == "deploy" {
            r.warnings.push(format!("ledger {}: product {} is not {}", e.id, e.product, l.cfg.product));
        }
        if e.result != "ok" {
            continue;
        }
        if git.resolve(&e.commit).is_none() {
            r.errors.push(format!("ledger {}: commit {} is not in this repository", e.id, short(&e.commit)));
            continue;
        }
        match view.reach(git, &e.commit) {
            Reach::Nowhere => {
                r.errors.push(format!("ledger {}: {} is on no {} branch or tag", e.id, short(&e.commit), view.remote))
            }
            Reach::LocalTagOnly(t) => r.warnings.push(format!(
                "ledger {}: {} is only under tag {t}, not confirmed on {} offline",
                e.id,
                short(&e.commit),
                view.remote
            )),
            _ => {}
        }
        if e.action != "deploy" {
            continue;
        }
        let Some(ch_name) = &e.channel else { continue };
        let Some(ch) = l.cfg.channels.get(ch_name) else {
            r.warnings.push(format!("ledger {}: channel {ch_name} is no longer configured", e.id));
            continue;
        };
        match ch.tier {
            Tier::Dev => r.errors.push(format!("ledger {}: dev-tier channel {ch_name} must never be recorded", e.id)),
            Tier::Staging => {
                let main_tip = view.branch(&l.cfg.main).unwrap_or_default();
                if !git.is_ancestor(&e.commit, main_tip) {
                    r.errors.push(format!(
                        "ledger {}: staging channel {ch_name} got {}, which is not on {}/{}",
                        e.id,
                        short(&e.commit),
                        view.remote,
                        l.cfg.main
                    ));
                }
            }
            Tier::Prod => {
                let tags = view.remote_tags.clone().unwrap_or_else(|| view.local_tags.clone());
                let at = policy::tags_at(l, &tags, &e.commit);
                let is_final = at.finals.iter().any(|(n, _)| *n == e.ref_);
                let is_pre = at.pres.iter().any(|(n, _, _)| *n == e.ref_);
                if !(is_final || (is_pre && ch.allow_prerelease)) {
                    r.errors.push(format!(
                        "ledger {}: prod channel {ch_name} holds {} ({}), which is not a release tag at that commit{}",
                        e.id,
                        e.ref_,
                        short(&e.commit),
                        if view.online() { " on the remote" } else { "" }
                    ));
                }
            }
        }
    }
    let current = ledger::current_by_channel(&entries);
    for (ch, e) in &current {
        r.ok.push(format!("{ch} holds {} ({}) since {}", e.ref_, short(&e.commit), e.at));
    }
    concordance_checks(l, &entries, r);
}

fn concordance_checks(l: &Loaded, entries: &[ledger::Entry], r: &mut Report) {
    if l.cfg.concordance.is_none() {
        return;
    }
    let recorded: Vec<&ledger::Entry> = entries
        .iter()
        .filter(|e| e.action == "deploy" && e.result == "ok" && e.build.is_some())
        .filter(|e| e.channel.as_ref().and_then(|c| l.cfg.channels.get(c)).is_some_and(|c| c.platform.is_some()))
        .collect();
    let releases = match crate::concordance::releases(l) {
        Ok(rs) => rs,
        Err(e) => {
            r.warnings.push(format!("Concordance not checked: {e}"));
            return;
        }
    };
    r.ok.push(format!("Concordance tracker read ({} release(s) listed)", releases.len()));
    // The latest build per (release, platform) in the ledger must match Concordance.
    let mut latest: Vec<(String, String, String, String)> = Vec::new();
    for e in recorded {
        let ch = e.channel.as_ref().and_then(|c| l.cfg.channels.get(c));
        let Some(platform) = ch.and_then(|c| c.platform.clone()) else { continue };
        let platform = l.cfg.platforms.get(&platform).and_then(|p| p.concordance.clone()).unwrap_or(platform);
        let Some(version) = e.version.as_deref() else { continue };
        let base = Version::parse(version).map(|v| v.base().to_string()).unwrap_or_else(|_| version.to_string());
        let Some(rid) = crate::concordance::release_id(l, &base) else { continue };
        let build = e.build.clone().unwrap_or_default();
        latest.retain(|(r0, p0, _, _)| !(r0 == &rid && p0 == &platform));
        latest.push((rid, platform, build, e.id.clone()));
    }
    for (rid, platform, build, id) in latest {
        match releases.iter().find(|x| x.id == rid) {
            None => r.warnings.push(format!(
                "ledger {id}: release {rid} is not among Concordance's listed releases (add it to tracker/releases.yaml)"
            )),
            Some(rel) => match rel.builds.get(&platform) {
                Some(b) if *b == build => r.ok.push(format!("Concordance {rid} {platform} build {b} matches the ledger")),
                other => r.errors.push(format!(
                    "Concordance {rid} says {platform} build {}, the ledger says {build} (record it: concordance log uploaded {rid} --platform {platform} --build {build})",
                    other.map(String::as_str).unwrap_or("none")
                )),
            },
        }
    }
}
