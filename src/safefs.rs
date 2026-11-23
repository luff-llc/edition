//! Reading and writing files named in the configuration without following links.
//!
//! A configured path (a version file, a pin file, the legacy ledger) is relative to the
//! repository root. Every component is checked with `lstat` from the root down and a
//! link anywhere refuses the path; the final file is opened with `O_NOFOLLOW`; writes go
//! to a new file in the same directory that is then renamed over the old one, so no
//! write ever passes through a link.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

/// The checked absolute path of `rel` under `root`.
pub fn checked(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    if rel.is_empty() || !rel_path.is_relative() || !rel_path.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(format!("{rel} must be a relative path inside the repository"));
    }
    let mut cur = root.to_path_buf();
    for comp in rel_path.components() {
        cur.push(comp);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("{rel}: {} is a symbolic link; refusing to follow it", cur.display()))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("{rel}: {} does not exist", cur.display()))
            }
            Err(e) => return Err(format!("{rel}: {e}")),
        }
    }
    Ok(cur)
}

/// Reads a configured file without following links.
pub fn read(root: &Path, rel: &str) -> Result<String, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = checked(root, rel)?;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(crate::steps::NOFOLLOW)
        .open(&path)
        .map_err(|e| format!("{rel}: {e}"))?;
    if !f.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("{rel} is not a regular file"));
    }
    let mut text = String::new();
    f.read_to_string(&mut text).map_err(|e| format!("{rel}: {e}"))?;
    Ok(text)
}

fn identity(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).ok().filter(|m| !m.file_type().is_symlink()).map(|m| (m.dev(), m.ino()))
}

/// Replaces a configured file's contents: a new file beside it (same mode), then a rename.
///
/// Without `openat`/`renameat` (Edition has no libc dependency) the parent directory is
/// named by path at each step, so the sequence cannot be made atomic against a process
/// that can write to that directory. It is narrowed as far as the standard library
/// allows: the whole path is re-checked for links, and the parent directory's and the
/// temporary file's device and inode are compared, after the temporary file is created,
/// just before the rename and just after it. Any change before the rename aborts and
/// removes the temporary file (only if it is still the one created here); a change seen
/// after the rename is reported as an error, since it cannot be undone.
pub fn replace(root: &Path, rel: &str, text: &str) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let path = checked(root, rel)?;
    let parent = path.parent().ok_or_else(|| format!("{rel} has no parent directory"))?.to_path_buf();
    let parent_id = identity(&parent).ok_or_else(|| format!("{rel}: its directory is not a plain directory"))?;
    let mode = std::fs::symlink_metadata(&path).map(|m| m.permissions().mode() & 0o7777).map_err(|e| e.to_string())?;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.edition-{}", crate::util::random_hex(6)?));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(crate::steps::NOFOLLOW)
        .open(&tmp)
        .map_err(|e| format!("{rel}: cannot write beside it: {e}"))?;
    let tm = f.metadata().map_err(|e| e.to_string())?;
    let tmp_id = (tm.dev(), tm.ino());
    let unchanged = || -> Result<(), String> {
        checked(root, rel)?;
        if identity(&parent) != Some(parent_id) {
            return Err(format!("{rel}: its directory changed while it was being written; nothing was replaced"));
        }
        if identity(&tmp) != Some(tmp_id) {
            return Err(format!("{rel}: the temporary file changed while it was being written; nothing was replaced"));
        }
        Ok(())
    };
    let abort = |e: String| -> Result<(), String> {
        if identity(&tmp) == Some(tmp_id) {
            let _ = std::fs::remove_file(&tmp);
        }
        Err(e)
    };
    if let Err(e) = unchanged() {
        return abort(e);
    }
    if let Err(e) = f.write_all(text.as_bytes()).and_then(|_| f.sync_all()) {
        return abort(format!("{rel}: {e}"));
    }
    if let Err(e) = unchanged() {
        return abort(e);
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        return abort(format!("{rel}: {e}"));
    }
    if identity(&path) != Some(tmp_id) || identity(&parent) != Some(parent_id) {
        return Err(format!("{rel}: the directory changed during the rename; check {} by hand", path.display()));
    }
    Ok(())
}
