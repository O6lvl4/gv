//! Download + verify + extract a Go release into the content-addressed store.

use std::io::{self, Read, Write};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tar::Archive;

use crate::paths::Paths;
use crate::platform::Platform;
use crate::release::{self, Release, ReleaseFile};
use crate::store::Store;

const DL_BASE: &str = "https://go.dev/dl/";

pub struct Installer<'a> {
    pub paths: &'a Paths,
    pub client: &'a reqwest::Client,
    pub platform: Platform,
}

pub struct InstallReport {
    pub version: String,        // canonical "go1.25.0"
    pub sha256: String,
    pub install_dir: std::path::PathBuf,
    pub already_present: bool,
}

impl<'a> Installer<'a> {
    pub async fn install(&self, version: &str) -> Result<InstallReport> {
        let releases = release::fetch_index(self.client).await?;
        let (release, file) = release::select_archive(&releases, version, self.platform)?;
        self.install_file(release, file).await
    }

    pub async fn install_file(&self, release: &Release, file: &ReleaseFile) -> Result<InstallReport> {
        self.paths.ensure_dirs()?;
        let store = Store::new(self.paths);

        if store.has_sha(&file.sha256) {
            store.link_version(&release.version, &file.sha256)?;
            return Ok(InstallReport {
                version: release.version.clone(),
                sha256: file.sha256.clone(),
                install_dir: store.dir_for_sha(&file.sha256),
                already_present: true,
            });
        }

        let tmp_path = self.paths.cache.join(&file.filename);
        crate::paths::ensure_dir(&self.paths.cache)?;

        download_with_sha(self.client, &format!("{DL_BASE}{}", file.filename), &tmp_path, &file.sha256)
            .await
            .with_context(|| format!("download {}", file.filename))?;

        let dest = store.dir_for_sha(&file.sha256);
        if dest.exists() {
            std::fs::remove_dir_all(&dest).ok();
        }
        crate::paths::ensure_dir(&dest)?;

        extract_tar_gz(&tmp_path, &dest)
            .with_context(|| format!("extract {}", file.filename))?;

        // The Go archive contains a top-level `go/` directory; promote it.
        promote_go_subdir(&dest)?;

        store.mark_installed(&file.sha256)?;
        store.link_version(&release.version, &file.sha256)?;

        std::fs::remove_file(&tmp_path).ok();

        Ok(InstallReport {
            version: release.version.clone(),
            sha256: file.sha256.clone(),
            install_dir: dest,
            already_present: false,
        })
    }
}

async fn download_with_sha(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    expected_sha: &str,
) -> Result<()> {
    let res = client.get(url).send().await?.error_for_status()?;
    let mut stream = res.bytes_stream();
    let mut file = std::fs::File::create(dest)?;
    let mut hasher = Sha256::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk)?;
    }
    file.flush()?;

    let got = hex::encode(hasher.finalize());
    if got != expected_sha {
        std::fs::remove_file(dest).ok();
        return Err(anyhow!(
            "sha256 mismatch for {url}: expected {expected_sha}, got {got}"
        ));
    }
    Ok(())
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let f = std::fs::File::open(archive)?;
    let gz = GzDecoder::new(f);
    let mut tar = Archive::new(gz);
    tar.set_preserve_permissions(true);
    tar.set_overwrite(true);
    tar.unpack(dest)?;
    Ok(())
}

/// Go archives contain a top-level `go/` directory. Move its contents up so the
/// installed layout is `<store>/<sha>/{bin,src,pkg,...}` instead of
/// `<store>/<sha>/go/{bin,...}`.
fn promote_go_subdir(dest: &Path) -> Result<()> {
    let inner = dest.join("go");
    if !inner.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&inner)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if to.exists() {
            std::fs::remove_dir_all(&to).ok();
        }
        std::fs::rename(&from, &to)?;
    }
    std::fs::remove_dir(&inner).ok();
    Ok(())
}

#[allow(dead_code)]
fn copy_io<R: Read, W: Write>(mut from: R, mut to: W) -> io::Result<u64> {
    io::copy(&mut from, &mut to)
}
