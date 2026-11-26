//! In-flight work for a release line: what is still heading for `X.Y` while `X.Y.Z` is
//! being released. A release cut while more work for its line is in flight is replaced
//! right away, and every consumer tests and deploys it twice; `rc`, `tag` and
//! `hotfix finish` refuse until each item is folded in or explicitly allowed.
//!
//! Two sources:
//! - **The forge**: open pull requests into the line's branch, and open pull requests
//!   from the line's hotfix branches, read with `gh pr list` against the repository the
//!   remote's URL names. Read failures fail closed.
//! - **The line itself**, at its tip: a changelog section headed "Unreleased" that has
//!   entries, and a changelog heading or version files naming a later patch of the line
//!   while the version being released is still untagged ("fold into X.Y.Z").

use crate::config::Loaded;
use crate::git::Git;
use crate::policy::RemoteView;
use crate::version::{Template, Version};

/// The most open pull requests read in one listing. A repository with more is refused
/// rather than checked partially.
pub const PR_LIMIT: usize = 500;

/// One piece of in-flight work. `id` is what `--allow-in-flight` names: `#12` for a pull
/// request (`12` is accepted too), `changelog:Unreleased`, `changelog:0.5.8`,
/// `version:0.5.8`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub what: String,
}

impl Item {
    /// True when `given` (an `--allow-in-flight` value) names this item.
    pub fn allowed_by(&self, given: &str) -> bool {
        let given = given.trim();
        given == self.id || self.id.strip_prefix('#').is_some_and(|n| n == given)
    }
}

/// Pull requests heading for a line, or why they were not read.
pub enum Prs {
    Read(Vec<Item>),
    /// The remote's URL names no forge repository, so there is nowhere a pull request
    /// could be open.
    NoForge,
}

/// Items found on the line at `tip`: the changelog and the version files.
pub fn on_line(l: &Loaded, git: &Git, tip: &str, version: &Version, released: bool) -> Vec<Item> {
    let mut items = Vec::new();
    let path = &l.cfg.changelog;
    if let Some(text) = git.show(tip, path) {
        if unreleased_has_entries(&text) {
            items.push(Item {
                id: "changelog:Unreleased".into(),
                what: format!("{path} has entries under \"Unreleased\" on the {} line", version.line()),
            });
        }
        if !released {
            for later in later_patches(&text, version) {
                items.push(Item {
                    id: format!("changelog:{later}"),
                    what: format!("{path} has an entry for {later} while {version} is untagged (fold into {version})"),
                });
            }
        }
    }
    if !released {
        if let Ok(Some(v)) = crate::policy::agreed_version(l, git, Some(tip)) {
            if let Ok(p) = Version::parse(&v) {
                if p.is_final() && p.line() == version.line() && p.patch > version.patch {
                    items.push(Item {
                        id: format!("version:{p}"),
                        what: format!(
                            "the version files on the line say {p} while {version} is untagged (fold into {version})"
                        ),
                    });
                }
            }
        }
    }
    items
}

/// True when a heading whose text starts with "Unreleased" (any level, with or without
/// brackets) is followed by at least one non-blank line that is not itself a heading,
/// before the next heading at the same or a higher level.
pub fn unreleased_has_entries(text: &str) -> bool {
    let mut open: Option<usize> = None;
    for line in text.lines() {
        if let Some((level, title)) = heading(line) {
            if let Some(l) = open {
                if level <= l {
                    open = None;
                }
            }
            if open.is_none() && title.to_ascii_lowercase().starts_with("unreleased") {
                open = Some(level);
            }
            continue;
        }
        if open.is_some() && !line.trim().is_empty() {
            return true;
        }
    }
    false
}

/// Versions of `version`'s line, newer than it, that a changelog heading names.
pub fn later_patches(text: &str, version: &Version) -> Vec<Version> {
    let mut out: Vec<Version> = Vec::new();
    for line in text.lines() {
        let Some((_, title)) = heading(line) else { continue };
        let Some(v) = first_version(title) else { continue };
        if v.is_final() && v.line() == version.line() && v.patch > version.patch && !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// `(level, text)` of a Markdown ATX heading.
fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.bytes().take_while(|b| *b == b'#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &line[level..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some((level, rest.trim().trim_start_matches('[')))
}

/// The first `X.Y.Z` (optionally `vX.Y.Z`) at the start of a heading's text.
fn first_version(title: &str) -> Option<Version> {
    let t = title.strip_prefix('v').unwrap_or(title);
    let end = t.find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-')).unwrap_or(t.len());
    Version::parse(&t[..end]).ok()
}

/// True when `head` is a hotfix branch of `line`: the configured template with a
/// version on the line, or the template rendered with the line itself followed by the
/// end of the name or a separator (`hotfix/0.5`, `hotfix/0.5-ledger`).
pub fn is_line_hotfix(template: &Template, head: &str, line: &str) -> bool {
    if let Some(caps) = template.matches(head) {
        if let Some(v) = Template::get(&caps, "version").and_then(|v| Version::parse(v).ok()) {
            if v.line() == line {
                return true;
            }
        }
    }
    let Ok(prefix) = template.render(&[("version", line)]) else { return false };
    match head.strip_prefix(prefix.as_str()) {
        Some("") => true,
        Some(rest) => rest.starts_with(['.', '-', '/', '_']),
        None => false,
    }
}

/// Open pull requests into the line's branch or from one of its hotfix branches. The
/// listing goes to the repository the remote's URL names, on that host, with the same
/// cleared environment as every other gh call; any failure is an error (fail closed).
pub fn pull_requests(l: &Loaded, git: &Git, view: &RemoteView, line: &str) -> Result<Prs, String> {
    let Some((host, repo)) = crate::git::forge_repo(&view.fetch_url).or_else(|| crate::git::forge_repo(&view.raw_url))
    else {
        return Ok(Prs::NoForge);
    };
    let program = crate::steps::resolve_program("gh", &l.root, &l.run_path())?;
    let limit = PR_LIMIT.to_string();
    let out = std::process::Command::new(program)
        .env_clear()
        .envs(git.env.iter().cloned())
        .env("GH_HOST", &host)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .current_dir(&l.root)
        .stdin(std::process::Stdio::null())
        .args(["pr", "list", "--repo", &format!("{host}/{repo}"), "--state", "open", "--limit", &limit])
        .args(["--json", "number,title,baseRefName,headRefName"])
        .output()
        .map_err(|e| format!("cannot run gh: {e}"))?;
    if !out.status.success() {
        return Err(crate::util::redact(&format!(
            "gh pr list --repo {host}/{repo} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let prs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("gh pr list --repo {host}/{repo} returned something other than a JSON list: {e}"))?;
    if prs.len() >= PR_LIMIT {
        return Err(format!(
            "{host}/{repo} has {PR_LIMIT} or more open pull requests; Edition reads at most {PR_LIMIT} and will not check a partial list"
        ));
    }
    let branch = l.line_branch_name(line);
    let mut items = Vec::new();
    for pr in &prs {
        let field = |k: &str| pr.get(k).and_then(|v| v.as_str()).unwrap_or("");
        let Some(number) = pr.get("number").and_then(|v| v.as_u64()) else {
            return Err(format!("gh pr list --repo {host}/{repo} returned a pull request without a number"));
        };
        let (base, head) = (field("baseRefName"), field("headRefName"));
        if base != branch && !is_line_hotfix(&l.hotfix_branch, head, line) {
            continue;
        }
        let title: String = field("title").chars().filter(|c| !c.is_control()).take(120).collect();
        items.push(Item {
            id: format!("#{number}"),
            what: format!("open pull request #{number} {head} -> {base}: {title}"),
        });
    }
    Ok(Prs::Read(items))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).expect("version")
    }

    #[test]
    fn unreleased_counts_only_with_entries() {
        assert!(!unreleased_has_entries("# Changelog\n\n## [Unreleased]\n\n## [0.5.7]\n- fix\n"));
        assert!(!unreleased_has_entries("## Unreleased\n### Added\n\n## 0.5.7\n- fix\n"));
        assert!(unreleased_has_entries("## [Unreleased]\n### Fixed\n- a crash\n## 0.5.7\n"));
        assert!(unreleased_has_entries("## Unreleased\nSomething\n"));
        assert!(!unreleased_has_entries("## 0.5.7\n- Unreleased work was folded in\n"));
    }

    #[test]
    fn later_patches_on_the_same_line_only() {
        let text = "# Changelog\n## [0.5.8] - 2026-11-30\n## v0.5.9\n## 0.6.0\n## 0.5.7\n## 0.5.6\n#### 0.5.8\n";
        let got = later_patches(text, &v("0.5.7"));
        assert_eq!(got, vec![v("0.5.8"), v("0.5.9")]);
        assert!(later_patches("## 0.5.10-rc.1\n", &v("0.5.7")).is_empty());
    }

    #[test]
    fn hotfix_branches_of_a_line() {
        let t = Template::parse("hotfix/{version}").expect("template");
        assert!(is_line_hotfix(&t, "hotfix/0.5.4", "0.5"));
        assert!(is_line_hotfix(&t, "hotfix/0.5", "0.5"));
        assert!(is_line_hotfix(&t, "hotfix/0.5-ledger", "0.5"));
        assert!(!is_line_hotfix(&t, "hotfix/0.50.1", "0.5"));
        assert!(!is_line_hotfix(&t, "hotfix/0.6.1", "0.5"));
        assert!(!is_line_hotfix(&t, "feature/0.5.4", "0.5"));
    }

    #[test]
    fn allowances_name_items() {
        let pr = Item { id: "#12".into(), what: String::new() };
        assert!(pr.allowed_by("#12") && pr.allowed_by("12"));
        assert!(!pr.allowed_by("1") && !pr.allowed_by("#123"));
        let c = Item { id: "changelog:0.5.8".into(), what: String::new() };
        assert!(c.allowed_by("changelog:0.5.8") && !c.allowed_by("0.5.8"));
    }
}
