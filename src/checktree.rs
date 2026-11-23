//! The tree a deploy's checks run on.
//!
//! Checks must see exactly the deployed commit's tree and nothing else, so Edition writes
//! that tree itself, afresh for every check, and only once the plan's receipt is spent: a
//! dry run never writes it.
//!
//! **How it is written.** The entries come from `git ls-tree -r -z -l` and the raw blob
//! bytes from `git cat-file --batch`, both with `GIT_NO_LAZY_FETCH=1`. Neither command
//! checks anything out, so no smudge, clean or process filter, `.gitattributes`
//! conversion, hook, submodule or LFS download can run, and a partial clone cannot fetch a
//! missing object. (`git archive` and `git worktree add` both run smudge filters.) Blobs
//! are streamed to disk and hashed in pieces; limits on the entry count, the largest file
//! and the total size refuse a tree before anything is written.
//!
//! **What a tree may hold.** No path component that git itself would treat as `.git`
//! (in any case, with trailing dots or spaces, as the 8.3 name `git~1`, or with the
//! code points HFS+ ignores), no link that is absolute or climbs with `..`, and no path
//! that collides with another on this file system (case or Unicode normalisation): each
//! refuses the run. A submodule's place is an empty directory.
//!
//! **Where.** Outside every repository, in a private per-user directory:
//! `$XDG_CACHE_HOME/edition/checks`, else `~/.cache/edition/checks`. Every ancestor must
//! belong to root or the current user and not be writable by others unless sticky. Each
//! check gets `<base>/<receipt>-<n>/tree`, and checks run with `GIT_CEILING_DIRECTORIES`
//! set to the tree's parent, so a `git` command in a check finds no repository at all.
//!
//! **Ownership.** One lock, `<base>/.lock`, is held while a directory is created and
//! while stale ones are swept. The directory is made with `mkdir`, which fails if anything
//! is already there (refusing the run). Only then is an ownership marker, `owner`, written
//! inside it, naming the directory, and locked for as long as the check runs. A sweep
//! removes only a directory whose marker names it and is not locked; a directory without
//! a valid marker is never removed. The directory is removed when its check ends.

use crate::git::Git;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

/// How the tree is written, as bound in the plan.
pub const METHOD: &str = "edition-blobs-v2 (git ls-tree -r -l + streamed git cat-file --batch; a fresh tree per check; no checkout, filters, attributes, hooks, submodules, lazy fetch, .git paths or escaping links)";

const MARKER: &str = "edition-check-tree v1";

/// Limits on a check tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_entries: u64,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_entries: 100_000, max_file_bytes: 256 << 20, max_total_bytes: 2 << 30 }
    }
}

/// One entry of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub oid: String,
    pub size: u64,
}

/// The listed tree: entries by path.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub entries: BTreeMap<String, TreeEntry>,
}

/// True when git would treat this path component as `.git` on some file system: compared
/// without case, without trailing dots and spaces, without an NTFS stream suffix, as the
/// 8.3 name `git~1`, and without the code points HFS+ ignores.
pub fn is_git_component(name: &str) -> bool {
    let ignorable = |c: char| matches!(c as u32, 0x200C..=0x200F | 0x202A..=0x202E | 0x206A..=0x206F | 0xFEFF);
    let cleaned: String = name.chars().filter(|c| !ignorable(*c)).collect();
    let base = cleaned.split(':').next().unwrap_or("");
    let base = base.trim_end_matches([' ', '.']).to_ascii_lowercase();
    base == ".git" || base == "git~1"
}

fn check_path(path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if path.is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(format!("the tree has an unusable path {path:?}"));
    }
    if path.chars().any(char::is_control) {
        return Err(format!("the tree has a path with a control character: {path:?}"));
    }
    if let Some(c) = path.split('/').find(|c| is_git_component(c)) {
        return Err(format!("the tree has {path:?}, whose component {c:?} git treats as .git"));
    }
    Ok(())
}

/// A link target may only lead down inside the tree: relative, no `..`, not empty.
fn check_link(path: &str, target: &[u8]) -> Result<(), String> {
    let t = std::str::from_utf8(target).map_err(|_| format!("the link {path} has a target that is not UTF-8"))?;
    let tp = Path::new(t);
    if t.is_empty()
        || tp.is_absolute()
        || !tp.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!("the link {path} points to {t:?}, outside the tree or upward"));
    }
    if let Some(c) = t.split('/').find(|c| is_git_component(c)) {
        return Err(format!("the link {path} points into {c:?}"));
    }
    Ok(())
}

/// Every entry of `tree`, checked against the path rules and the limits.
pub fn list(git: &Git, tree: &str, limits: &Limits) -> Result<Tree, String> {
    let out = git.run_bytes(&["ls-tree", "-r", "-z", "-l", "--full-tree", tree])?;
    let mut entries = BTreeMap::new();
    let mut total: u64 = 0;
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let rec = String::from_utf8(rec.to_vec()).map_err(|_| "the tree has a path that is not UTF-8".to_string())?;
        let (meta, path) = rec.split_once('\t').ok_or("unexpected ls-tree output")?;
        let f: Vec<&str> = meta.split_whitespace().collect();
        let [mode, _kind, oid, size] = f.as_slice() else { return Err("unexpected ls-tree output".into()) };
        check_path(path)?;
        if !["100644", "100755", "120000", "160000"].contains(mode) {
            return Err(format!("the tree has {path:?} with an unknown mode {mode}"));
        }
        let size: u64 = if *mode == "160000" { 0 } else { size.parse().map_err(|_| "unexpected ls-tree size")? };
        if size > limits.max_file_bytes {
            return Err(format!(
                "{path} is {size} bytes, over the check tree limit of {} (check_tree.max_file_bytes)",
                limits.max_file_bytes
            ));
        }
        total = total.saturating_add(size);
        entries.insert(path.to_string(), TreeEntry { mode: mode.to_string(), oid: oid.to_string(), size });
        if entries.len() as u64 > limits.max_entries {
            return Err(format!("the tree has more than {} entries (check_tree.max_entries)", limits.max_entries));
        }
    }
    if total > limits.max_total_bytes {
        return Err(format!(
            "the tree holds {total} bytes, over the check tree limit of {} (check_tree.max_total_bytes)",
            limits.max_total_bytes
        ));
    }
    Ok(Tree { entries })
}

/// True when `rel` names an executable file in the tree.
pub fn is_executable(tree: &Tree, rel: &str) -> bool {
    let rel = rel.strip_prefix("./").unwrap_or(rel);
    tree.entries.get(rel).is_some_and(|e| e.mode == "100755")
}

/// Where check trees are written: a private per-user directory outside any repository.
pub fn base_dir() -> Result<PathBuf, String> {
    let cache = match std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).filter(|p| p.is_absolute()) {
        Some(p) => p,
        None => {
            let home = std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute());
            home.ok_or("HOME must be an absolute path to place the check tree")?.join(".cache")
        }
    };
    Ok(cache.join("edition").join("checks"))
}

/// Creates the base directory: its ancestors must be trustworthy, `edition` and `checks`
/// private.
fn prepare_base(base: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let edition = base.parent().ok_or("no parent for the check tree directory")?;
    let cache = edition.parent().ok_or("no parent for the check tree directory")?;
    if std::fs::symlink_metadata(cache).is_err() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(cache)
            .map_err(|e| format!("cannot create {}: {e}", cache.display()))?;
    }
    let real = cache.canonicalize().map_err(|e| format!("{}: {e}", cache.display()))?;
    let uid = crate::config::current_uid()?;
    let mut cur = Some(real.as_path());
    while let Some(dir) = cur {
        let m = std::fs::metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mode = m.permissions().mode();
        if (m.uid() != 0 && m.uid() != uid) || (mode & 0o002 != 0 && mode & 0o1000 == 0) {
            return Err(format!(
                "{} is not safe for the check tree (it must belong to you or root and not be writable by others)",
                dir.display()
            ));
        }
        cur = dir.parent();
    }
    crate::steps::ensure_private_dir(edition)?;
    crate::steps::ensure_private_dir(base)
}

/// The name of check `index` of the plan with receipt `receipt`.
pub fn name(receipt: &str, index: usize) -> String {
    format!("{receipt}-{index}")
}

fn valid_name(name: &str) -> bool {
    match name.split_once('-') {
        Some((r, n)) => crate::util::valid_receipt(r) && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

fn open_nofollow(path: &Path, create: bool) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(create)
        .mode(0o600)
        .custom_flags(crate::steps::NOFOLLOW)
        .open(path)
}

/// Holds `<base>/.lock` for creating and sweeping.
fn global_lock(base: &Path) -> Result<File, String> {
    let p = base.join(".lock");
    let f = match open_nofollow(&p, false) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => open_nofollow(&p, true)
            .or_else(|_| open_nofollow(&p, false))
            .map_err(|e| format!("{}: {e}", p.display()))?,
        Err(e) => return Err(format!("{}: {e}", p.display())),
    };
    if !f.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("{} is not a regular file", p.display()));
    }
    f.lock().map_err(|e| format!("cannot lock {}: {e}", p.display()))?;
    Ok(f)
}

/// The ownership marker in `dir`, if it is a valid one for `name`, opened.
fn owned_marker(dir: &Path, name: &str) -> Option<File> {
    let mut f = open_nofollow(&dir.join("owner"), false).ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    f.read_to_string(&mut text).ok()?;
    (text == format!("{MARKER} {name}\n")).then_some(f)
}

/// Removes check trees whose runs ended without cleaning up. Call with the global lock.
fn sweep_locked(base: &Path) {
    let Ok(rd) = std::fs::read_dir(base) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !valid_name(&name) {
            continue;
        }
        let dir = base.join(&name);
        if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let Some(f) = owned_marker(&dir, &name) else { continue };
        if f.try_lock().is_ok() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// Removes stale check trees (see `sweep_locked`), taking the global lock.
pub fn sweep(base: &Path) -> Result<(), String> {
    prepare_base(base)?;
    let _g = global_lock(base)?;
    sweep_locked(base);
    Ok(())
}

/// A check tree written for one check. Dropping it removes the directory.
pub struct Materialised {
    /// The directory holding the owner marker and `tree`.
    pub home: PathBuf,
    /// Where the check runs.
    pub dir: PathBuf,
    owner: Option<File>,
    tree: Tree,
    sha256: bool,
}

impl Drop for Materialised {
    fn drop(&mut self) {
        // Removed while the marker is still locked, so a sweep never races it.
        let _ = std::fs::remove_dir_all(&self.home);
        self.owner.take();
    }
}

/// Writes the tree into `<base>/<name>/tree` and checks the result against it exactly.
pub fn materialise(git: &Git, base: &Path, tree: &Tree, name: &str, limits: &Limits) -> Result<Materialised, String> {
    use std::os::unix::fs::DirBuilderExt;
    if !valid_name(name) {
        return Err("a check tree is named by its receipt and check number".into());
    }
    let sha256 = git.run(&["rev-parse", "--show-object-format"])?.trim() == "sha256";
    prepare_base(base)?;
    let home = base.join(name);
    let owner = {
        let _g = global_lock(base)?;
        sweep_locked(base);
        std::fs::DirBuilder::new().mode(0o700).create(&home).map_err(|e| {
            format!(
                "{} already exists or cannot be made ({e}); it is not Edition's, so the check does not run",
                home.display()
            )
        })?;
        let mut f = open_nofollow(&home.join("owner"), true).map_err(|e| format!("{}: {e}", home.display()))?;
        f.write_all(format!("{MARKER} {name}\n").as_bytes()).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        f.lock().map_err(|e| format!("cannot lock {}: {e}", home.display()))?;
        f
    };
    let dir = home.join("tree");
    let m = Materialised { home, dir: dir.clone(), owner: Some(owner), tree: tree.clone(), sha256 };
    std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    write_tree(git, &dir, &m.tree, limits)?;
    m.verify().map_err(|e| format!("the check tree does not match after writing it: {e}"))?;
    Ok(m)
}

const COLLIDE: &str = "does the tree hold two paths that are the same on this file system?";

fn write_tree(git: &Git, dir: &Path, tree: &Tree, limits: &Limits) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let mut made: BTreeSet<PathBuf> = BTreeSet::new();
    let mkdir = |p: &Path, made: &mut BTreeSet<PathBuf>| -> Result<(), String> {
        let mut cur = dir.to_path_buf();
        for c in p.components() {
            cur.push(c);
            if made.contains(&cur) {
                continue;
            }
            // Fails when the name already exists, as it does when two paths collide by
            // case or Unicode normalisation on this file system.
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&cur)
                .map_err(|e| format!("{}: {e} ({COLLIDE})", cur.display()))?;
            made.insert(cur.clone());
        }
        Ok(())
    };
    for path in tree.entries.iter().filter(|(_, e)| e.mode == "160000").map(|(p, _)| p) {
        mkdir(Path::new(path), &mut made)?;
    }
    let blobs: Vec<(&String, &TreeEntry)> = tree.entries.iter().filter(|(_, e)| e.mode != "160000").collect();
    let oids: Vec<String> = blobs.iter().map(|(_, e)| e.oid.clone()).collect();
    let mut i = 0;
    let mut total: u64 = 0;
    git.cat_blobs(&oids, &mut |oid, size, data| {
        let (path, e) = blobs.get(i).ok_or("more blobs than asked for")?;
        i += 1;
        if e.oid != oid || e.size != size || size > limits.max_file_bytes {
            return Err(format!("git returned an unexpected object for {path}"));
        }
        total = total.saturating_add(size);
        if total > limits.max_total_bytes {
            return Err("the tree is over check_tree.max_total_bytes".into());
        }
        let rel = Path::new(path.as_str());
        if let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
            mkdir(parent, &mut made)?;
        }
        let full = dir.join(rel);
        if e.mode == "120000" {
            let mut target = Vec::new();
            data.take(4097).read_to_end(&mut target).map_err(|err| err.to_string())?;
            if target.len() > 4096 {
                return Err(format!("the link {path} has a target over 4096 bytes"));
            }
            check_link(path, &target)?;
            use std::os::unix::ffi::OsStrExt;
            std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&target), &full)
                .map_err(|err| format!("{path}: {err} ({COLLIDE})"))?;
        } else {
            let mode = if e.mode == "100755" { 0o755 } else { 0o644 };
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .custom_flags(crate::steps::NOFOLLOW)
                .open(&full)
                .map_err(|err| format!("{path}: {err} ({COLLIDE})"))?;
            std::io::copy(data, &mut f).map_err(|err| format!("{path}: {err}"))?;
        }
        Ok(())
    })?;
    if i != blobs.len() {
        return Err(format!("git returned {i} of {} blobs", blobs.len()));
    }
    Ok(())
}

impl Materialised {
    /// Compares the directory with the bound tree exactly: every entry with its mode and
    /// content, and nothing else. Links are read, never followed; files are hashed as
    /// they are read.
    pub fn verify(&self) -> Result<(), String> {
        let found = scan(&self.dir, self.sha256)?;
        for (path, want) in &self.tree.entries {
            let Some(got) = found.get(path) else { return Err(format!("{path} is missing")) };
            if got.mode != want.mode || (want.mode != "160000" && got.oid != want.oid) {
                return Err(format!("{path} was changed"));
            }
        }
        if let Some(extra) = found.keys().find(|p| !self.tree.entries.contains_key(*p)) {
            return Err(format!("{extra} is not in the tree"));
        }
        Ok(())
    }
}

/// What a directory holds, as tree entries: regular files and links are hashed as git
/// blobs; an empty directory stands for a submodule.
fn scan(dir: &Path, sha256: bool) -> Result<BTreeMap<String, TreeEntry>, String> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = BTreeMap::new();
    let mut stack = vec![PathBuf::new()];
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    while let Some(rel) = stack.pop() {
        let full = dir.join(&rel);
        let rd = std::fs::read_dir(&full).map_err(|e| format!("{}: {e}", full.display()))?;
        let mut empty = true;
        for e in rd {
            empty = false;
            let e = e.map_err(|e| e.to_string())?;
            let r = rel.join(e.file_name());
            let key = r.to_str().ok_or("a path in the check tree is not UTF-8")?.to_string();
            let m = std::fs::symlink_metadata(e.path()).map_err(|err| format!("{key}: {err}"))?;
            let ft = m.file_type();
            if ft.is_dir() {
                stack.push(r);
            } else if ft.is_symlink() {
                let target = std::fs::read_link(e.path()).map_err(|err| format!("{key}: {err}"))?;
                let bytes = target.as_os_str().as_encoded_bytes();
                let oid = blob_id(bytes, sha256);
                out.insert(key, TreeEntry { mode: "120000".into(), oid, size: bytes.len() as u64 });
            } else if ft.is_file() {
                let mode = if m.permissions().mode() & 0o100 != 0 { "100755" } else { "100644" };
                let (oid, size) = hash_file(&e.path(), m.len(), sha256).map_err(|err| format!("{key}: {err}"))?;
                out.insert(key, TreeEntry { mode: mode.into(), oid, size });
            } else {
                return Err(format!("{key} is not a file, link or directory"));
            }
        }
        if empty && !rel.as_os_str().is_empty() {
            let key = rel.to_str().ok_or("a path in the check tree is not UTF-8")?.to_string();
            out.insert(key, TreeEntry { mode: "160000".into(), oid: String::new(), size: 0 });
        }
    }
    Ok(out)
}

/// A file's git blob id, read in pieces. A file that changes length while it is read
/// gives an id that matches nothing.
fn hash_file(path: &Path, len: u64, sha256: bool) -> std::io::Result<(String, u64)> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(crate::steps::NOFOLLOW).open(path)?;
    let mut h = crate::util::StreamHash::new(sha256);
    h.update(format!("blob {len}\0").as_bytes());
    let mut buf = vec![0u8; 64 * 1024];
    let mut read: u64 = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        read += n as u64;
        h.update(&buf[..n]);
    }
    if read != len {
        return Ok((String::new(), read));
    }
    Ok((h.finish(), read))
}

/// A git blob id for `data` in the repository's object format.
pub fn blob_id(data: &[u8], sha256: bool) -> String {
    let mut h = crate::util::StreamHash::new(sha256);
    h.update(format!("blob {}\0", data.len()).as_bytes());
    h.update(data);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_components_in_every_spelling() {
        for n in [
            ".git",
            ".GIT",
            ".Git",
            ".git.",
            ".git ",
            ".git. .",
            "git~1",
            "GIT~1",
            ".git::$INDEX_ALLOCATION",
            ".g\u{200c}it",
            "\u{feff}.git",
        ] {
            assert!(is_git_component(n), "{n:?}");
        }
        for n in [".gitignore", ".github", "git", ".gi", "a.git", ".git2"] {
            assert!(!is_git_component(n), "{n:?}");
        }
        assert!(check_path("sub/.GIT/config").is_err());
        assert!(check_path("a/b.txt").is_ok());
    }

    #[test]
    fn links_lead_only_down() {
        for ok in ["a", "a/b", "./a", "a/./b"] {
            assert!(check_link("l", ok.as_bytes()).is_ok(), "{ok}");
        }
        for bad in ["", "/etc/passwd", "../x", "a/../../x", "a/..", ".git/config", "a/.GIT"] {
            assert!(check_link("l", bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn names_carry_a_receipt_and_a_check_number() {
        let r = "0123456789abcdef0123456789abcdef";
        assert!(valid_name(&name(r, 0)));
        assert!(!valid_name(r));
        assert!(!valid_name(&format!("{r}-")));
        assert!(!valid_name(&format!("{r}-x")));
    }
}
