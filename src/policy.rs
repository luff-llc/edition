//! What a ref is (tags, lines, remote reachability) and whether a channel's tier allows it.

use crate::config::{Loaded, Tier};
use crate::git::Git;
use crate::version::{FilePattern, Template, Version};

/// What is known about one remote, offline from remote-tracking refs, or online from
/// `git ls-remote` (read-only).
#[derive(Clone, Debug)]
pub struct RemoteView {
    pub remote: String,
    pub branches: Vec<(String, String)>,
    /// Tags known to be on the remote (online only).
    pub remote_tags: Option<Vec<(String, String)>>,
    pub local_tags: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reach {
    Branch(String),
    Tag(String),
    /// Under a local tag that may or may not be on the remote (offline view).
    LocalTagOnly(String),
    Nowhere,
}

impl RemoteView {
    /// Online, heads and tags are the remote's live values from `git ls-remote`;
    /// offline, heads come from remote-tracking refs and are unverified.
    pub fn build(git: &Git, remote: &str, online: bool) -> Result<RemoteView, String> {
        let (branches, remote_tags) = if online {
            let (heads, tags) = git.ls_remote(remote)?;
            (heads, Some(tags))
        } else {
            (git.remote_branches(remote), None)
        };
        Ok(RemoteView { remote: remote.to_string(), branches, remote_tags, local_tags: git.tags() })
    }

    /// Every commit the remote is known to have at a ref tip.
    pub fn tips(&self) -> Vec<String> {
        let mut v: Vec<String> = self.branches.iter().map(|(_, c)| c.clone()).collect();
        v.extend(self.remote_tags.iter().flatten().map(|(_, c)| c.clone()));
        v
    }

    pub fn branch(&self, name: &str) -> Option<&str> {
        self.branches.iter().find(|(n, _)| n == name).map(|(_, c)| c.as_str())
    }

    pub fn online(&self) -> bool {
        self.remote_tags.is_some()
    }

    /// `Some(true)` on the remote, `Some(false)` not on it, `None` unknown (offline).
    pub fn tag_on_remote(&self, name: &str, commit: &str) -> Option<bool> {
        self.remote_tags.as_ref().map(|t| t.iter().any(|(n, c)| n == name && c == commit))
    }

    pub fn reach(&self, git: &Git, commit: &str) -> Reach {
        for (name, tip) in &self.branches {
            if git.is_ancestor(commit, tip) {
                return Reach::Branch(format!("{}/{name}", self.remote));
            }
        }
        if let Some(tags) = &self.remote_tags {
            for (name, c) in tags {
                if git.resolve(c).is_some() && git.is_ancestor(commit, c) {
                    return Reach::Tag(name.clone());
                }
            }
            return Reach::Nowhere;
        }
        for (name, c) in &self.local_tags {
            if git.is_ancestor(commit, c) {
                return Reach::LocalTagOnly(name.clone());
            }
        }
        Reach::Nowhere
    }
}

/// Tags that point exactly at `commit`, classified by the configured templates.
#[derive(Clone, Debug, Default)]
pub struct TagsAt {
    pub finals: Vec<(String, Version)>,
    pub pres: Vec<(String, Version, u32)>,
}

pub fn classify_tag(final_t: &Template, pre_t: &Template, name: &str) -> Option<(Version, Option<u32>)> {
    if let Some(caps) = pre_t.matches(name) {
        let v = Template::get(&caps, "version").and_then(|v| Version::parse(v).ok());
        let n = Template::get(&caps, "n").and_then(|n| n.parse::<u32>().ok());
        if let (Some(v), Some(n)) = (v, n) {
            if v.is_final() {
                return Some((v, Some(n)));
            }
        }
    }
    let caps = final_t.matches(name)?;
    let v = Version::parse(Template::get(&caps, "version")?).ok()?;
    v.is_final().then_some((v, None))
}

pub fn tags_at(l: &Loaded, tags: &[(String, String)], commit: &str) -> TagsAt {
    let mut out = TagsAt::default();
    for (name, _) in tags.iter().filter(|(_, c)| c == commit) {
        match classify_tag(&l.final_tag, &l.pre_tag, name) {
            Some((v, None)) => out.finals.push((name.clone(), v)),
            Some((v, Some(n))) => out.pres.push((name.clone(), v, n)),
            None => {}
        }
    }
    out.finals.sort_by(|a, b| b.1.cmp(&a.1));
    out.pres.sort_by(|a, b| (&b.1, b.2).cmp(&(&a.1, a.2)));
    out
}

/// The value of every version file at a revision (`None` = working tree).
pub fn version_values(l: &Loaded, git: &Git, rev: Option<&str>) -> Vec<(String, Result<Vec<String>, String>)> {
    let mut out = Vec::new();
    for vf in &l.cfg.version_files {
        let text = match rev {
            Some(r) => git.show(r, &vf.path).ok_or_else(|| format!("missing at {r}")),
            None => std::fs::read_to_string(l.root.join(&vf.path)).map_err(|e| e.to_string()),
        };
        let res = text.and_then(|t| {
            let p = FilePattern::parse(&vf.pattern, &["version"])?;
            let found: Vec<String> = p.find_all(&t, vf.after.as_deref()).into_iter().map(|(_, v)| v).collect();
            if found.is_empty() {
                Err(format!("pattern `{}` not found", vf.pattern))
            } else {
                Ok(found)
            }
        });
        let label = match &vf.after {
            Some(a) => format!("{} (after {a})", vf.path),
            None => vf.path.clone(),
        };
        out.push((label, res));
    }
    out
}

/// The single version all version files agree on, or the disagreement.
pub fn agreed_version(l: &Loaded, git: &Git, rev: Option<&str>) -> Result<Option<String>, String> {
    let vals = version_values(l, git, rev);
    if vals.is_empty() {
        return Ok(None);
    }
    let mut seen: Vec<String> = Vec::new();
    let mut problems = Vec::new();
    for (label, r) in &vals {
        match r {
            Ok(vs) => {
                for v in vs {
                    if !seen.contains(v) {
                        seen.push(v.clone());
                    }
                }
            }
            Err(e) => problems.push(format!("{label}: {e}")),
        }
    }
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    if seen.len() == 1 {
        Ok(seen.pop())
    } else {
        let detail: Vec<String> = vals
            .iter()
            .filter_map(|(label, r)| r.as_ref().ok().map(|vs| format!("{label}={}", vs.join(","))))
            .collect();
        Err(format!("version files disagree: {}", detail.join(", ")))
    }
}

/// One pin as read from a pin file.
#[derive(Clone, Debug)]
pub struct PinState {
    pub name: String,
    pub repo: String,
    pub value: Option<String>,
    pub commit: Option<String>,
    pub reach: Option<Reach>,
    /// The final release tag that points exactly at the pinned commit.
    pub release_tag: Option<String>,
    /// For a release tag, whether the remote has it (None when offline).
    pub tag_on_remote: Option<bool>,
    pub problem: Option<String>,
}

impl PinState {
    /// The prod-tier rule: a final release tag, reachable on the remote.
    pub fn prod_violation(&self) -> Option<String> {
        if let Some(p) = &self.problem {
            return Some(format!("pin {}: {p}", self.name));
        }
        let commit = self.commit.as_deref().unwrap_or("?");
        match (&self.release_tag, &self.reach) {
            (_, Some(Reach::Nowhere)) => Some(format!(
                "pin {}: {} is on no {} branch or tag (it exists only in a local checkout)",
                self.name,
                crate::util::short(commit),
                self.repo
            )),
            (None, _) => Some(format!(
                "pin {}: {} is not a release tag commit in {}; production needs consumers pinned to a final tag",
                self.name,
                crate::util::short(commit),
                self.repo
            )),
            (Some(t), _) if self.tag_on_remote == Some(false) => {
                Some(format!("pin {}: release tag {t} is not on the {} remote", self.name, self.repo))
            }
            _ => None,
        }
    }
}

/// Reads every pin (at `rev`, or the working tree) and evaluates it in its repository.
pub fn pins(l: &Loaded, git: &Git, rev: Option<&str>, online: bool) -> Vec<PinState> {
    let mut out = Vec::new();
    for p in &l.cfg.pins {
        let mut st = PinState {
            name: p.name.clone(),
            repo: p.repo.clone(),
            value: None,
            commit: None,
            reach: None,
            release_tag: None,
            tag_on_remote: None,
            problem: None,
        };
        let text = match rev {
            Some(r) => git.show(r, &p.file),
            None => std::fs::read_to_string(l.root.join(&p.file)).ok(),
        };
        let Some(text) = text else {
            st.problem = Some(format!("{} is missing", p.file));
            out.push(st);
            continue;
        };
        let pat = match FilePattern::parse(&p.pattern, &["commit", "tag"]) {
            Ok(pat) => pat,
            Err(e) => {
                st.problem = Some(e);
                out.push(st);
                continue;
            }
        };
        let found = pat.find_all(&text, None);
        let Some((_, value)) = found.first() else {
            st.problem = Some(format!("pattern `{}` not found in {}", p.pattern, p.file));
            out.push(st);
            continue;
        };
        st.value = Some(value.clone());
        let Some((path, remote, final_t)) = l.repo_path(&p.repo) else {
            st.problem = Some(format!("repo {} is not configured", p.repo));
            out.push(st);
            continue;
        };
        let rg = Git::new(&path);
        if !rg.is_repo() {
            st.problem = Some(format!("{} is not a git checkout (repo {})", path.display(), p.repo));
            out.push(st);
            continue;
        }
        let Some(commit) = rg.resolve(value) else {
            st.problem = Some(format!("{value} is not a commit in {}", path.display()));
            out.push(st);
            continue;
        };
        let view = match RemoteView::build(&rg, &remote, online) {
            Ok(v) => v,
            Err(e) => {
                st.problem = Some(e);
                out.push(st);
                continue;
            }
        };
        st.reach = Some(view.reach(&rg, &commit));
        let tags = view.remote_tags.clone().unwrap_or_else(|| view.local_tags.clone());
        st.release_tag = tags
            .iter()
            .filter(|(n, c)| {
                c == &commit
                    && final_t.matches(n).is_some_and(|caps| {
                        Template::get(&caps, "version")
                            .and_then(|v| Version::parse(v).ok())
                            .is_some_and(|v| v.is_final())
                    })
            })
            .map(|(n, _)| n.clone())
            .next();
        if let Some(t) = &st.release_tag {
            st.tag_on_remote = view.tag_on_remote(t, &commit);
        }
        st.commit = Some(commit);
        out.push(st);
    }
    out
}

/// The verdict on putting `commit` on a channel of `tier`.
#[derive(Clone, Debug, Default)]
pub struct Verdict {
    pub violations: Vec<String>,
    pub notes: Vec<String>,
    /// The release tag the deploy ships under (prod).
    pub tag: Option<String>,
    pub version: Option<String>,
}

impl Verdict {
    pub fn allowed(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Tier policy. `input` is the ref as the user named it.
pub fn check_tier(
    l: &Loaded,
    git: &Git,
    view: &RemoteView,
    tier: Tier,
    allow_prerelease: bool,
    input: &str,
    commit: &str,
) -> Verdict {
    let mut v = Verdict::default();
    match tier {
        Tier::Dev => {
            v.notes.push("dev tier: any ref, this machine only, not recorded in the ledger".into());
            v.version = agreed_version(l, git, Some(commit)).ok().flatten();
        }
        Tier::Staging => {
            let main = &l.cfg.main;
            match view.branch(main) {
                None => v.violations.push(format!("{}/{main} is not known here (fetch first)", view.remote)),
                Some(tip) if tip == commit => {
                    v.notes.push(format!(
                        "staging tier: {} is the tip of {}/{main}",
                        crate::util::short(commit),
                        view.remote
                    ));
                }
                Some(tip) => v.violations.push(format!(
                    "staging takes only the tip of {}/{main} ({}); {input} is {}{}",
                    view.remote,
                    crate::util::short(tip),
                    crate::util::short(commit),
                    if git.is_ancestor(commit, tip) { ", an older main commit" } else { ", which is not on main" }
                )),
            }
            v.version = agreed_version(l, git, Some(commit)).ok().flatten();
        }
        Tier::Prod => {
            // Classify against local and remote tags; presence on the remote is checked below.
            let mut tags = view.local_tags.clone();
            for t in view.remote_tags.iter().flatten() {
                if !tags.contains(t) {
                    tags.push(t.clone());
                }
            }
            let at = tags_at(l, &tags, commit);
            // Production names its tag explicitly: `refs/tags/X`, or a name that is a tag
            // and not also a branch. A branch or a bare commit is refused even when a
            // release tag points at it.
            let name = input.strip_prefix("refs/tags/").unwrap_or(input);
            let explicit = input.starts_with("refs/tags/")
                || (git.has_ref(&format!("refs/tags/{input}"))
                    && !git.has_ref(&format!("refs/heads/{input}"))
                    && !git.has_ref(&format!("refs/remotes/{}/{input}", view.remote))
                    && !view.branches.iter().any(|(b, _)| b == input));
            let named = classify_tag(&l.final_tag, &l.pre_tag, name)
                .filter(|_| at.finals.iter().any(|(n, _)| n == name) || at.pres.iter().any(|(n, _, _)| n == name));
            let chosen: Option<(String, Version, bool)> = match (explicit, named) {
                (true, Some((ver, None))) => Some((name.to_string(), ver, false)),
                (true, Some((ver, Some(_)))) => Some((name.to_string(), ver, true)),
                (false, _) => {
                    v.violations.push(format!(
                        "prod takes a release tag by name (refs/tags/<tag>, or a tag name that is not also a branch); `{input}` is not one"
                    ));
                    return v;
                }
                (true, None) => None,
            };
            match chosen {
                None => v.violations.push(format!(
                    "prod takes only a release tag ({}); no release tag points at {}",
                    l.final_tag.source(),
                    crate::util::short(commit)
                )),
                Some((name, _, true)) if !allow_prerelease => v.violations.push(format!(
                    "{name} is a pre-release tag; this channel takes only final tags (set allow_prerelease for store review channels, never for homes)"
                )),
                Some((name, ver, pre)) => {
                    match view.tag_on_remote(&name, commit) {
                        Some(true) => v.notes.push(format!("{name} is on {}", view.remote)),
                        Some(false) => v.violations.push(format!("{name} is not on {} (push the tag first)", view.remote)),
                        None => v.notes.push(format!("{name}: presence on {} not checked offline (pass --online)", view.remote)),
                    }
                    let line = l.line_branch_name(&ver.line());
                    match view.branch(&line) {
                        Some(tip) if git.is_ancestor(commit, tip) => v.notes.push(format!("{name} is on {}/{line}", view.remote)),
                        Some(_) => v.violations.push(format!("{name} is not on {}/{line}", view.remote)),
                        None => v.violations.push(format!("release line {}/{line} does not exist", view.remote)),
                    }
                    if pre {
                        v.notes.push(format!("{name} is a pre-release; allowed here by allow_prerelease"));
                    }
                    v.version = Some(ver.to_string());
                    v.tag = Some(name);
                }
            }
            for p in pins(l, git, Some(commit), view.online()) {
                if let Some(problem) = p.prod_violation() {
                    v.violations.push(problem);
                }
            }
        }
    }
    v
}
