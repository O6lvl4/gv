//! Read project-level configuration: `go.mod` toolchain line, `.go-version`,
//! and (in the future) `gv.toml`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainHit {
    pub version: String, // "go1.25.0"
    pub source: ToolchainSource,
    pub origin: PathBuf, // file the value came from
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolchainSource {
    EnvVar,
    GoWork,
    GoMod,
    GoVersionFile,
    Global,
    LatestInstalled,
}

/// Walk up from `start` looking for the most authoritative toolchain pin.
///
/// Order of precedence (highest first):
/// 1. `go.work` `toolchain` line (workspace-wide)
/// 2. `go.mod` `toolchain` line at the nearest module
/// 3. `.go-version`
///
/// `go.work` wins because Go itself overrides per-member `go.mod` toolchain
/// pins when in workspace mode.
pub fn find_project_toolchain(start: &Path) -> Result<Option<ToolchainHit>> {
    // Pass 1 — look for go.work anywhere in the ancestor chain.
    let mut dir: Option<&Path> = Some(start);
    while let Some(d) = dir {
        let go_work = d.join(crate::workspace::WORKSPACE_FILE);
        if go_work.is_file() {
            let work = crate::workspace::load(d)?;
            if let Some(v) = work.toolchain {
                return Ok(Some(ToolchainHit {
                    version: v,
                    source: ToolchainSource::GoWork,
                    origin: go_work,
                }));
            }
            // go.work without a toolchain line: keep searching go.mod / .go-version.
            break;
        }
        dir = d.parent();
    }

    // Pass 2 — fall back to per-module pins.
    let mut dir: Option<&Path> = Some(start);
    while let Some(d) = dir {
        let go_mod = d.join("go.mod");
        if go_mod.is_file() {
            if let Some(v) = read_go_mod_toolchain(&go_mod)? {
                return Ok(Some(ToolchainHit {
                    version: v,
                    source: ToolchainSource::GoMod,
                    origin: go_mod,
                }));
            }
        }
        let go_version = d.join(".go-version");
        if go_version.is_file() {
            let raw = std::fs::read_to_string(&go_version)
                .with_context(|| format!("read {}", go_version.display()))?;
            let v = raw.trim();
            if !v.is_empty() {
                return Ok(Some(ToolchainHit {
                    version: normalize(v),
                    source: ToolchainSource::GoVersionFile,
                    origin: go_version,
                }));
            }
        }
        dir = d.parent();
    }
    Ok(None)
}

/// Parse the `toolchain` directive from a `go.mod` file. Returns `None` if no
/// toolchain line is present.
pub fn read_go_mod_toolchain(go_mod: &Path) -> Result<Option<String>> {
    let content =
        std::fs::read_to_string(go_mod).with_context(|| format!("read {}", go_mod.display()))?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("toolchain ") {
            return Ok(Some(normalize(rest.trim())));
        }
    }
    Ok(None)
}

fn normalize(v: &str) -> String {
    let v = v.trim();
    if v.starts_with("go") {
        v.to_string()
    } else {
        format!("go{v}")
    }
}
