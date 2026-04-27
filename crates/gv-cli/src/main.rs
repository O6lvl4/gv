use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use gv_core::install::Installer;
use gv_core::lock::Lock;
use gv_core::manifest::ToolchainSource;
use gv_core::paths::Paths;
use gv_core::platform::Platform;
use gv_core::project::{self, ToolSpec};
use gv_core::{release, resolve, tool};

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
    /// Print the resolved binary path for a tool (or `go`) in this project.
    Which {
        #[arg(default_value = "go")]
        tool: String,
    },
    /// Set the global default version (writes to `~/.config/gv/global`).
    UseGlobal {
        version: String,
    },
    /// Run a command using the resolved toolchain (or a pinned tool).
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        argv: Vec<String>,
    },
    /// Add a tool pin to gv.toml and install it.
    /// Format: `gv add tool gopls` or `gv add tool gopls@v0.18.1`.
    Add {
        #[command(subcommand)]
        target: AddCmd,
    },
    /// Reconcile installs with gv.toml / gv.lock.
    Sync {
        /// Refuse to update gv.lock; install exactly what is locked.
        #[arg(long)]
        frozen: bool,
    },
    /// Health check.
    Doctor,
}

#[derive(Debug, Subcommand)]
enum AddCmd {
    /// Pin a tool. Use `name` (registry lookup) or `name@version`.
    Tool {
        spec: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
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
        Cmd::Add { target } => match target {
            AddCmd::Tool { spec } => cmd_add_tool(&paths, &spec).await,
        },
        Cmd::Sync { frozen } => cmd_sync(&paths, platform, frozen).await,
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

async fn cmd_list(paths: &Paths, _platform: Platform, remote: bool) -> Result<ExitCode> {
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

fn cmd_which(paths: &Paths, name: &str) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;

    if let Some((bin, locked_version)) = lookup_project_tool(paths, &cwd, name)? {
        println!("{}", bin.display());
        let _ = locked_version;
        return Ok(ExitCode::SUCCESS);
    }

    let r = resolve::resolve(paths, &cwd)?
        .ok_or_else(|| anyhow!("no Go version resolved in {}", cwd.display()))?;
    let bin = paths.version_dir(&r.version).join("bin").join(name);
    if !bin.exists() {
        return Err(anyhow!(
            "{} not found in {} (is the toolchain installed?)",
            name,
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
    let cmd = &argv[0];

    // Tool first: if `gv.lock` pins this name, prefer it.
    let exe: Option<PathBuf> = lookup_project_tool(paths, &cwd, cmd)?.map(|(p, _)| p);

    let r = resolve::resolve(paths, &cwd)?;
    let (exe, version_dir) = match (exe, r.as_ref()) {
        (Some(p), Some(r)) => (p, paths.version_dir(&r.version)),
        (None, Some(r)) => {
            let toolchain_bin = paths.version_dir(&r.version).join("bin").join(cmd);
            let exe = if toolchain_bin.exists() { toolchain_bin } else { PathBuf::from(cmd) };
            (exe, paths.version_dir(&r.version))
        }
        (Some(p), None) => (p, PathBuf::new()),
        (None, None) => bail!("no Go version resolved in {}", cwd.display()),
    };

    use std::process::Command;
    let mut child = Command::new(&exe);
    child.args(&argv[1..]);

    let bin_dir = if version_dir.as_os_str().is_empty() {
        PathBuf::new()
    } else {
        version_dir.join("bin")
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = std::ffi::OsString::new();
    if !bin_dir.as_os_str().is_empty() {
        new_path.push(bin_dir.as_os_str());
        new_path.push(":");
    }
    new_path.push(&path);
    child.env("PATH", new_path);
    if !version_dir.as_os_str().is_empty() {
        child.env("GOROOT", &version_dir);
    }
    child.env("GOTOOLCHAIN", "local");

    let status = child.status().with_context(|| format!("spawn {}", exe.display()))?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

async fn cmd_add_tool(paths: &Paths, spec: &str) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let root = project::find_root(&cwd)
        .ok_or_else(|| anyhow!("no project root found (need a go.mod or gv.toml above {})", cwd.display()))?;

    let (name, version) = parse_tool_spec(spec);
    let mut proj = project::load(&root)?;
    proj.tools.insert(
        name.clone(),
        ToolSpec::Short(version.unwrap_or_else(|| "latest".to_string())),
    );
    project::save(&root, &proj)?;
    println!("✓ pinned {name} in {}", root.join("gv.toml").display());

    sync_project(paths, &root, false).await?;
    Ok(ExitCode::SUCCESS)
}

async fn cmd_sync(paths: &Paths, _platform: Platform, frozen: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let root = project::find_root(&cwd)
        .ok_or_else(|| anyhow!("no project root found (need a go.mod or gv.toml above {})", cwd.display()))?;
    sync_project(paths, &root, frozen).await?;
    Ok(ExitCode::SUCCESS)
}

async fn sync_project(paths: &Paths, root: &Path, frozen: bool) -> Result<()> {
    let proj = project::load(root)?;
    let mut lock = Lock::load(root)?;

    // Step 1 — Go toolchain. Honor go.mod, fall back to gv.toml.
    let resolved = resolve::resolve(paths, root)?;
    let go_version = match resolved {
        Some(r) => r.version,
        None => proj
            .go
            .as_ref()
            .map(|g| release::normalize_version(&g.version))
            .ok_or_else(|| anyhow!(
                "no Go version is resolvable here. Set `toolchain` in go.mod, write .go-version, or set `[go] version = \"...\"` in gv.toml"
            ))?,
    };

    let go_dir = paths.version_dir(&go_version);
    let go_sha256: String = if !go_dir.join("bin").join("go").exists() {
        let client = http_client()?;
        let installer = Installer { paths, client: &client, platform: Platform::detect()? };
        println!("→ installing {go_version} (required by project)");
        installer.install(&go_version).await?.sha256
    } else {
        println!("✓ {go_version} present");
        // Recover sha from the existing store layout: store dir name = sha256[..16].
        let target = std::fs::read_link(&go_dir).ok();
        target
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_default()
    };

    // Step 2 — tools.
    if proj.tools.is_empty() {
        println!("(no tools to sync)");
        lock.save(root)?;
        return Ok(());
    }

    let client = http_client()?;
    for (name, spec) in &proj.tools {
        let resolved = if frozen {
            let l = lock.find_tool(name).ok_or_else(|| anyhow!(
                "frozen sync: tool '{name}' is in gv.toml but not in gv.lock"
            ))?;
            tool::ResolvedTool {
                name: l.name.clone(),
                package: l.package.clone(),
                version: l.version.clone(),
                bin: l.bin.clone(),
                module_hash: l.module_hash.clone(),
            }
        } else {
            tool::resolve(&client, name, spec).await?
        };

        let installed = tool::install(paths, &go_version, &resolved)?;
        let already = lock.find_tool(&installed.name).map(|l| l.binary_sha256.clone());
        let new_sha = installed.binary_sha256.clone();
        lock.upsert_tool(installed);
        let bin = tool::tool_bin_path(paths, lock.find_tool(name).unwrap());
        let suffix = if already.as_deref() == Some(new_sha.as_str()) { "(unchanged)" } else { "(updated)" };
        println!(
            "✓ {name}@{ver} {suffix}\n  → {bin}",
            ver = resolved.version,
            bin = bin.display()
        );
    }

    lock.go = Some(gv_core::lock::LockedGo {
        version: go_version.clone(),
        sha256: go_sha256,
    });

    if frozen {
        // Don't re-write the lock in frozen mode (it should already match).
    } else {
        lock.save(root)?;
        println!("✓ wrote {}", root.join("gv.lock").display());
    }
    Ok(())
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
    if let Some(root) = project::find_root(&cwd) {
        println!("  project    : {}", root.display());
        let lock = Lock::load(&root)?;
        if !lock.tools.is_empty() {
            println!("  pinned tools:");
            for t in &lock.tools {
                println!("    - {}@{} (built with {})", t.name, t.version, t.built_with);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("gv/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

fn display_path(p: &Path) -> String {
    p.display().to_string()
}

fn parse_tool_spec(spec: &str) -> (String, Option<String>) {
    if let Some((name, version)) = spec.rsplit_once('@') {
        (name.to_string(), Some(version.to_string()))
    } else {
        (spec.to_string(), None)
    }
}

/// If a project lock pins this name, return (binary_path, version).
fn lookup_project_tool(
    paths: &Paths,
    cwd: &Path,
    name: &str,
) -> Result<Option<(PathBuf, String)>> {
    let Some(root) = project::find_root(cwd) else { return Ok(None); };
    let lock = Lock::load(&root)?;
    let Some(t) = lock.find_tool(name) else { return Ok(None); };
    let bin = tool::tool_bin_path(paths, t);
    if bin.exists() {
        Ok(Some((bin, t.version.clone())))
    } else {
        Ok(None)
    }
}
