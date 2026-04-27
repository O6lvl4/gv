//! Install and manage Go tools (gopls, golangci-lint, etc.) — replacing
//! `go install` with sha256-pinned, lockfile-tracked installs.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::lock::LockedTool;
use crate::paths::Paths;
use crate::project::ToolSpec;
use crate::{proxy, registry};

#[derive(Debug, Clone)]
pub struct ResolvedTool {
    pub name: String,        // short name in gv.toml, e.g. "gopls"
    pub package: String,     // full Go package path
    pub version: String,     // concrete (e.g. "v0.18.1") — never "latest"
    pub bin: String,         // binary name produced by `go install`
    pub module_hash: String, // ziphash from proxy.golang.org
}

/// Resolve a `gv.toml` ToolSpec to a concrete version + module hash.
pub async fn resolve(
    client: &reqwest::Client,
    name: &str,
    spec: &ToolSpec,
) -> Result<ResolvedTool> {
    let package = spec
        .package_override()
        .map(|s| s.to_string())
        .or_else(|| registry::lookup(name).map(|e| e.package.to_string()))
        .ok_or_else(|| anyhow!(
            "unknown tool '{name}': either pick from the built-in registry or set package = \"...\" in gv.toml"
        ))?;

    let raw = spec.version();
    let (module, version) = match raw {
        "latest" | "*" => {
            let (m, info) = proxy::find_module(client, &package).await?;
            (m, info.version)
        }
        v => (
            resolve_module_for_explicit_version(client, &package).await?,
            v.to_string(),
        ),
    };

    let module_hash = proxy::ziphash(client, &module, &version)
        .await
        .with_context(|| format!("fetch ziphash for {module}@{version}"))?;

    let bin = spec
        .bin_override()
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_binary_name(&package));

    Ok(ResolvedTool {
        name: name.to_string(),
        package,
        version,
        bin,
        module_hash,
    })
}

/// Run `go install package@version` into a temp GOBIN, then move the binary
/// into the gv tool store. Returns the LockedTool record.
pub fn install(
    paths: &Paths,
    go_version: &str, // e.g. "go1.25.0"
    resolved: &ResolvedTool,
) -> Result<LockedTool> {
    let go_bin = paths.version_dir(go_version).join("bin").join("go");
    if !go_bin.exists() {
        bail!(
            "Go toolchain {go_version} is not installed (looked at {})",
            go_bin.display()
        );
    }

    let dest_dir = tool_dir(paths, &resolved.name, &resolved.version);
    let dest_bin = dest_dir.join(&resolved.bin);
    if !dest_bin.exists() {
        crate::paths::ensure_dir(&dest_dir)?;
        let tmp_bin =
            paths
                .cache
                .join(format!("tool-{}-{}.tmp", resolved.name, std::process::id()));
        let tmp_gobin = paths.cache.join(format!(
            "gobin-{}-{}.tmp",
            resolved.name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp_gobin);
        crate::paths::ensure_dir(&tmp_gobin)?;

        let target = format!("{}@{}", resolved.package, resolved.version);
        let status = Command::new(&go_bin)
            .args(["install", &target])
            .env("GOROOT", paths.version_dir(go_version))
            .env("GOTOOLCHAIN", "local")
            .env("GOBIN", &tmp_gobin)
            .env("GOFLAGS", "-mod=mod")
            .status()
            .with_context(|| format!("spawn go install {target}"))?;
        if !status.success() {
            bail!("go install {target} failed (exit {:?})", status.code());
        }

        let produced = tmp_gobin.join(&resolved.bin);
        if !produced.exists() {
            // Fall back: pick the only file in tmp_gobin.
            let entries: Vec<_> = std::fs::read_dir(&tmp_gobin)?
                .filter_map(|e| e.ok())
                .collect();
            if entries.len() == 1 {
                std::fs::rename(entries[0].path(), &tmp_bin)?;
            } else {
                bail!(
                    "go install produced no binary named {} in {}",
                    resolved.bin,
                    tmp_gobin.display()
                );
            }
        } else {
            std::fs::rename(produced, &tmp_bin)?;
        }

        std::fs::rename(&tmp_bin, &dest_bin)
            .with_context(|| format!("move binary to {}", dest_bin.display()))?;
        let _ = std::fs::remove_dir_all(&tmp_gobin);
    }

    let binary_sha256 = sha256_file(&dest_bin)?;

    Ok(LockedTool {
        name: resolved.name.clone(),
        package: resolved.package.clone(),
        version: resolved.version.clone(),
        bin: resolved.bin.clone(),
        module_hash: resolved.module_hash.clone(),
        built_with: go_version.to_string(),
        binary_sha256,
    })
}

pub fn tool_dir(paths: &Paths, name: &str, version: &str) -> PathBuf {
    paths.data.join("tools").join(name).join(version)
}

pub fn tool_bin_path(paths: &Paths, locked: &LockedTool) -> PathBuf {
    tool_dir(paths, &locked.name, &locked.version).join(&locked.bin)
}

/// Walk up `package_path` asking the proxy whether each prefix is a module.
/// Returns the first match. Used when the user pinned an explicit version
/// (so we still need a module path for ziphash, but skip the `@latest` query).
async fn resolve_module_for_explicit_version(
    client: &reqwest::Client,
    package_path: &str,
) -> Result<String> {
    let (module, _) = proxy::find_module(client, package_path).await?;
    Ok(module)
}

fn default_binary_name(package: &str) -> String {
    // `go install` names the binary after the last path segment, ignoring any
    // trailing `/vN` major-version marker. On Windows, an `.exe` suffix is
    // appended.
    let mut last = package.rsplit('/').next().unwrap_or(package);
    if is_major_marker(last) {
        let trimmed = &package[..package.len() - last.len() - 1];
        last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    }
    format!("{}{}", last, std::env::consts::EXE_SUFFIX)
}

fn is_major_marker(s: &str) -> bool {
    s.starts_with('v') && s.len() > 1 && s[1..].chars().all(|c| c.is_ascii_digit())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("hash {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_name_default() {
        let exe = std::env::consts::EXE_SUFFIX;
        assert_eq!(
            default_binary_name("golang.org/x/tools/gopls"),
            format!("gopls{exe}")
        );
        assert_eq!(
            default_binary_name("github.com/foo/bar/cmd/x"),
            format!("x{exe}")
        );
        assert_eq!(
            default_binary_name("github.com/foo/bar/v2"),
            format!("bar{exe}")
        );
        assert_eq!(
            default_binary_name("github.com/goreleaser/goreleaser/v2"),
            format!("goreleaser{exe}")
        );
    }
}
