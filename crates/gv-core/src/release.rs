//! Query the official Go release index at https://go.dev/dl/?mode=json

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::platform::Platform;

const INDEX_URL: &str = "https://go.dev/dl/?mode=json&include=all";

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub version: String, // e.g. "go1.25.0"
    pub stable: bool,
    pub files: Vec<ReleaseFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseFile {
    pub filename: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub sha256: String,
    pub size: u64,
    pub kind: String, // "archive" | "installer" | "source"
}

pub async fn fetch_index(client: &reqwest::Client) -> Result<Vec<Release>> {
    let res = client
        .get(INDEX_URL)
        .send()
        .await
        .context("fetch Go release index")?
        .error_for_status()?;
    let releases: Vec<Release> = res.json().await.context("parse Go release index")?;
    Ok(releases)
}

/// Pick the archive file matching the host platform for a given version.
pub fn select_archive<'a>(
    releases: &'a [Release],
    version: &str,
    platform: Platform,
) -> Result<(&'a Release, &'a ReleaseFile)> {
    let normalized = normalize_version(version);
    let release = releases
        .iter()
        .find(|r| r.version == normalized)
        .ok_or_else(|| anyhow!("version {normalized} not found in release index"))?;

    let want_os = match platform.os {
        crate::platform::Os::Darwin => "darwin",
        crate::platform::Os::Linux => "linux",
        crate::platform::Os::Windows => "windows",
    };
    let want_arch = match platform.arch {
        crate::platform::Arch::Amd64 => "amd64",
        crate::platform::Arch::Arm64 => "arm64",
    };

    let file = release
        .files
        .iter()
        .find(|f| f.kind == "archive" && f.os == want_os && f.arch == want_arch)
        .ok_or_else(|| anyhow!("no archive for {want_os}/{want_arch} in {normalized}"))?;

    Ok((release, file))
}

/// Accept "1.25.0", "go1.25.0", "1.25" and return the canonical `go1.25.0`
/// form. The `go` prefix is added when missing and left in place when present.
/// We do not pad partial versions here — resolution against the release index
/// happens elsewhere.
pub fn normalize_version(input: &str) -> String {
    let s = input.trim().strip_prefix("go").unwrap_or(input.trim());
    format!("go{s}")
}

/// Latest stable version from the index.
pub fn latest_stable(releases: &[Release]) -> Option<&Release> {
    releases.iter().find(|r| r.stable)
}
