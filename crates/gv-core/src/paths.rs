//! Standard paths used by gv. Honors XDG; overridable via `GV_HOME`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;

#[derive(Debug, Clone)]
pub struct Paths {
    pub data: PathBuf,    // ~/.local/share/gv
    pub config: PathBuf,  // ~/.config/gv
    pub cache: PathBuf,   // ~/.cache/gv
}

impl Paths {
    pub fn discover() -> Result<Self> {
        if let Ok(home) = std::env::var("GV_HOME") {
            let root = PathBuf::from(home);
            return Ok(Self {
                data: root.join("data"),
                config: root.join("config"),
                cache: root.join("cache"),
            });
        }
        let pd = ProjectDirs::from("dev", "O6lvl4", "gv")
            .context("could not resolve XDG directories for gv")?;
        Ok(Self {
            data: pd.data_dir().to_path_buf(),
            config: pd.config_dir().to_path_buf(),
            cache: pd.cache_dir().to_path_buf(),
        })
    }

    pub fn store(&self) -> PathBuf {
        self.data.join("store")
    }

    pub fn versions(&self) -> PathBuf {
        self.data.join("versions")
    }

    pub fn version_dir(&self, version: &str) -> PathBuf {
        self.versions().join(version)
    }

    pub fn bin(&self) -> PathBuf {
        self.data.join("bin")
    }

    pub fn global_version_file(&self) -> PathBuf {
        self.config.join("global")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for d in [&self.data, &self.config, &self.cache, &self.store(), &self.versions(), &self.bin()] {
            ensure_dir(d)?;
        }
        Ok(())
    }
}

pub fn ensure_dir(p: &Path) -> Result<()> {
    if !p.exists() {
        std::fs::create_dir_all(p).with_context(|| format!("create dir: {}", p.display()))?;
    }
    Ok(())
}
