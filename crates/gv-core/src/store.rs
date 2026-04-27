//! Content-addressed store. Each archive is unpacked into
//! `<store>/<sha256-prefix>/` and `<versions>/<version>` is a symlink to it.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths::Paths;

pub struct Store<'a> {
    paths: &'a Paths,
}

impl<'a> Store<'a> {
    pub fn new(paths: &'a Paths) -> Self {
        Self { paths }
    }

    pub fn dir_for_sha(&self, sha256: &str) -> PathBuf {
        // Use the first 16 hex chars to keep paths short while collision-safe in practice.
        self.paths.store().join(&sha256[..16])
    }

    pub fn version_link(&self, version: &str) -> PathBuf {
        self.paths.version_dir(version)
    }

    pub fn link_version(&self, version: &str, sha256: &str) -> Result<PathBuf> {
        let target = self.dir_for_sha(sha256);
        let link = self.version_link(version);
        if link.exists() || link.is_symlink() {
            fs::remove_file(&link).ok();
        }
        if let Some(parent) = link.parent() {
            crate::paths::ensure_dir(parent)?;
        }
        symlink(&target, &link)
            .with_context(|| format!("link {} -> {}", link.display(), target.display()))?;
        Ok(link)
    }

    pub fn has_sha(&self, sha256: &str) -> bool {
        self.dir_for_sha(sha256).join(".gv-installed").exists()
    }

    pub fn mark_installed(&self, sha256: &str) -> Result<()> {
        let dir = self.dir_for_sha(sha256);
        crate::paths::ensure_dir(&dir)?;
        fs::write(dir.join(".gv-installed"), sha256)?;
        Ok(())
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}
