//! Detect the host platform for matching Go release artifacts.

use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os { Darwin, Linux }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch { Amd64, Arm64 }

#[derive(Debug, Clone, Copy)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

impl Platform {
    pub fn detect() -> Result<Self> {
        let os = match std::env::consts::OS {
            "macos" => Os::Darwin,
            "linux" => Os::Linux,
            other => bail!("unsupported OS: {other}"),
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => Arch::Amd64,
            "aarch64" => Arch::Arm64,
            other => bail!("unsupported arch: {other}"),
        };
        Ok(Self { os, arch })
    }

    /// Component used in Go release filenames, e.g. `darwin-arm64`.
    pub fn release_suffix(&self) -> &'static str {
        match (self.os, self.arch) {
            (Os::Darwin, Arch::Arm64) => "darwin-arm64",
            (Os::Darwin, Arch::Amd64) => "darwin-amd64",
            (Os::Linux,  Arch::Arm64) => "linux-arm64",
            (Os::Linux,  Arch::Amd64) => "linux-amd64",
        }
    }
}
