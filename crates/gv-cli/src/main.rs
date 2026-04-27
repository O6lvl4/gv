use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use gv_core::install::Installer;
use gv_core::manifest::ToolchainSource;
use gv_core::paths::Paths;
use gv_core::platform::Platform;
use gv_core::{release, resolve};

#[derive(Debug, Parser)]
#[command(
    name = "gv",
    version,
    about = "Go version & toolchain manager. uv-grade speed.",
    propagate_version = true,
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Install a Go toolchain (e.g. `gv install 1.25.0`, `gv install latest`).
    Install {
        version: String,
    },
    /// List installed toolchains, or remote ones with --remote.
    List {
        #[arg(long)]
        remote: bool,
    },
    /// Show the version that resolves in the current directory and why.
    Current,
    /// Print the resolved binary path for `go` (or another tool) in this project.
    Which {
        #[arg(default_value = "go")]
        tool: String,
    },
    /// Set the global default version (writes to `~/.config/gv/global`).
    UseGlobal {
        version: String,
    },
    /// Run a command using the resolved toolchain.
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        argv: Vec<String>,
    },
    /// Health check.
    Doctor,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    match rt.block_on(run(cli)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let paths = Paths::discover()?;
    paths.ensure_dirs()?;
    let platform = Platform::detect()?;

    match cli.cmd {
        Cmd::Install { version } => cmd_install(&paths, platform, &version).await,
        Cmd::List { remote } => cmd_list(&paths, platform, remote).await,
        Cmd::Current => cmd_current(&paths),
        Cmd::Which { tool } => cmd_which(&paths, &tool),
        Cmd::UseGlobal { version } => cmd_use_global(&paths, &version),
        Cmd::Run { argv } => cmd_run(&paths, argv),
        Cmd::Doctor => cmd_doctor(&paths, platform),
    }
}

async fn cmd_install(paths: &Paths, platform: Platform, version: &str) -> Result<ExitCode> {
    let client = http_client()?;
    let releases = release::fetch_index(&client).await?;

    let resolved = if version == "latest" {
        release::latest_stable(&releases)
            .ok_or_else(|| anyhow!("no stable Go release found"))?
            .version
            .clone()
    } else {
        release::normalize_version(version)
    };

    println!("→ installing {resolved} for {}", platform.release_suffix());
    let installer = Installer { paths, client: &client, platform };
    let report = installer.install(&resolved).await?;

    if report.already_present {
        println!("✓ {} already in store ({})", report.version, &report.sha256[..12]);
    } else {
        println!("✓ installed {} ({})", report.version, &report.sha256[..12]);
    }
    println!("  → {}", report.install_dir.display());
    println!("  → linked: {}", paths.version_dir(&report.version).display());
    Ok(ExitCode::SUCCESS)
}

async fn cmd_list(paths: &Paths, platform: Platform, remote: bool) -> Result<ExitCode> {
    if remote {
        let client = http_client()?;
        let releases = release::fetch_index(&client).await?;
        for r in releases.iter() {
            let stable = if r.stable { "stable" } else { "  -   " };
            println!("{stable}  {}", r.version);
        }
    } else {
        let installed = resolve::list_installed(paths)?;
        if installed.is_empty() {
            println!("(no toolchains installed; try `gv install latest`)");
        } else {
            for v in installed {
                println!("{v}");
            }
        }
        let _ = platform; // unused in local mode
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_current(paths: &Paths) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    match resolve::resolve(paths, &cwd)? {
        Some(r) => {
            println!("{}", r.version);
            let why = match r.source {
                ToolchainSource::EnvVar => "GV_VERSION".to_string(),
                ToolchainSource::GoMod => format!(
                    "go.mod toolchain ({})",
                    r.origin.as_deref().map(display_path).unwrap_or_default()
                ),
                ToolchainSource::GoVersionFile => format!(
                    ".go-version ({})",
                    r.origin.as_deref().map(display_path).unwrap_or_default()
                ),
                ToolchainSource::Global => "global".to_string(),
                ToolchainSource::LatestInstalled => "latest installed".to_string(),
            };
            println!("  source: {why}");
            Ok(ExitCode::SUCCESS)
        }
        None => {
            println!("(no version resolved; run `gv install <version>`)");
            Ok(ExitCode::from(2))
        }
    }
}

fn cmd_which(paths: &Paths, tool: &str) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let r = resolve::resolve(paths, &cwd)?
        .ok_or_else(|| anyhow!("no Go version resolved in {}", cwd.display()))?;
    let bin = paths.version_dir(&r.version).join("bin").join(tool);
    if !bin.exists() {
        return Err(anyhow!(
            "{} not found in {} (is the toolchain installed?)",
            tool,
            paths.version_dir(&r.version).display()
        ));
    }
    println!("{}", bin.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_use_global(paths: &Paths, version: &str) -> Result<ExitCode> {
    let canonical = release::normalize_version(version);
    std::fs::write(paths.global_version_file(), &canonical)
        .with_context(|| format!("write {}", paths.global_version_file().display()))?;
    println!("✓ global → {canonical}");
    Ok(ExitCode::SUCCESS)
}

fn cmd_run(paths: &Paths, argv: Vec<String>) -> Result<ExitCode> {
    if argv.is_empty() {
        return Err(anyhow!("usage: gv run <cmd> [args...]"));
    }
    let cwd = std::env::current_dir()?;
    let r = resolve::resolve(paths, &cwd)?
        .ok_or_else(|| anyhow!("no Go version resolved in {}", cwd.display()))?;
    let version_dir = paths.version_dir(&r.version);
    let bin_dir = version_dir.join("bin");
    let cmd = &argv[0];
    let candidate = bin_dir.join(cmd);
    let exe: PathBuf = if candidate.exists() { candidate } else { PathBuf::from(cmd) };

    use std::process::Command;
    let mut child = Command::new(&exe);
    child.args(&argv[1..]);
    // Prepend our toolchain bin to PATH so spawned processes also resolve correctly.
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = std::ffi::OsString::from(bin_dir.as_os_str());
    new_path.push(":");
    new_path.push(&path);
    child.env("PATH", new_path);
    child.env("GOROOT", &version_dir);
    child.env("GOTOOLCHAIN", "local");

    let status = child.status().with_context(|| format!("spawn {}", exe.display()))?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

fn cmd_doctor(paths: &Paths, platform: Platform) -> Result<ExitCode> {
    println!("gv doctor");
    println!("  platform   : {}", platform.release_suffix());
    println!("  data dir   : {}", paths.data.display());
    println!("  config dir : {}", paths.config.display());
    println!("  cache dir  : {}", paths.cache.display());
    let installed = resolve::list_installed(paths)?;
    println!("  installed  : {} version(s)", installed.len());
    for v in installed.iter().take(8) {
        println!("    - {v}");
    }
    let cwd = std::env::current_dir()?;
    match resolve::resolve(paths, &cwd)? {
        Some(r) => println!("  resolved   : {} (from {:?})", r.version, r.source),
        None => println!("  resolved   : (none)"),
    }
    Ok(ExitCode::SUCCESS)
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("gv/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

fn display_path(p: &std::path::Path) -> String {
    p.display().to_string()
}
