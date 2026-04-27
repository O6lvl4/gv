//! Parse `go.work` files. Workspaces let a single `gv.toml` + `gv.lock` rule
//! a multi-module repo, with `toolchain` at the workspace root taking
//! precedence over individual members' `go.mod`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const WORKSPACE_FILE: &str = "go.work";

#[derive(Debug, Clone, Default)]
pub struct GoWork {
    /// `toolchain go1.25.0` line, if present.
    pub toolchain: Option<String>,
    /// Module directories listed under `use (...)` (or `use ./path`),
    /// resolved relative to the go.work file.
    pub members: Vec<PathBuf>,
}

/// Walk up from `start`, returning the directory that contains a `go.work`.
pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut dir: Option<&Path> = Some(start);
    while let Some(d) = dir {
        if d.join(WORKSPACE_FILE).is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

pub fn load(root: &Path) -> Result<GoWork> {
    let path = root.join(WORKSPACE_FILE);
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(parse(&raw, root))
}

fn parse(text: &str, root: &Path) -> GoWork {
    let mut out = GoWork::default();
    let mut in_use_block = false;
    for raw_line in text.lines() {
        let mut line = raw_line.trim();
        if let Some(idx) = line.find("//") {
            line = line[..idx].trim();
        }
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("toolchain ") {
            out.toolchain = Some(normalize_toolchain(rest.trim()));
            continue;
        }
        if line.starts_with("use ") {
            // Two surface forms:
            //   use ./foo
            //   use (
            //       ./foo
            //       ./bar
            //   )
            let after = line.trim_start_matches("use").trim();
            if after == "(" {
                in_use_block = true;
                continue;
            }
            if let Some(p) = parse_use_target(after) {
                out.members.push(root.join(p));
            }
            continue;
        }
        if in_use_block {
            if line == ")" {
                in_use_block = false;
                continue;
            }
            if let Some(p) = parse_use_target(line) {
                out.members.push(root.join(p));
            }
        }
    }
    out
}

fn parse_use_target(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip surrounding quotes if any.
    let stripped = trimmed.trim_start_matches('"').trim_end_matches('"').trim();
    if stripped.is_empty() {
        None
    } else {
        Some(stripped)
    }
}

fn normalize_toolchain(v: &str) -> String {
    let v = v.trim();
    if v.starts_with("go") {
        v.to_string()
    } else {
        format!("go{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_block_form() {
        let src = r#"
go 1.21

toolchain go1.25.0

use (
    ./service-a
    ./service-b
    ../shared/lib
)
"#;
        let root = PathBuf::from("/repo");
        let parsed = parse(src, &root);
        assert_eq!(parsed.toolchain.as_deref(), Some("go1.25.0"));
        assert_eq!(
            parsed.members,
            vec![
                PathBuf::from("/repo/./service-a"),
                PathBuf::from("/repo/./service-b"),
                PathBuf::from("/repo/../shared/lib"),
            ]
        );
    }

    #[test]
    fn parse_inline_use() {
        let src = "use ./single\ntoolchain 1.25.0";
        let parsed = parse(src, &PathBuf::from("/r"));
        assert_eq!(parsed.toolchain.as_deref(), Some("go1.25.0"));
        assert_eq!(parsed.members, vec![PathBuf::from("/r/./single")]);
    }
}
