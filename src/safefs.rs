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

/// Replaces a configured file's contents: a new file beside it (same mode), then a rename.
pub fn replace(root: &Path, rel: &str, text: &str) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = checked(root, rel)?;
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
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    // Check again right before the rename: a link introduced meanwhile refuses.
    if let Err(e) = checked(root, rel) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{rel}: {e}")
    })
}
