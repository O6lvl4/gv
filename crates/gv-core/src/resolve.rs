//! Resolve which Go version applies in the current context.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::manifest::{find_project_toolchain, ToolchainHit, ToolchainSource};
use crate::paths::Paths;

#[derive(Debug, Clone)]
pub struct Resolved {
    pub version: String,
    pub source: ToolchainSource,
    pub origin: Option<PathBuf>,
}

pub fn resolve(paths: &Paths, cwd: &Path) -> Result<Option<Resolved>> {
    if let Ok(v) = std::env::var("GV_VERSION") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Ok(Some(Resolved {
                version: normalize(&v),
                source: ToolchainSource::EnvVar,
                origin: None,
            }));
        }
    }

    if let Some(hit) = find_project_toolchain(cwd)? {
        let ToolchainHit { version, source, origin } = hit;
        return Ok(Some(Resolved {
            version,
            source,
            origin: Some(origin),
        }));
    }

    let global = paths.global_version_file();
    if global.is_file() {
        let raw = std::fs::read_to_string(&global)?;
        let v = raw.trim();
        if !v.is_empty() {
            return Ok(Some(Resolved {
                version: normalize(v),
                source: ToolchainSource::Global,
                origin: Some(global),
            }));
        }
    }

    if let Some(latest) = pick_latest_installed(paths)? {
        return Ok(Some(Resolved {
            version: latest,
            source: ToolchainSource::LatestInstalled,
            origin: None,
        }));
    }

    Ok(None)
}

pub fn list_installed(paths: &Paths) -> Result<Vec<String>> {
    let dir = paths.versions();
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("go") {
            out.push(name);
        }
    }
    out.sort_by(|a, b| compare_versions(b, a)); // descending
    Ok(out)
}

fn pick_latest_installed(paths: &Paths) -> Result<Option<String>> {
    let mut all = list_installed(paths)?;
    Ok(if all.is_empty() { None } else { Some(all.remove(0)) })
}

fn normalize(v: &str) -> String {
    let v = v.trim();
    if v.starts_with("go") { v.to_string() } else { format!("go{v}") }
}

fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let pa = parse_components(a);
    let pb = parse_components(b);
    pa.cmp(&pb)
}

fn parse_components(v: &str) -> (u64, u64, u64) {
    let s = v.strip_prefix("go").unwrap_or(v);
    let mut parts = s.split('.').map(|x| x.parse::<u64>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}
