//! The tree a deploy's checks run on.
//!
//! Checks must see exactly the deployed commit's tree, so Edition writes that tree itself
//! into a private directory of its own, and only once the plan's receipt is spent: a dry
//! run never writes it. The files come from `git ls-tree -r -z` (the entries) and
//! `git cat-file --batch` (the raw blob bytes). Neither command checks anything out, so
//! no smudge, clean or process filter, `.gitattributes` conversion, hook, submodule or
//! LFS download can run, and `GIT_NO_LAZY_FETCH=1` keeps a partial clone from fetching
//! a missing object. `git archive` was ruled out because it applies working-tree
//! conversions, filters among them; `git worktree add` and `checkout` because they run
//! filters too.
//!
//! Each plan gets its own directory, `.edition/checks/<receipt>`, beside a lock file
//! held for as long as the checks run. The directory must not exist beforehand (a link,
//! a leftover worktree or anything else there refuses the run). It is compared with the
//! bound tree after it is written and again before each check, and removed when the run
//! ends. A run that was killed leaves its directory and an unlocked lock file. The next
//! run sweeps those away, and leaves anything without an Edition lock file alone.

use crate::git::Git;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Component, Path, PathBuf};

/// How the tree is written, as bound in the plan.
pub const METHOD: &str = "edition-blobs-v1 (git ls-tree -r + git cat-file --batch; no checkout, filters, attributes, hooks, submodules or lazy fetch)";

/// Where per-plan check trees live, under the repository root.
pub const DIR: &str = ".edition/checks";

/// One entry of the tree: `(mode, object id)` by path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub oid: String,
}

/// Every file in `tree`, keyed by path. Paths are checked to be plain relative paths.
pub fn list(git: &Git, tree: &str) -> Result<BTreeMap<String, TreeEntry>, String> {
    let out = git.run_bytes(&["ls-tree", "-r", "-z", "--full-tree", tree])?;
    let mut entries = BTreeMap::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let rec = String::from_utf8(rec.to_vec()).map_err(|_| "the tree has a path that is not UTF-8".to_string())?;
        let (meta, path) = rec.split_once('\t').ok_or("unexpected ls-tree output")?;
        let mut m = meta.split(' ');
        let (Some(mode), Some(_kind), Some(oid)) = (m.next(), m.next(), m.next()) else {
            return Err("unexpected ls-tree output".into());
        };
        let p = Path::new(path);
        if path.is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
            return Err(format!("the tree has an unusable path {path:?}"));
        }
        if !["100644", "100755", "120000", "160000"].contains(&mode) {
            return Err(format!("the tree has {path:?} with an unknown mode {mode}"));
        }
        entries.insert(path.to_string(), TreeEntry { mode: mode.to_string(), oid: oid.to_string() });
    }
    Ok(entries)
}

/// True when `rel` names an executable file in the tree.
pub fn is_executable(entries: &BTreeMap<String, TreeEntry>, rel: &str) -> bool {
    let rel = rel.strip_prefix("./").unwrap_or(rel);
    entries.get(rel).is_some_and(|e| e.mode == "100755")
}

/// A check tree written for one plan. Dropping it removes the directory.
pub struct Materialised {
    pub dir: PathBuf,
    lock_path: PathBuf,
    lock: Option<File>,
    entries: BTreeMap<String, TreeEntry>,
    sha256: bool,
}

impl Drop for Materialised {
    fn drop(&mut self) {
        // remove_dir_all does not follow links.
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_file(&self.lock_path);
        self.lock.take();
    }
}

fn private_dirs(root: &Path) -> Result<PathBuf, String> {
    crate::steps::ensure_private_dir(&root.join(".edition"))?;
    let ignore = root.join(".edition/.gitignore");
    if std::fs::symlink_metadata(&ignore).is_err() {
        let _ = std::fs::write(&ignore, "*\n");
    }
    let base = root.join(DIR);
    crate::steps::ensure_private_dir(&base)?;
    Ok(base)
}

fn valid_name(name: &str) -> bool {
    crate::util::valid_receipt(name)
}

/// Removes check trees left by runs that ended without cleaning up: a directory whose
/// Edition lock file exists and is not held. Anything else is left alone.
pub fn sweep(root: &Path) -> Result<(), String> {
    let base = private_dirs(root)?;
    let Ok(rd) = std::fs::read_dir(&base) else { return Ok(()) };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".lock").filter(|s| valid_name(s)) else { continue };
        let lock_path = base.join(&name);
        let Ok(f) = open_lock(&lock_path, false) else { continue };
        if f.try_lock().is_ok() {
            let dir = base.join(stem);
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_file(&lock_path);
        }
    }
    Ok(())
}

fn open_lock(path: &Path, create: bool) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(create)
        .mode(0o600)
        .custom_flags(crate::steps::NOFOLLOW)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    if !f.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    Ok(f)
}

/// Writes `tree` into `.edition/checks/<name>` and checks the result against it.
pub fn materialise(git: &Git, root: &Path, tree: &str, name: &str) -> Result<Materialised, String> {
    if !valid_name(name) {
        return Err("a check tree is named by a 32-character receipt".into());
    }
    let base = private_dirs(root)?;
    let lock_path = base.join(format!("{name}.lock"));
    let dir = base.join(name);
    // The lock file comes first, so a sweep never takes a directory being written.
    let lock = open_lock(&lock_path, true)
        .map_err(|e| format!("{e}; another run is using this receipt's check tree, or it was left behind"))?;
    lock.lock().map_err(|e| format!("cannot lock {}: {e}", lock_path.display()))?;
    let mut m = Materialised { dir: dir.clone(), lock_path, lock: Some(lock), entries: BTreeMap::new(), sha256: false };
    {
        use std::os::unix::fs::DirBuilderExt;
        if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            // Not ours: leave it. Dropping `m` removes only the lock file, so a sweep
            // never takes this directory either.
            m.dir = PathBuf::new();
            return Err(format!(
                "{} already exists or cannot be made ({e}); it is not Edition's, so the checks do not run",
                dir.display()
            ));
        }
    }
    m.sha256 = git.run(&["rev-parse", "--show-object-format"])?.trim() == "sha256";
    m.entries = list(git, tree)?;
    write_tree(git, &dir, &m.entries)?;
    m.verify(true).map_err(|e| format!("the check tree does not match {tree} after writing it: {e}"))?;
    Ok(m)
}

fn write_tree(git: &Git, dir: &Path, entries: &BTreeMap<String, TreeEntry>) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let mut made: BTreeSet<PathBuf> = BTreeSet::new();
    let mkdir = |p: &Path, made: &mut BTreeSet<PathBuf>| -> Result<(), String> {
        let mut cur = dir.to_path_buf();
        for c in p.components() {
            cur.push(c);
            if made.contains(&cur) {
                continue;
            }
            std::fs::DirBuilder::new().mode(0o700).create(&cur).map_err(|e| format!("{}: {e}", cur.display()))?;
            made.insert(cur.clone());
        }
        Ok(())
    };
    for (path, e) in entries.iter().filter(|(_, e)| e.mode == "160000") {
        // A submodule is never fetched or checked out: its place is an empty directory.
        mkdir(Path::new(path), &mut made)?;
        let _ = e;
    }
    let blobs: Vec<(&String, &TreeEntry)> = entries.iter().filter(|(_, e)| e.mode != "160000").collect();
    let oids: Vec<String> = blobs.iter().map(|(_, e)| e.oid.clone()).collect();
    let mut i = 0;
    git.cat_blobs(&oids, &mut |oid, data| {
        let (path, e) = blobs.get(i).ok_or("more blobs than asked for")?;
        i += 1;
        if e.oid != oid {
            return Err(format!("expected blob {} for {path}, got {oid}", e.oid));
        }
        let rel = Path::new(path.as_str());
        if let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
            mkdir(parent, &mut made)?;
        }
        let full = dir.join(rel);
        if e.mode == "120000" {
            use std::os::unix::ffi::OsStrExt;
            let target = std::ffi::OsStr::from_bytes(data);
            std::os::unix::fs::symlink(target, &full).map_err(|err| format!("{path}: {err}"))?;
        } else {
            let mode = if e.mode == "100755" { 0o755 } else { 0o644 };
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .custom_flags(crate::steps::NOFOLLOW)
                .open(&full)
                .map_err(|err| format!("{path}: {err}"))?;
            f.write_all(data).map_err(|err| format!("{path}: {err}"))?;
        }
        Ok(())
    })?;
    if i != blobs.len() {
        return Err(format!("git returned {i} of {} blobs", blobs.len()));
    }
    Ok(())
}

impl Materialised {
    /// Compares the directory with the bound tree. Every file must have its mode and
    /// content; with `exact`, nothing else may be there either. Links are read, never
    /// followed.
    pub fn verify(&self, exact: bool) -> Result<(), String> {
        let found = scan(&self.dir, self.sha256)?;
        for (path, want) in &self.entries {
            let Some(got) = found.get(path) else { return Err(format!("{path} is missing")) };
            if got.mode != want.mode || (want.mode != "160000" && got.oid != want.oid) {
                return Err(format!("{path} was changed"));
            }
        }
        if exact {
            if let Some(extra) = found.keys().find(|p| !self.entries.contains_key(*p)) {
                return Err(format!("{extra} is not in the tree"));
            }
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
                let oid = blob_id(target.as_os_str().as_encoded_bytes(), sha256);
                out.insert(key, TreeEntry { mode: "120000".into(), oid });
            } else if ft.is_file() {
                let data = std::fs::read(e.path()).map_err(|err| format!("{key}: {err}"))?;
                let mode = if m.permissions().mode() & 0o100 != 0 { "100755" } else { "100644" };
                out.insert(key, TreeEntry { mode: mode.into(), oid: blob_id(&data, sha256) });
            } else {
                return Err(format!("{key} is not a file, link or directory"));
            }
        }
        if empty && !rel.as_os_str().is_empty() {
            let key = rel.to_str().ok_or("a path in the check tree is not UTF-8")?.to_string();
            out.insert(key, TreeEntry { mode: "160000".into(), oid: String::new() });
        }
    }
    Ok(out)
}

/// A git blob id for `data` in the repository's object format.
pub fn blob_id(data: &[u8], sha256: bool) -> String {
    let mut buf = format!("blob {}\0", data.len()).into_bytes();
    buf.extend_from_slice(data);
    if sha256 {
        crate::util::sha256_hex(&buf)
    } else {
        crate::util::sha1_hex(&buf)
    }
}
