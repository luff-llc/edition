//! Reads Concordance's release entries (`concordance track --format json`). Edition keeps
//! no feature or release list of its own: what a release carries is Concordance's.

use crate::config::Loaded;
use crate::util::resolve_path;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Debug, Deserialize)]
pub struct Carried {
    pub id: String,
    #[serde(default)]
    pub ready: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub builds: BTreeMap<String, String>,
    #[serde(default)]
    pub carries: Vec<Carried>,
}

#[derive(Deserialize)]
struct Track {
    #[serde(default)]
    releases: Vec<Release>,
}

pub fn catalog_dir(l: &Loaded) -> Option<PathBuf> {
    l.cfg.concordance.as_ref().map(|c| resolve_path(&l.root, &l.expand(&c.catalog)))
}

pub fn release_id(l: &Loaded, version: &str) -> Option<String> {
    let c = l.cfg.concordance.as_ref()?;
    crate::version::Template::parse(&c.release).ok()?.render(&[("version", version)]).ok()
}

/// Runs `concordance track <catalog> --format json` into a temporary file and parses it.
pub fn releases(l: &Loaded) -> Result<Vec<Release>, String> {
    let c = l.cfg.concordance.as_ref().ok_or("no [concordance] table")?;
    let dir = catalog_dir(l).ok_or("no catalog")?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let out = std::env::temp_dir().join(format!("edition-track-{}-{nanos}.json", std::process::id()));
    let status = Command::new(l.expand(&c.bin))
        .arg("track")
        .arg(&dir)
        .args(["--format", "json", "--out"])
        .arg(&out)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", c.bin))?;
    if !status.status.success() {
        let _ = std::fs::remove_file(&out);
        return Err(format!("concordance track failed: {}", String::from_utf8_lossy(&status.stderr).trim()));
    }
    let text = std::fs::read_to_string(&out).map_err(|e| format!("cannot read {}: {e}", out.display()));
    let _ = std::fs::remove_file(&out);
    let t: Track = serde_json::from_str(&text?).map_err(|e| format!("cannot parse concordance track output: {e}"))?;
    Ok(t.releases)
}

/// The command that records an uploaded build as a Concordance fact.
pub fn log_uploaded(
    l: &Loaded,
    release: &str,
    platform: &str,
    build: &str,
    commit: &str,
    key: &str,
    by: &str,
) -> Option<Vec<String>> {
    let c = l.cfg.concordance.as_ref()?;
    let dir = catalog_dir(l)?;
    Some(vec![
        l.expand(&c.bin),
        "log".into(),
        "uploaded".into(),
        release.into(),
        "--platform".into(),
        platform.into(),
        "--build".into(),
        build.into(),
        "--commit".into(),
        commit.into(),
        "--catalog".into(),
        dir.display().to_string(),
        "--key".into(),
        key.into(),
        "--by".into(),
        by.into(),
    ])
}
