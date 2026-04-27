//! Project-level configuration: `gv.toml` parsing and project-root discovery.
//!
//! `gv.toml` is optional. Without it, `gv` works off `go.mod` alone. With it,
//! you get pinned tools and (later) scripts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const PROJECT_FILE: &str = "gv.toml";
pub const LOCK_FILE: &str = "gv.lock";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Project {
    /// Optional Go toolchain pin. `go.mod`'s `toolchain` line takes precedence.
    #[serde(default)]
    pub go: Option<GoSection>,

    /// Map of tool-name → ToolSpec. Short form: `gopls = "latest"`.
    /// Long form: `[tools.gopls]` with explicit `package`.
    #[serde(default)]
    pub tools: BTreeMap<String, ToolSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoSection {
    pub version: String,
}

/// A tool spec as it appears in `gv.toml`. Two surface forms:
///   gopls = "latest"
///   [tools.gopls]
///   package = "golang.org/x/tools/gopls"
///   version = "v0.18.1"
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolSpec {
    Short(String),
    Long {
        #[serde(default)]
        package: Option<String>,
        version: String,
        /// Override the binary name installed by `go install`. Defaults to the
        /// last path segment of `package` (Go's own rule).
        #[serde(default)]
        bin: Option<String>,
    },
}

impl ToolSpec {
    pub fn version(&self) -> &str {
        match self {
            ToolSpec::Short(v) => v,
            ToolSpec::Long { version, .. } => version,
        }
    }
    pub fn package_override(&self) -> Option<&str> {
        match self {
            ToolSpec::Short(_) => None,
            ToolSpec::Long { package, .. } => package.as_deref(),
        }
    }
    pub fn bin_override(&self) -> Option<&str> {
        match self {
            ToolSpec::Short(_) => None,
            ToolSpec::Long { bin, .. } => bin.as_deref(),
        }
    }
}

/// Walk up from `start` looking for a project root (first dir with `go.mod` or
/// `gv.toml`).
pub fn find_root(start: &Path) -> Option<PathBuf> {
    let mut dir: Option<&Path> = Some(start);
    while let Some(d) = dir {
        if d.join(PROJECT_FILE).is_file() || d.join("go.mod").is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

pub fn load(root: &Path) -> Result<Project> {
    let path = root.join(PROJECT_FILE);
    if !path.is_file() {
        return Ok(Project::default());
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let project: Project =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(project)
}

pub fn save(root: &Path, project: &Project) -> Result<()> {
    let path = root.join(PROJECT_FILE);
    let text =
        toml::to_string_pretty(project).with_context(|| format!("serialize {}", path.display()))?;
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
