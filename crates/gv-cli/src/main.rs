use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use futures::future::try_join_all;
use gv_core::install::Installer;
use gv_core::lock::{Lock, LockedTool};
use gv_core::manifest::ToolchainSource;
use gv_core::paths::Paths;
use gv_core::platform::Platform;
use gv_core::project::{self, ToolSpec};
use gv_core::{release, resolve, tool};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use sha2::Digest;

#[derive(Debug, Parser)]
#[command(
    name = "gv",
    version,
    about = "Go version & toolchain manager. uv-grade speed.",
    propagate_version = true
)]
struct Cli {
    /// Suppress non-essential output (spinners, summaries, hints).
    /// Real errors and explicit query results still print.
    #[arg(short = 'q', long = "quiet", global = true)]
    quiet: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Install a Go toolchain (e.g. `gv install 1.25.0`, `gv install latest`).
    Install { version: String },
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
    UseGlobal { version: String },
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
    /// Install symlinks so `go`, `gofmt`, etc. dispatch through gv-shim.
    Link {
        /// Where to create the symlinks (defaults to ~/.local/bin).
        #[arg(long)]
        bin_dir: Option<PathBuf>,
        /// Path to the gv-shim binary (defaults to alongside `gv`).
        #[arg(long)]
        shim: Option<PathBuf>,
        /// Tool names to link. Defaults to the standard Go toolchain set.
        #[arg(long, value_delimiter = ',')]
        tools: Option<Vec<String>>,
        /// Replace existing files even if they aren't symlinks.
        #[arg(long)]
        force: bool,
    },
    /// Remove symlinks created by `gv link`.
    Unlink {
        #[arg(long)]
        bin_dir: Option<PathBuf>,
        #[arg(long, value_delimiter = ',')]
        tools: Option<Vec<String>>,
    },
    /// Initialize gv.toml in the current directory.
    Init {
        /// Comma-separated tool names to preselect (e.g. `gopls,golangci-lint`).
        #[arg(long, value_delimiter = ',')]
        with: Option<Vec<String>>,
        /// Override the toolchain pin (default: read go.mod / .go-version,
        /// or fall back to the latest stable Go release).
        #[arg(long)]
        go: Option<String>,
        /// Overwrite an existing gv.toml.
        #[arg(long)]
        force: bool,
    },
    /// Update gv to the latest release.
    SelfUpdate {
        /// Only check for a newer release; don't install.
        #[arg(long)]
        check: bool,
    },
    /// Manage Go tools pinned in this project.
    Tool {
        #[command(subcommand)]
        op: ToolCmd,
    },
    /// Run a tool ephemerally without pinning it in gv.toml. Resolves and
    /// installs (or reuses a cached install), then execs. Same backing
    /// content-addressed store as `gv tool add`, just no project state
    /// touched. The `gvx` shim dispatches into this command.
    X {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
        argv: Vec<String>,
    },
    /// Remove a Go toolchain (drops the versions/<v> link; the store dir
    /// is reclaimed by `gv cache prune`).
    Uninstall { version: String },
    /// Re-resolve gv.toml against the proxy and rewrite gv.lock without
    /// installing anything.
    Lock,
    /// Print resolved environment as a tree.
    Tree {
        /// Also list direct go.mod dependencies.
        #[arg(long)]
        deps: bool,
    },
    /// Re-resolve pinned tools (and optionally the toolchain) to their latest
    /// matching versions. Updates gv.lock and re-installs anything that moved.
    Upgrade {
        /// Specific tool name(s) to upgrade. Default: all pinned tools.
        names: Vec<String>,
        /// Also upgrade the Go toolchain to `latest`.
        #[arg(long)]
        toolchain: bool,
    },
    /// Inspect or prune the gv data directories.
    Cache {
        #[command(subcommand)]
        op: CacheCmd,
    },
    /// Show pinned tools / toolchain that are behind their latest available
    /// versions, without modifying anything. Exits non-zero if anything is
    /// behind (suitable for CI gating).
    Outdated,
    /// Print shell-evaluable exports for the resolved toolchain. Useful when
    /// a tool needs to run *outside* `gv run`. e.g. `eval "$(gv env)"`.
    Env {
        /// Output dialect.
        #[arg(long, value_enum, default_value_t = EnvShell::Sh)]
        shell: EnvShell,
    },
    /// Import tools from a `tools.go` (with `//go:build tools`) and pin them
    /// in gv.toml. Run `gv sync` afterwards to install.
    MigrateTools {
        /// Path to the tools.go file. Defaults to scanning the project root
        /// for any file with the `//go:build tools` constraint.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Don't write gv.toml, just print what would be added.
        #[arg(long)]
        dry_run: bool,
    },
    /// Generate shell completions for `gv`.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Print the path of a gv-managed directory (for shell substitution,
    /// e.g. `cd "$(gv dir tools)"`).
    Dir {
        #[arg(value_enum)]
        kind: DirKind,
    },
    /// Health check.
    Doctor,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum DirKind {
    /// Top-level data directory (`~/.local/share/gv`).
    Data,
    /// Cache directory.
    Cache,
    /// Config directory.
    Config,
    /// Content-addressed toolchain store.
    Store,
    /// Toolchain symlink farm.
    Versions,
    /// Per-tool store (`~/.local/share/gv/tools`).
    Tools,
}

#[derive(Debug, Subcommand)]
enum CacheCmd {
    /// Show disk usage by category.
    Info,
    /// Remove store entries no longer referenced.
    Prune {
        /// Show what would be removed without doing it.
        #[arg(long)]
        dry_run: bool,
        /// Also wipe the Go build cache (GOCACHE). Go re-creates it lazily.
        #[arg(long)]
        go_cache: bool,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum EnvShell {
    Sh,
    Fish,
    Powershell,
}

#[derive(Debug, Subcommand)]
enum ToolCmd {
    /// List tools pinned in the current project.
    #[command(visible_alias = "ls")]
    List,
    /// Print the built-in tool registry (name → package).
    Registry,
    /// Pin a tool. Same as `gv add tool NAME[@VERSION]`.
    Add { spec: String },
    /// Remove a tool from gv.toml and gv.lock (binary stays in store
    /// until `gv cache prune`).
    Remove { name: String },
}

const STD_GO_TOOLS: &[&str] = &["go", "gofmt"];

#[derive(Debug, Subcommand)]
enum AddCmd {
    /// Pin a tool. Use `name` (registry lookup) or `name@version`.
    Tool { spec: String },
}

/// Global verbosity gate. Set once from `--quiet`; helpers consult it.
static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn quiet() -> bool {
    QUIET.load(std::sync::atomic::Ordering::Relaxed)
}

/// `say!` works like `println!` but stays silent under `--quiet`.
macro_rules! say {
    ($($arg:tt)*) => {{
        if !quiet() {
            println!($($arg)*);
        }
    }};
}

fn main() -> ExitCode {
    // argv[0]-based dispatch: when the binary is invoked as `gvx` (symlink or
    // copy), inject `x` as the first positional so users don't have to type
    // `gv x …`. Mirrors uv's `uvx` shim.
    let argv0_basename = std::env::args_os()
        .next()
        .and_then(|p| {
            Path::new(&p)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let cli = if argv0_basename == "gvx" {
        let injected = std::iter::once("gv".to_string())
            .chain(std::iter::once("x".to_string()))
            .chain(std::env::args().skip(1));
        Cli::parse_from(injected)
    } else {
        Cli::parse()
    };
    QUIET.store(cli.quiet, std::sync::atomic::Ordering::Relaxed);
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
        Cmd::Link {
            bin_dir,
            shim,
            tools,
            force,
        } => cmd_link(bin_dir, shim, tools, force),
        Cmd::Unlink { bin_dir, tools } => cmd_unlink(bin_dir, tools),
        Cmd::Init { with, go, force } => cmd_init(with, go, force).await,
        Cmd::SelfUpdate { check } => cmd_self_update(platform, check).await,
        Cmd::Tool { op } => match op {
            ToolCmd::List => cmd_tool_list(&paths),
            ToolCmd::Registry => cmd_tool_registry(),
            ToolCmd::Add { spec } => cmd_add_tool(&paths, &spec).await,
            ToolCmd::Remove { name } => cmd_tool_remove(&paths, &name),
        },
        Cmd::X { argv } => cmd_x(&paths, argv).await,
        Cmd::Uninstall { version } => cmd_uninstall(&paths, &version),
        Cmd::Lock => cmd_lock(&paths).await,
        Cmd::Tree { deps } => cmd_tree(&paths, deps),
        Cmd::Upgrade { names, toolchain } => cmd_upgrade(&paths, platform, names, toolchain).await,
        Cmd::Cache { op } => match op {
            CacheCmd::Info => cmd_cache_info(&paths),
            CacheCmd::Prune { dry_run, go_cache } => cmd_cache_prune(&paths, dry_run, go_cache),
        },
        Cmd::Dir { kind } => cmd_dir(&paths, kind),
        Cmd::Outdated => cmd_outdated(&paths, platform).await,
        Cmd::Env { shell } => cmd_env(&paths, shell),
        Cmd::MigrateTools { from, dry_run } => cmd_migrate_tools(from, dry_run).await,
        Cmd::Completions { shell } => cmd_completions(shell),
        Cmd::Doctor => cmd_doctor(&paths, platform),
    }
}

fn cmd_completions(shell: clap_complete::Shell) -> Result<ExitCode> {
    let mut cmd = <Cli as clap::CommandFactory>::command();
    let bin_name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
    Ok(ExitCode::SUCCESS)
}

fn default_bin_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local").join("bin"))
}

fn default_shim_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locate gv binary")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("gv binary has no parent dir"))?;
    let shim = dir.join("gv-shim");
    if !shim.exists() {
        bail!(
            "gv-shim not found next to gv ({}). Pass --shim explicitly.",
            shim.display()
        );
    }
    Ok(shim)
}

fn cmd_link(
    bin_dir: Option<PathBuf>,
    shim: Option<PathBuf>,
    tools: Option<Vec<String>>,
    force: bool,
) -> Result<ExitCode> {
    let bin_dir = bin_dir.map(Result::Ok).unwrap_or_else(default_bin_dir)?;
    let shim = shim.map(Result::Ok).unwrap_or_else(default_shim_path)?;
    let tools: Vec<String> =
        tools.unwrap_or_else(|| STD_GO_TOOLS.iter().map(|s| s.to_string()).collect());

    if !shim.exists() {
        bail!("shim binary not found at {}", shim.display());
    }
    std::fs::create_dir_all(&bin_dir).with_context(|| format!("create {}", bin_dir.display()))?;

    for raw in &tools {
        let name = if cfg!(windows) && !raw.ends_with(".exe") {
            format!("{raw}.exe")
        } else {
            raw.clone()
        };
        let dest = bin_dir.join(&name);
        let marker = bin_dir.join(format!(".{name}.gv-managed"));
        if dest.exists() || dest.is_symlink() {
            if !force {
                let owned_by_us = if cfg!(windows) {
                    marker.exists()
                } else {
                    std::fs::read_link(&dest)
                        .map(|p| p == shim)
                        .unwrap_or(false)
                };
                if owned_by_us {
                    println!("✓ {name} already managed by gv");
                    continue;
                } else {
                    println!(
                        "! {name} exists at {} and is not managed by gv — skipping (use --force)",
                        dest.display()
                    );
                    continue;
                }
            }
            std::fs::remove_file(&dest).ok();
            let _ = std::fs::remove_file(&marker);
        }
        if cfg!(windows) {
            std::fs::copy(&shim, &dest)
                .with_context(|| format!("copy {} -> {}", shim.display(), dest.display()))?;
            std::fs::write(&marker, "")
                .with_context(|| format!("write marker {}", marker.display()))?;
            println!("✓ installed {name} (copy of gv-shim)");
        } else {
            symlink(&shim, &dest)
                .with_context(|| format!("link {} -> {}", dest.display(), shim.display()))?;
            println!("✓ linked {name} → gv-shim");
        }
    }

    if !path_contains(&bin_dir) {
        println!();
        println!(
            "note: {} is not on $PATH. Add this to your shell rc:",
            bin_dir.display()
        );
        println!("  export PATH=\"{}:$PATH\"", bin_dir.display());
    }

    Ok(ExitCode::SUCCESS)
}

fn cmd_unlink(bin_dir: Option<PathBuf>, tools: Option<Vec<String>>) -> Result<ExitCode> {
    let bin_dir = bin_dir.map(Result::Ok).unwrap_or_else(default_bin_dir)?;
    let tools: Vec<String> =
        tools.unwrap_or_else(|| STD_GO_TOOLS.iter().map(|s| s.to_string()).collect());

    for raw in &tools {
        let name = if cfg!(windows) && !raw.ends_with(".exe") {
            format!("{raw}.exe")
        } else {
            raw.clone()
        };
        let dest = bin_dir.join(&name);
        let marker = bin_dir.join(format!(".{name}.gv-managed"));
        if !dest.exists() && !dest.is_symlink() {
            continue;
        }
        let owned_by_us = if cfg!(windows) {
            marker.exists()
        } else {
            dest.is_symlink()
        };
        if owned_by_us {
            std::fs::remove_file(&dest).with_context(|| format!("remove {}", dest.display()))?;
            let _ = std::fs::remove_file(&marker);
            println!("✓ unlinked {name}");
        } else {
            println!(
                "! {name} at {} is not managed by gv — leaving it",
                dest.display()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

fn path_contains(dir: &Path) -> bool {
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    std::env::split_paths(&path).any(|p| p == dir)
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
    let installer = Installer {
        paths,
        client: &client,
        platform,
    };
    let report = installer.install(&resolved).await?;

    if report.already_present {
        println!(
            "✓ {} already in store ({})",
            report.version,
            &report.sha256[..12]
        );
    } else {
        println!("✓ installed {} ({})", report.version, &report.sha256[..12]);
    }
    println!("  → {}", report.install_dir.display());
    println!(
        "  → linked: {}",
        paths.version_dir(&report.version).display()
    );
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
                ToolchainSource::GoWork => format!(
                    "go.work toolchain ({})",
                    r.origin.as_deref().map(display_path).unwrap_or_default()
                ),
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
    let bin = find_toolchain_binary(&paths.version_dir(&r.version).join("bin"), name).ok_or_else(
        || {
            anyhow!(
                "{} not found in {} (is the toolchain installed?)",
                name,
                paths.version_dir(&r.version).display()
            )
        },
    )?;
    println!("{}", bin.display());
    Ok(ExitCode::SUCCESS)
}

/// Look up an executable in `bin_dir` honoring the host's executable suffix.
/// Tries `<name>` first, then `<name>{EXE_SUFFIX}` so users can type `go`
/// even when the installed binary is `go.exe`.
fn find_toolchain_binary(bin_dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = bin_dir.join(name);
    if direct.exists() {
        return Some(direct);
    }
    let exe_suffix = std::env::consts::EXE_SUFFIX;
    if !exe_suffix.is_empty() && !name.ends_with(exe_suffix) {
        let with_suffix = bin_dir.join(format!("{name}{exe_suffix}"));
        if with_suffix.exists() {
            return Some(with_suffix);
        }
    }
    None
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
            let bin_dir = paths.version_dir(&r.version).join("bin");
            let exe = find_toolchain_binary(&bin_dir, cmd).unwrap_or_else(|| PathBuf::from(cmd));
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

    let status = child
        .status()
        .with_context(|| format!("spawn {}", exe.display()))?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

async fn cmd_add_tool(paths: &Paths, spec: &str) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let root = project::find_root(&cwd).ok_or_else(|| {
        anyhow!(
            "no project root found (need a go.mod or gv.toml above {})",
            cwd.display()
        )
    })?;

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
    let root = project::find_root(&cwd).ok_or_else(|| {
        anyhow!(
            "no project root found (need a go.mod or gv.toml above {})",
            cwd.display()
        )
    })?;
    sync_project(paths, &root, frozen).await?;
    Ok(ExitCode::SUCCESS)
}

async fn sync_project(paths: &Paths, root: &Path, frozen: bool) -> Result<()> {
    let proj = project::load(root)?;
    let mut lock = Lock::load(root)?;

    // Step 1 — Go toolchain. Honor go.mod, fall back to gv.toml.
    let resolved_toolchain = resolve::resolve(paths, root)?;
    let go_version = match resolved_toolchain {
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
        let installer = Installer {
            paths,
            client: &client,
            platform: Platform::detect()?,
        };
        let pb = spinner(&format!("installing {go_version}"));
        let sha = installer.install(&go_version).await?.sha256;
        pb.finish_with_message(format!("installed {go_version}"));
        sha
    } else {
        // Recover sha from the existing store layout: store dir name = sha256[..16].
        let target = std::fs::read_link(&go_dir).ok();
        let recovered = target
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_default();
        say!(
            "{} {go_version} {}",
            success_mark(),
            dim("(already present)")
        );
        recovered
    };

    // Step 2 — tools.
    if proj.tools.is_empty() {
        if !frozen {
            lock.go = Some(gv_core::lock::LockedGo {
                version: go_version.clone(),
                sha256: go_sha256,
            });
            lock.save(root)?;
        }
        println!("{}", dim("(no tools to sync)"));
        return Ok(());
    }

    let client = http_client()?;
    let mp = MultiProgress::new();

    // -- resolve all tools in parallel ---------------------------------------
    let resolve_started = Instant::now();
    let resolve_futs = proj.tools.iter().map(|(name, spec)| {
        let client = client.clone();
        let mp = mp.clone();
        let lock_ref = &lock;
        let name = name.clone();
        let spec = spec.clone();
        async move {
            let pb = mp.add(spinner(&format!("resolving {name}")));
            let resolved = if frozen {
                let l = lock_ref.find_tool(&name).ok_or_else(|| {
                    anyhow!("frozen sync: tool '{name}' is in gv.toml but not in gv.lock")
                })?;
                tool::ResolvedTool {
                    name: l.name.clone(),
                    package: l.package.clone(),
                    version: l.version.clone(),
                    bin: l.bin.clone(),
                    module_hash: l.module_hash.clone(),
                }
            } else {
                tool::resolve(&client, &name, &spec).await?
            };
            pb.finish_and_clear();
            Ok::<_, anyhow::Error>(resolved)
        }
    });
    let resolved: Vec<tool::ResolvedTool> = try_join_all(resolve_futs).await?;
    let resolve_ms = resolve_started.elapsed().as_millis();
    say!(
        "{} Resolved {} tool{} in {}",
        success_mark(),
        resolved.len(),
        plural(resolved.len()),
        format_duration(resolve_ms)
    );

    // -- install all tools in parallel ---------------------------------------
    let install_started = Instant::now();
    let install_futs = resolved.iter().map(|r| {
        let mp = mp.clone();
        let paths = paths.clone();
        let go_version = go_version.clone();
        let r = r.clone();
        async move {
            let pb = mp.add(spinner(&format!("building {}@{}", r.name, r.version)));
            let res = tokio::task::spawn_blocking(move || tool::install(&paths, &go_version, &r))
                .await
                .map_err(|e| anyhow!("install task panicked: {e}"))??;
            pb.finish_and_clear();
            Ok::<_, anyhow::Error>(res)
        }
    });
    let installed: Vec<LockedTool> = try_join_all(install_futs).await?;
    let install_ms = install_started.elapsed().as_millis();

    // -- merge into lock + summarize -----------------------------------------
    let mut summary: Vec<(String, String, char)> = Vec::with_capacity(installed.len());
    for new in installed {
        let prev_sha = lock.find_tool(&new.name).map(|l| l.binary_sha256.clone());
        let prev_ver = lock.find_tool(&new.name).map(|l| l.version.clone());
        let mark = match (prev_sha, prev_ver) {
            (None, _) => '+',
            (_, Some(v)) if v != new.version => '~',
            _ => '=',
        };
        summary.push((new.name.clone(), new.version.clone(), mark));
        lock.upsert_tool(new);
    }
    summary.sort();

    say!(
        "{} Built {} tool{} in {}",
        success_mark(),
        summary.len(),
        plural(summary.len()),
        format_duration(install_ms)
    );
    for (name, version, mark) in &summary {
        let glyph = match mark {
            '+' => format!(" {}", color_green("+")),
            '~' => format!(" {}", color_yellow("~")),
            _ => format!(" {}", dim("=")),
        };
        let detail = match mark {
            '+' => format!("{name}@{version} {}", dim("(new)")),
            '~' => format!("{name}@{version} {}", dim("(changed)")),
            _ => format!("{name}@{version} {}", dim("(unchanged)")),
        };
        println!("{glyph} {detail}");
    }

    lock.go = Some(gv_core::lock::LockedGo {
        version: go_version.clone(),
        sha256: go_sha256,
    });

    if !frozen {
        lock.save(root)?;
    }
    Ok(())
}

// ----- gv init --------------------------------------------------------------

async fn cmd_init(with: Option<Vec<String>>, go: Option<String>, force: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let target = cwd.join(project::PROJECT_FILE);
    if target.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            target.display()
        );
    }

    // Resolve a sensible Go pin: explicit flag > go.mod toolchain > .go-version > latest stable.
    let go_pin = match go {
        Some(v) => Some(release::normalize_version(&v)),
        None => match gv_core::manifest::find_project_toolchain(&cwd)? {
            Some(hit) => Some(hit.version),
            None => {
                let pb = spinner("resolving latest Go release");
                let client = http_client()?;
                let releases = release::fetch_index(&client).await?;
                pb.finish_and_clear();
                release::latest_stable(&releases).map(|r| r.version.clone())
            }
        },
    };

    let mut proj = project::Project {
        go: go_pin.as_deref().map(|v| gv_core::project::GoSection {
            version: v.to_string(),
        }),
        tools: Default::default(),
    };

    if let Some(names) = with {
        for raw in names {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let (name, version) = parse_tool_spec(raw);
            // Validate: must be in registry or have an explicit @ pin.
            if version.is_none() && gv_core::registry::lookup(&name).is_none() {
                bail!("unknown tool '{name}' — pick from the registry or pass `name@version`");
            }
            proj.tools.insert(
                name,
                ToolSpec::Short(version.unwrap_or_else(|| "latest".to_string())),
            );
        }
    }

    project::save(&cwd, &proj)?;
    println!("{} wrote {}", success_mark(), target.display());
    if let Some(v) = go_pin {
        println!("    toolchain : {v}");
    }
    if proj.tools.is_empty() {
        println!(
            "    tools     : {} ({})",
            dim("(none)"),
            dim("add later via `gv add tool <name>`")
        );
    } else {
        println!("    tools     :");
        for (name, spec) in &proj.tools {
            println!("      - {name} = \"{}\"", spec.version());
        }
    }
    println!(
        "{}",
        dim("    next      : run `gv sync` to install everything")
    );
    Ok(ExitCode::SUCCESS)
}

// ----- gv self update -------------------------------------------------------

#[derive(serde::Deserialize)]
struct GhRelease {
    tag_name: String,
}

async fn cmd_self_update(platform: Platform, check: bool) -> Result<ExitCode> {
    let current = env!("CARGO_PKG_VERSION");
    let client = http_client()?;
    let release: GhRelease = client
        .get("https://api.github.com/repos/O6lvl4/gv/releases/latest")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("parse GitHub release JSON")?;
    let latest_tag = release.tag_name; // "v0.2.0"
    let latest = latest_tag.strip_prefix('v').unwrap_or(&latest_tag);

    if !is_semver_newer(latest, current) {
        println!(
            "{} gv is already up to date {}",
            success_mark(),
            dim(&format!("(installed: {current}, latest: {latest})"))
        );
        return Ok(ExitCode::SUCCESS);
    }
    if check {
        println!(
            "{} a newer release is available: {} {} {}",
            success_mark(),
            dim(current),
            dim("→"),
            color_bold(latest)
        );
        return Ok(ExitCode::SUCCESS);
    }

    let triple =
        target_triple().ok_or_else(|| anyhow!("self-update is not supported on this platform"))?;
    let asset_stem = format!("gv-{latest_tag}-{triple}");
    let _ = platform;
    let archive_name = if cfg!(target_os = "windows") {
        format!("{asset_stem}.zip")
    } else {
        format!("{asset_stem}.tar.gz")
    };
    let url = format!("https://github.com/O6lvl4/gv/releases/download/{latest_tag}/{archive_name}");
    let sha_url = format!("{url}.sha256");

    let pb = spinner(&format!("downloading {archive_name}"));
    let bytes = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let sha_text = client
        .get(&sha_url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    pb.finish_and_clear();

    // Verify sha256.
    let expected: String = sha_text.split_whitespace().next().unwrap_or("").to_string();
    let mut hasher = sha2::Sha256::new();
    hasher.update(&bytes);
    let actual = hex::encode(hasher.finalize());
    if !expected.is_empty() && expected != actual {
        bail!("sha256 mismatch: expected {expected}, got {actual}");
    }

    // Extract gv + gv-shim into a temp dir.
    let tmp = tempdir_in(std::env::temp_dir(), "gv-self-update-")?;
    let archive_path = tmp.join(&archive_name);
    std::fs::write(&archive_path, &bytes)?;
    gv_core::install::extract_archive(&archive_path, &tmp)?;

    let stage = tmp.join(&asset_stem);
    let new_gv = stage.join(if cfg!(windows) { "gv.exe" } else { "gv" });
    let new_shim = stage.join(if cfg!(windows) {
        "gv-shim.exe"
    } else {
        "gv-shim"
    });
    if !new_gv.exists() || !new_shim.exists() {
        bail!(
            "extracted archive missing expected binaries at {}",
            stage.display()
        );
    }

    // Atomic replace.
    let current_exe = std::env::current_exe()?;
    let parent = current_exe.parent().unwrap_or(Path::new("."));
    let shim_dest = parent.join(if cfg!(windows) {
        "gv-shim.exe"
    } else {
        "gv-shim"
    });
    replace_binary(&new_gv, &current_exe)?;
    replace_binary(&new_shim, &shim_dest).ok(); // shim is best-effort

    println!(
        "{} gv {} → {}",
        success_mark(),
        dim(current),
        color_bold(latest)
    );
    println!("    binary    : {}", current_exe.display());
    println!("    shim      : {}", shim_dest.display());
    Ok(ExitCode::SUCCESS)
}

fn target_triple() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => return None,
    })
}

fn is_semver_newer(latest: &str, current: &str) -> bool {
    fn parse(s: &str) -> (u64, u64, u64) {
        let mut parts = s.split('.').map(|p| p.split('-').next().unwrap_or(""));
        (
            parts.next().and_then(|p| p.parse().ok()).unwrap_or(0),
            parts.next().and_then(|p| p.parse().ok()).unwrap_or(0),
            parts.next().and_then(|p| p.parse().ok()).unwrap_or(0),
        )
    }
    parse(latest) > parse(current)
}

fn tempdir_in(parent: impl AsRef<Path>, prefix: &str) -> Result<PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = parent.as_ref().join(format!("{prefix}{nonce}"));
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

fn replace_binary(src: &Path, dest: &Path) -> Result<()> {
    // On Unix, std::fs::rename across the running binary is allowed because
    // the kernel tracks running processes by inode. On Windows, the running
    // .exe cannot be replaced; rename it aside first then move new in.
    if cfg!(windows) && dest.exists() {
        let backup = dest.with_extension("old");
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(dest, &backup)
            .with_context(|| format!("rename {} → {}", dest.display(), backup.display()))?;
    }
    std::fs::rename(src, dest)
        .or_else(|_| {
            // Cross-device rename not allowed → copy + remove.
            std::fs::copy(src, dest)
                .map(|_| ())
                .and_then(|_| std::fs::remove_file(src))
        })
        .with_context(|| format!("install binary at {}", dest.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dest)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(dest, perms)?;
    }
    Ok(())
}

// ----- gv outdated ----------------------------------------------------------

async fn cmd_outdated(paths: &Paths, _platform: Platform) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let root = project::find_root(&cwd).ok_or_else(|| {
        anyhow!(
            "no project root found (need a go.mod or gv.toml above {})",
            cwd.display()
        )
    })?;
    let proj = project::load(&root)?;
    let lock = Lock::load(&root)?;
    let client = http_client()?;

    let mut rows: Vec<(String, String, String, bool)> = Vec::new();

    // Toolchain
    let pb = spinner("checking toolchain");
    let releases = release::fetch_index(&client).await?;
    pb.finish_and_clear();
    let latest_go =
        release::latest_stable(&releases).ok_or_else(|| anyhow!("no stable Go release found"))?;
    let cur_go = resolve::resolve(paths, &root)?.map(|r| r.version);
    if let Some(cur) = cur_go {
        let behind = cur != latest_go.version;
        rows.push((
            "toolchain".to_string(),
            cur,
            latest_go.version.clone(),
            behind,
        ));
    }

    // Tools (parallel @latest fetches)
    if !proj.tools.is_empty() {
        let pb = spinner(&format!(
            "checking {} tool(s) for updates",
            proj.tools.len()
        ));
        let futs = proj.tools.keys().map(|name| {
            let client = client.clone();
            let name = name.clone();
            async move {
                let resolved =
                    tool::resolve(&client, &name, &ToolSpec::Short("latest".into())).await?;
                Ok::<_, anyhow::Error>((name, resolved.version))
            }
        });
        let resolved: Vec<(String, String)> = try_join_all(futs).await?;
        pb.finish_and_clear();
        for (name, latest) in resolved {
            let locked = lock
                .find_tool(&name)
                .map(|t| t.version.clone())
                .unwrap_or_else(|| "—".to_string());
            let behind = locked != latest;
            rows.push((name, locked, latest, behind));
        }
    }

    if rows.is_empty() {
        println!("{}", dim("(nothing to check)"));
        return Ok(ExitCode::SUCCESS);
    }

    let any_behind = rows.iter().any(|(_, _, _, b)| *b);
    let name_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(4);
    let cur_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(6);
    let new_w = rows.iter().map(|r| r.2.len()).max().unwrap_or(0).max(6);
    println!(
        "{:<name_w$}  {:<cur_w$}  {:<new_w$}  {}",
        color_bold("NAME"),
        color_bold("LOCKED"),
        color_bold("LATEST"),
        color_bold("STATUS"),
        name_w = name_w,
        cur_w = cur_w,
        new_w = new_w
    );
    for (name, locked, latest, behind) in &rows {
        let mark = if *behind {
            color_yellow("behind")
        } else {
            color_green("up to date")
        };
        println!(
            "{:<name_w$}  {:<cur_w$}  {:<new_w$}  {}",
            name,
            locked,
            latest,
            mark,
            name_w = name_w,
            cur_w = cur_w,
            new_w = new_w
        );
    }

    if any_behind {
        println!();
        println!("{} run `gv upgrade` to bump", dim("→"));
        Ok(ExitCode::from(2))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

// ----- gv migrate-tools -----------------------------------------------------

async fn cmd_migrate_tools(from: Option<PathBuf>, dry_run: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let root = project::find_root(&cwd).ok_or_else(|| {
        anyhow!(
            "no project root found (need a go.mod or gv.toml above {})",
            cwd.display()
        )
    })?;

    let candidates: Vec<PathBuf> = match from {
        Some(p) => vec![p],
        None => find_tools_go_files(&root)?,
    };
    if candidates.is_empty() {
        bail!(
            "no tools.go-style file found under {}. Pass --from <path> if it lives elsewhere.",
            root.display()
        );
    }

    let mut imports: Vec<String> = Vec::new();
    for path in &candidates {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if !looks_like_tools_file(&text) {
            continue;
        }
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(pkg) = parse_blank_import(trimmed) {
                imports.push(pkg.to_string());
            }
        }
    }

    if imports.is_empty() {
        bail!("no `_ \"…\"` imports found under a `//go:build tools` constraint");
    }

    let client = http_client()?;
    let mut additions: Vec<(String, String)> = Vec::new(); // (gv name, package)
    let pb = spinner(&format!("resolving {} import(s)", imports.len()));
    for pkg in &imports {
        let (_module, _info) = gv_core::proxy::find_module(&client, pkg).await?;
        let name = derive_tool_name(pkg);
        additions.push((name, pkg.clone()));
    }
    pb.finish_and_clear();

    let mut proj = project::load(&root)?;
    let mut new_count = 0usize;
    for (name, pkg) in &additions {
        if proj.tools.contains_key(name) {
            println!(
                "{} {} {}",
                dim("="),
                name,
                dim(&format!("(already pinned, skipping {pkg})"))
            );
            continue;
        }
        // If the import path matches the registry's package, use a Short spec;
        // otherwise emit a Long spec with explicit package so reproducibility
        // doesn't depend on the registry.
        let spec = if gv_core::registry::lookup(name)
            .map(|e| e.package == pkg.as_str())
            .unwrap_or(false)
        {
            ToolSpec::Short("latest".into())
        } else {
            ToolSpec::Long {
                package: Some(pkg.clone()),
                version: "latest".into(),
                bin: None,
            }
        };
        proj.tools.insert(name.clone(), spec);
        new_count += 1;
        println!("{} {} = \"{}\"", color_green("+"), name, pkg);
    }

    if new_count == 0 {
        println!("{}", dim("(nothing new to migrate)"));
        return Ok(ExitCode::SUCCESS);
    }

    if dry_run {
        println!("{}", dim("    --dry-run: gv.toml unchanged"));
    } else {
        project::save(&root, &proj)?;
        println!(
            "{} pinned {} new tool(s) in {}",
            success_mark(),
            new_count,
            root.join("gv.toml").display()
        );
        println!("{}", dim("    next: run `gv sync` to install them"));
    }
    Ok(ExitCode::SUCCESS)
}

fn find_tools_go_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in walk_files(root, 3) {
        let path = entry;
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.ends_with(".go"))
            .unwrap_or(false)
        {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if looks_like_tools_file(&content) {
                    out.push(path);
                }
            }
        }
    }
    Ok(out)
}

fn walk_files(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "vendor" || name == "node_modules" {
                continue;
            }
            if p.is_dir() {
                if depth < max_depth {
                    stack.push((p, depth + 1));
                }
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn looks_like_tools_file(text: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim();
        t == "//go:build tools" || t == "// +build tools"
    })
}

fn parse_blank_import(line: &str) -> Option<&str> {
    let s = line.strip_prefix('_')?.trim_start();
    let s = s.strip_prefix('"')?;
    let end = s.find('"')?;
    Some(&s[..end])
}

fn derive_tool_name(pkg: &str) -> String {
    // Match Go's `go install` convention: the last path segment, ignoring a
    // trailing `/vN` major-version marker.
    let mut last = pkg.rsplit('/').next().unwrap_or(pkg);
    if last.starts_with('v') && last.len() > 1 && last[1..].chars().all(|c| c.is_ascii_digit()) {
        let trimmed = &pkg[..pkg.len() - last.len() - 1];
        last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    }
    last.to_string()
}

// ----- gv tool {list, registry, remove} -------------------------------------

fn cmd_tool_list(paths: &Paths) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let Some(root) = project::find_root(&cwd) else {
        bail!("no project root found above {}", cwd.display());
    };
    let proj = project::load(&root)?;
    let lock = Lock::load(&root)?;

    if proj.tools.is_empty() {
        println!("{}", dim("(no tools pinned in gv.toml)"));
        return Ok(ExitCode::SUCCESS);
    }

    let name_w = proj.tools.keys().map(|s| s.len()).max().unwrap_or(0).max(4);
    println!(
        "{:<name_w$}  {:<12}  {:<10}  {}",
        color_bold("NAME"),
        color_bold("REQUESTED"),
        color_bold("LOCKED"),
        color_bold("STATUS"),
        name_w = name_w
    );
    for (name, spec) in &proj.tools {
        let requested = spec.version();
        let (locked, status) = match lock.find_tool(name) {
            Some(t) => {
                let bin = tool::tool_bin_path(paths, t);
                let installed = if bin.exists() {
                    color_green("present")
                } else {
                    color_yellow("missing")
                };
                (t.version.clone(), installed)
            }
            None => ("—".to_string(), color_yellow("unsynced (run `gv sync`)")),
        };
        println!(
            "{:<name_w$}  {:<12}  {:<10}  {}",
            name,
            requested,
            locked,
            status,
            name_w = name_w
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_tool_registry() -> Result<ExitCode> {
    let entries = gv_core::registry::all();
    let name_w = entries
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(0)
        .max(4);
    println!(
        "{:<name_w$}  {}",
        color_bold("NAME"),
        color_bold("PACKAGE"),
        name_w = name_w
    );
    for e in entries {
        println!("{:<name_w$}  {}", e.name, e.package, name_w = name_w);
    }
    println!();
    println!(
        "{}",
        dim(&format!(
            "    {} entries — pass `name@version` or set [tools.{{...}}] in gv.toml for a custom package",
            entries.len()
        ))
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_tool_remove(paths: &Paths, name: &str) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let Some(root) = project::find_root(&cwd) else {
        bail!("no project root found above {}", cwd.display());
    };

    let mut proj = project::load(&root)?;
    let mut lock = Lock::load(&root)?;
    let in_proj = proj.tools.remove(name).is_some();
    let in_lock_before = lock.tools.len();
    lock.tools.retain(|t| t.name != name);
    let in_lock = in_lock_before != lock.tools.len();

    if !in_proj && !in_lock {
        bail!("tool '{name}' is not pinned");
    }

    project::save(&root, &proj)?;
    lock.save(&root)?;

    println!(
        "{} removed {} from project",
        success_mark(),
        color_bold(name)
    );
    let _ = paths; // binary lingers in the store until `gv cache prune`
    println!(
        "{}",
        dim("    binary stays in the store; run `gv cache prune` to reclaim disk")
    );
    Ok(ExitCode::SUCCESS)
}

// ----- gv tree --------------------------------------------------------------

fn cmd_tree(paths: &Paths, deps: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let root = project::find_root(&cwd);

    println!("{}", color_bold("gv tree"));

    // Workspace branch — only emitted if a go.work exists at the resolved root.
    if let Some(r) = root.as_deref() {
        let go_work = r.join(gv_core::workspace::WORKSPACE_FILE);
        if go_work.is_file() {
            let work = gv_core::workspace::load(r)?;
            println!(
                "├── {} {} ({})",
                color_cyan("workspace"),
                r.display(),
                dim(&format!(
                    "{} member{}",
                    work.members.len(),
                    plural(work.members.len())
                ))
            );
            for m in &work.members {
                println!("│   ├── {}", m.display());
            }
        }
    }

    // Toolchain branch
    let resolved = resolve::resolve(paths, &cwd)?;
    match resolved.as_ref() {
        Some(r) => {
            let store_path = paths.version_dir(&r.version);
            let store_target = std::fs::read_link(&store_path).ok();
            println!("├── {} {}", color_cyan("toolchain"), color_bold(&r.version));
            println!("│   ├── source: {}", source_label(r));
            if let Some(t) = store_target {
                println!("│   └── store:  {}", t.display());
            } else {
                println!("│   └── store:  {}", store_path.display());
            }
        }
        None => println!("├── {} {}", color_cyan("toolchain"), dim("(none)")),
    }

    // Tools branch
    let lock = match root.as_deref() {
        Some(r) => Lock::load(r).unwrap_or_else(|_| Lock::empty()),
        None => Lock::empty(),
    };
    if lock.tools.is_empty() {
        println!("└── {} {}", color_cyan("tools"), dim("(none pinned)"));
    } else {
        println!("└── {} ({})", color_cyan("tools"), lock.tools.len());
        let last = lock.tools.len() - 1;
        for (i, t) in lock.tools.iter().enumerate() {
            let (branch, indent) = if i == last {
                ("└──", "    ")
            } else {
                ("├──", "│   ")
            };
            let bin = tool::tool_bin_path(paths, t);
            let bin_status = if bin.exists() {
                color_green("present")
            } else {
                color_yellow("missing")
            };
            println!(
                "    {branch} {} @ {}  [{}]",
                color_bold(&t.name),
                t.version,
                bin_status
            );
            println!("    {indent}├── package : {}", t.package);
            println!("    {indent}├── h1      : {}", t.module_hash);
            println!("    {indent}├── built   : with {}", t.built_with);
            println!("    {indent}└── bin     : {}", bin.display());
        }
    }

    if deps {
        if let Some(r) = root.as_deref() {
            println!();
            println!("{}", color_bold("dependencies"));
            let go_mods = collect_go_mods(r);
            if go_mods.is_empty() {
                println!("  {}", dim("(no go.mod found)"));
            } else {
                for (i, gm) in go_mods.iter().enumerate() {
                    let last = i == go_mods.len() - 1;
                    let branch = if last { "└──" } else { "├──" };
                    let sub_indent = if last { "    " } else { "│   " };
                    println!(
                        "{branch} {} ({})",
                        color_cyan(&display_path(gm.parent().unwrap_or(gm))),
                        dim(&format!("{}", gm.display()))
                    );
                    let direct = parse_go_mod_direct_deps(gm).unwrap_or_default();
                    if direct.is_empty() {
                        println!("{sub_indent}└── {}", dim("(no direct dependencies)"));
                    } else {
                        let last_idx = direct.len() - 1;
                        for (j, dep) in direct.iter().enumerate() {
                            let mark = if j == last_idx {
                                "└──"
                            } else {
                                "├──"
                            };
                            println!("{sub_indent}{mark} {} {}", dep.0, dim(&dep.1));
                        }
                    }
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn collect_go_mods(root: &Path) -> Vec<PathBuf> {
    // If go.work is present, collect every member's go.mod. Otherwise just
    // the top-level go.mod (if any).
    let go_work = root.join(gv_core::workspace::WORKSPACE_FILE);
    if go_work.is_file() {
        if let Ok(work) = gv_core::workspace::load(root) {
            let mut out = Vec::new();
            for m in &work.members {
                let candidate = m.join("go.mod");
                if candidate.is_file() {
                    out.push(candidate);
                }
            }
            return out;
        }
    }
    let go_mod = root.join("go.mod");
    if go_mod.is_file() {
        vec![go_mod]
    } else {
        vec![]
    }
}

/// Parse direct (non-`// indirect`) `require` lines from a go.mod file.
/// Returns `(module_path, version)` pairs.
fn parse_go_mod_direct_deps(go_mod: &Path) -> Result<Vec<(String, String)>> {
    let text =
        std::fs::read_to_string(go_mod).with_context(|| format!("read {}", go_mod.display()))?;
    let mut out = Vec::new();
    let mut in_require_block = false;
    for raw in text.lines() {
        let stripped_comment = match raw.split_once("//") {
            Some((_, after)) if after.trim() == "indirect" => continue,
            Some((before, _)) => before,
            None => raw,
        };
        let line = stripped_comment.trim();
        if line.is_empty() {
            continue;
        }
        if line == "require (" {
            in_require_block = true;
            continue;
        }
        if in_require_block && line == ")" {
            in_require_block = false;
            continue;
        }
        let item = if let Some(rest) = line.strip_prefix("require ") {
            rest.trim()
        } else if in_require_block {
            line
        } else {
            continue;
        };
        // Split on whitespace into (path, version)
        let mut parts = item.split_whitespace();
        let (Some(path), Some(version)) = (parts.next(), parts.next()) else {
            continue;
        };
        out.push((path.to_string(), version.to_string()));
    }
    out.sort();
    Ok(out)
}

// ----- gv x (ephemeral) ----------------------------------------------------

async fn cmd_x(paths: &Paths, argv: Vec<String>) -> Result<ExitCode> {
    if argv.is_empty() {
        bail!("usage: gvx <tool> [args...]   (e.g. `gvx staticcheck ./...`)");
    }
    let (spec, rest) = (&argv[0], &argv[1..]);
    let (name, version) = parse_tool_spec(spec);
    let spec_obj = ToolSpec::Short(version.unwrap_or_else(|| "latest".to_string()));

    let client = http_client()?;
    let pb = spinner(&format!("resolving {name}"));
    let resolved = tool::resolve(&client, &name, &spec_obj).await?;
    pb.finish_and_clear();

    // Pick a Go toolchain. Prefer the project's (so workspace tools build
    // against the same Go), otherwise the latest installed, otherwise install
    // the latest stable. Whichever we land on, materialize it on disk if
    // missing — `gvx` should "just work" without a prior `gv sync`.
    let cwd = std::env::current_dir()?;
    let go_version = match resolve::resolve(paths, &cwd)? {
        Some(r) => r.version,
        None => {
            let installed = resolve::list_installed(paths)?;
            if let Some(v) = installed.into_iter().next() {
                v
            } else {
                let releases = release::fetch_index(&client).await?;
                release::latest_stable(&releases)
                    .ok_or_else(|| anyhow!("no stable Go release found"))?
                    .version
                    .clone()
            }
        }
    };
    let go_bin = paths.version_dir(&go_version).join("bin").join("go");
    if !go_bin.exists() {
        let installer = Installer {
            paths,
            client: &client,
            platform: Platform::detect()?,
        };
        let pb = spinner(&format!("installing {go_version} for ephemeral run"));
        installer.install(&go_version).await?;
        pb.finish_and_clear();
    }

    let bin_path = tool::tool_dir(paths, &resolved.name, &resolved.version).join(&resolved.bin);

    if !bin_path.exists() {
        let pb = spinner(&format!("building {}@{}", resolved.name, resolved.version));
        let resolved_clone = resolved.clone();
        let paths_clone = paths.clone();
        let go_version_clone = go_version.clone();
        let _locked = tokio::task::spawn_blocking(move || {
            tool::install(&paths_clone, &go_version_clone, &resolved_clone)
        })
        .await
        .map_err(|e| anyhow!("install task panicked: {e}"))??;
        pb.finish_and_clear();
        say!(
            "{} {} {}@{}",
            success_mark(),
            dim("ephemeral:"),
            resolved.name,
            resolved.version
        );
    }

    use std::process::Command;
    let mut child = Command::new(&bin_path);
    child.args(rest);
    let bin_dir = paths.version_dir(&go_version).join("bin");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = std::ffi::OsString::from(bin_dir.as_os_str());
    new_path.push(":");
    new_path.push(&path);
    child.env("PATH", new_path);
    child.env("GOROOT", paths.version_dir(&go_version));
    child.env("GOTOOLCHAIN", "local");
    let status = child
        .status()
        .with_context(|| format!("spawn {}", bin_path.display()))?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

// ----- gv uninstall ---------------------------------------------------------

fn cmd_uninstall(paths: &Paths, version: &str) -> Result<ExitCode> {
    let canonical = release::normalize_version(version);
    let link = paths.version_dir(&canonical);
    if !link.exists() && !link.is_symlink() {
        bail!("{canonical} is not installed");
    }
    std::fs::remove_file(&link)
        .or_else(|_| std::fs::remove_dir_all(&link))
        .with_context(|| format!("remove {}", link.display()))?;
    println!("{} uninstalled {canonical}", success_mark());
    say!(
        "{}",
        dim("    note: store dir kept; reclaim disk with `gv cache prune`")
    );
    Ok(ExitCode::SUCCESS)
}

// ----- gv lock --------------------------------------------------------------

async fn cmd_lock(paths: &Paths) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let root = project::find_root(&cwd).ok_or_else(|| {
        anyhow!(
            "no project root found (need a go.mod or gv.toml above {})",
            cwd.display()
        )
    })?;
    let proj = project::load(&root)?;
    let mut lock = Lock::load(&root)?;
    let client = http_client()?;

    // Toolchain
    let go_version = match resolve::resolve(paths, &root)? {
        Some(r) => r.version,
        None => proj
            .go
            .as_ref()
            .map(|g| release::normalize_version(&g.version))
            .ok_or_else(|| anyhow!("no Go version is resolvable here"))?,
    };
    // We don't need to install to compute go-archive sha; reach into the
    // release index for that.
    let releases = release::fetch_index(&client).await?;
    let platform = Platform::detect()?;
    let go_sha256 = match release::select_archive(&releases, &go_version, platform) {
        Ok((_, file)) => file.sha256.clone(),
        Err(_) => String::new(),
    };
    lock.go = Some(gv_core::lock::LockedGo {
        version: go_version.clone(),
        sha256: go_sha256,
    });

    if !proj.tools.is_empty() {
        let pb = spinner(&format!("re-resolving {} tool(s)", proj.tools.len()));
        let futs = proj.tools.iter().map(|(name, spec)| {
            let client = client.clone();
            let name = name.clone();
            let spec = spec.clone();
            async move { tool::resolve(&client, &name, &spec).await }
        });
        let resolved: Vec<tool::ResolvedTool> = try_join_all(futs).await?;
        pb.finish_and_clear();
        for r in resolved {
            // Carry over built_with / binary_sha256 from the existing lock entry
            // when version matches; otherwise leave them empty (they'll fill
            // in on the next sync).
            let prev = lock.find_tool(&r.name);
            let built_with = prev
                .filter(|p| p.version == r.version)
                .map(|p| p.built_with.clone())
                .unwrap_or_default();
            let binary_sha256 = prev
                .filter(|p| p.version == r.version)
                .map(|p| p.binary_sha256.clone())
                .unwrap_or_default();
            lock.upsert_tool(LockedTool {
                name: r.name,
                package: r.package,
                version: r.version,
                bin: r.bin,
                module_hash: r.module_hash,
                built_with,
                binary_sha256,
            });
        }
    }

    lock.save(&root)?;
    println!(
        "{} wrote {}",
        success_mark(),
        root.join("gv.lock").display()
    );
    say!(
        "{}",
        dim("    note: nothing was installed; run `gv sync` to materialize")
    );
    Ok(ExitCode::SUCCESS)
}

// ----- gv dir ---------------------------------------------------------------

fn cmd_dir(paths: &Paths, kind: DirKind) -> Result<ExitCode> {
    let p = match kind {
        DirKind::Data => paths.data.clone(),
        DirKind::Cache => paths.cache.clone(),
        DirKind::Config => paths.config.clone(),
        DirKind::Store => paths.store(),
        DirKind::Versions => paths.versions(),
        DirKind::Tools => paths.data.join("tools"),
    };
    println!("{}", p.display());
    Ok(ExitCode::SUCCESS)
}

fn source_label(r: &resolve::Resolved) -> String {
    use ToolchainSource::*;
    match r.source {
        EnvVar => "GV_VERSION".into(),
        GoWork => format!(
            "go.work toolchain ({})",
            r.origin.as_deref().map(display_path).unwrap_or_default()
        ),
        GoMod => format!(
            "go.mod toolchain ({})",
            r.origin.as_deref().map(display_path).unwrap_or_default()
        ),
        GoVersionFile => format!(
            ".go-version ({})",
            r.origin.as_deref().map(display_path).unwrap_or_default()
        ),
        Global => "global".into(),
        LatestInstalled => "latest installed".into(),
    }
}

// ----- gv upgrade -----------------------------------------------------------

async fn cmd_upgrade(
    paths: &Paths,
    _platform: Platform,
    names: Vec<String>,
    toolchain: bool,
) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let root = project::find_root(&cwd).ok_or_else(|| {
        anyhow!(
            "no project root found (need a go.mod or gv.toml above {})",
            cwd.display()
        )
    })?;
    let proj = project::load(&root)?;
    let mut lock = Lock::load(&root)?;
    let client = http_client()?;

    let target_names: Vec<String> = if names.is_empty() {
        proj.tools.keys().cloned().collect()
    } else {
        for n in &names {
            if !proj.tools.contains_key(n) {
                bail!("tool '{n}' is not pinned in gv.toml");
            }
        }
        names
    };

    if target_names.is_empty() && !toolchain {
        println!("{}", dim("(no tools to upgrade)"));
        return Ok(ExitCode::SUCCESS);
    }

    // Determine the active Go toolchain. We need it to build any tool that moved.
    let go_version = match resolve::resolve(paths, &root)? {
        Some(r) => r.version,
        None => bail!("no Go version is resolvable; run `gv sync` first"),
    };

    // ----- toolchain upgrade --------------------------------------------------
    if toolchain {
        let releases = release::fetch_index(&client).await?;
        let latest = release::latest_stable(&releases)
            .ok_or_else(|| anyhow!("no stable Go release found"))?;
        let new_version = latest.version.clone();
        if new_version == go_version {
            println!(
                "{} toolchain {} {}",
                success_mark(),
                new_version,
                dim("(already latest)")
            );
        } else {
            let installer = Installer {
                paths,
                client: &client,
                platform: Platform::detect()?,
            };
            let pb = spinner(&format!("upgrading toolchain → {new_version}"));
            let report = installer.install(&new_version).await?;
            pb.finish_and_clear();
            // Persist to gv.toml so future syncs honor it (without stomping go.mod).
            let mut proj_w = project::load(&root)?;
            proj_w.go = Some(gv_core::project::GoSection {
                version: new_version.clone(),
            });
            project::save(&root, &proj_w)?;
            lock.go = Some(gv_core::lock::LockedGo {
                version: new_version.clone(),
                sha256: report.sha256,
            });
            println!(
                "{} toolchain {} → {}",
                color_green("~"),
                go_version,
                color_bold(&new_version)
            );
        }
    }

    if target_names.is_empty() {
        lock.save(&root)?;
        return Ok(ExitCode::SUCCESS);
    }

    // ----- per-target tool upgrade -------------------------------------------
    let mp = MultiProgress::new();
    let resolve_started = Instant::now();
    let resolve_futs = target_names.iter().map(|name| {
        let client = client.clone();
        let mp = mp.clone();
        let name = name.clone();
        async move {
            let pb = mp.add(spinner(&format!("resolving {name}@latest")));
            let resolved = tool::resolve(&client, &name, &ToolSpec::Short("latest".into())).await?;
            pb.finish_and_clear();
            Ok::<_, anyhow::Error>(resolved)
        }
    });
    let resolved: Vec<tool::ResolvedTool> = try_join_all(resolve_futs).await?;
    println!(
        "{} Resolved {} tool{} in {}",
        success_mark(),
        resolved.len(),
        plural(resolved.len()),
        format_duration(resolve_started.elapsed().as_millis())
    );

    // Decide which actually changed.
    let mut to_install: Vec<tool::ResolvedTool> = Vec::new();
    let mut bumps: Vec<(String, String, String)> = Vec::new(); // name, old, new
    let mut skipped: Vec<String> = Vec::new();
    for r in resolved {
        match lock.find_tool(&r.name).map(|l| l.version.clone()) {
            Some(prev) if prev == r.version => skipped.push(r.name.clone()),
            prev => {
                bumps.push((
                    r.name.clone(),
                    prev.unwrap_or_else(|| "(none)".into()),
                    r.version.clone(),
                ));
                to_install.push(r);
            }
        }
    }

    if to_install.is_empty() {
        for n in &skipped {
            println!("  {} {n} {}", dim("="), dim("(already latest)"));
        }
        return Ok(ExitCode::SUCCESS);
    }

    let install_started = Instant::now();
    let install_futs = to_install.iter().map(|r| {
        let mp = mp.clone();
        let paths = paths.clone();
        let go_version = go_version.clone();
        let r = r.clone();
        async move {
            let pb = mp.add(spinner(&format!("building {}@{}", r.name, r.version)));
            let res = tokio::task::spawn_blocking(move || tool::install(&paths, &go_version, &r))
                .await
                .map_err(|e| anyhow!("install task panicked: {e}"))??;
            pb.finish_and_clear();
            Ok::<_, anyhow::Error>(res)
        }
    });
    let installed: Vec<LockedTool> = try_join_all(install_futs).await?;
    println!(
        "{} Built {} tool{} in {}",
        success_mark(),
        installed.len(),
        plural(installed.len()),
        format_duration(install_started.elapsed().as_millis())
    );

    for new in installed {
        lock.upsert_tool(new);
    }
    lock.save(&root)?;

    for (name, old, new) in &bumps {
        println!(
            " {} {name}: {} → {}",
            color_green("~"),
            dim(old),
            color_bold(new)
        );
    }
    for n in &skipped {
        println!("  {} {n} {}", dim("="), dim("(already latest)"));
    }
    Ok(ExitCode::SUCCESS)
}

// ----- gv env ---------------------------------------------------------------

fn cmd_env(paths: &Paths, shell: EnvShell) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let r = resolve::resolve(paths, &cwd)?
        .ok_or_else(|| anyhow!("no Go version resolved in {}", cwd.display()))?;
    let goroot = paths.version_dir(&r.version);
    let bin_dir = goroot.join("bin");

    match shell {
        EnvShell::Sh => {
            println!("export GOROOT={}", quote_sh(&goroot.display().to_string()));
            println!("export GOTOOLCHAIN=local");
            println!(
                "export PATH={}:\"$PATH\"",
                quote_sh(&bin_dir.display().to_string())
            );
        }
        EnvShell::Fish => {
            println!("set -gx GOROOT {}", quote_sh(&goroot.display().to_string()));
            println!("set -gx GOTOOLCHAIN local");
            println!(
                "set -gx PATH {} $PATH",
                quote_sh(&bin_dir.display().to_string())
            );
        }
        EnvShell::Powershell => {
            println!("$env:GOROOT = {}", quote_ps(&goroot.display().to_string()));
            println!("$env:GOTOOLCHAIN = 'local'");
            println!(
                "$env:Path = {} + ';' + $env:Path",
                quote_ps(&bin_dir.display().to_string())
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn quote_sh(s: &str) -> String {
    // Single-quote, escape any embedded single quotes.
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn quote_ps(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// ----- gv cache -------------------------------------------------------------

fn go_cache_dirs() -> (Option<PathBuf>, Option<PathBuf>) {
    let gomod = std::env::var_os("GOMODCACHE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("GOPATH")
                .map(|p| PathBuf::from(p).join("pkg").join("mod"))
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|h| PathBuf::from(h).join("go").join("pkg").join("mod"))
                })
        });
    let gocache = std::env::var_os("GOCACHE").map(PathBuf::from).or_else(|| {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("go-build"))
    });
    (gomod, gocache)
}

fn cmd_cache_info(paths: &Paths) -> Result<ExitCode> {
    let mut entries: Vec<(String, PathBuf)> = vec![
        ("store    ".into(), paths.store()),
        ("versions ".into(), paths.versions()),
        ("tools    ".into(), paths.data.join("tools")),
        ("cache    ".into(), paths.cache.clone()),
        ("config   ".into(), paths.config.clone()),
    ];
    let (gomod, gocache) = go_cache_dirs();
    if let Some(p) = gomod {
        entries.push(("GOMODCACHE".into(), p));
    }
    if let Some(p) = gocache {
        entries.push(("GOCACHE   ".into(), p));
    }

    println!("{}", color_bold("gv cache"));
    let mut total: u64 = 0;
    for (label, path) in &entries {
        let (size, count) = if path.exists() {
            dir_size(path)?
        } else {
            (0, 0)
        };
        total += size;
        println!(
            "  {} {:>10}  {:>5} entr{}  {}",
            label,
            humanize(size),
            count,
            if count == 1 { "y" } else { "ies" },
            dim(&path.display().to_string())
        );
    }
    println!("  {} {:>10}", color_bold("total    "), humanize(total));
    println!(
        "{}",
        dim("    note: GOMODCACHE is shared with system Go installs; gv won't auto-prune it")
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_cache_prune(paths: &Paths, dry_run: bool, go_cache: bool) -> Result<ExitCode> {
    let store = paths.store();
    let versions = paths.versions();
    if !store.exists() {
        println!("{}", dim("(empty store)"));
        return Ok(ExitCode::SUCCESS);
    }

    // Collect referenced store dirs (read symlink targets under versions/).
    let mut referenced = std::collections::HashSet::new();
    if versions.exists() {
        for entry in std::fs::read_dir(&versions)? {
            let entry = entry?;
            if let Ok(target) = std::fs::read_link(entry.path()) {
                referenced.insert(target.canonicalize().unwrap_or(target));
            }
        }
    }

    let mut to_remove: Vec<(PathBuf, u64)> = Vec::new();
    for entry in std::fs::read_dir(&store)? {
        let entry = entry?;
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let canon = p.canonicalize().unwrap_or(p.clone());
        if !referenced.contains(&canon) {
            let (size, _) = dir_size(&p)?;
            to_remove.push((p, size));
        }
    }

    if to_remove.is_empty() {
        println!("{} nothing to prune", success_mark());
        return Ok(ExitCode::SUCCESS);
    }

    let total: u64 = to_remove.iter().map(|(_, s)| *s).sum();
    let verb = if dry_run { "would remove" } else { "removed" };
    for (p, sz) in &to_remove {
        println!("  {} {:>10}  {}", verb, humanize(*sz), p.display());
        if !dry_run {
            std::fs::remove_dir_all(p).with_context(|| format!("remove {}", p.display()))?;
        }
    }
    println!(
        "{} {} {} unreferenced store entr{} ({})",
        success_mark(),
        verb,
        to_remove.len(),
        if to_remove.len() == 1 { "y" } else { "ies" },
        humanize(total)
    );

    if go_cache {
        let (_, gocache) = go_cache_dirs();
        if let Some(p) = gocache {
            if p.is_dir() {
                let (size, _) = dir_size(&p)?;
                if dry_run {
                    println!(
                        "  {} {:>10}  {} {}",
                        verb,
                        humanize(size),
                        p.display(),
                        dim("(GOCACHE)")
                    );
                } else {
                    std::fs::remove_dir_all(&p)
                        .with_context(|| format!("remove {}", p.display()))?;
                    println!(
                        "{} wiped GOCACHE at {} ({})",
                        success_mark(),
                        p.display(),
                        humanize(size)
                    );
                }
            } else {
                println!("{}", dim("    GOCACHE not present, nothing to wipe"));
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn dir_size(path: &Path) -> Result<(u64, usize)> {
    let mut total: u64 = 0;
    let mut count: usize = 0;
    if path.is_file() {
        return Ok((std::fs::metadata(path)?.len(), 1));
    }
    if !path.is_dir() {
        return Ok((0, 0));
    }
    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let p = entry.path();
            let meta = entry.metadata()?;
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if d == path {
                    count += 1;
                }
                stack.push(p);
            } else {
                if d == path {
                    count += 1;
                }
                total += meta.len();
            }
        }
    }
    Ok((total, count))
}

fn humanize(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes == 0 {
        return "0 B".into();
    }
    let mut value = bytes as f64;
    let mut idx = 0;
    while value >= 1024.0 && idx < UNITS.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }
    if value >= 100.0 || idx == 0 {
        format!("{:.0} {}", value, UNITS[idx])
    } else {
        format!("{:.1} {}", value, UNITS[idx])
    }
}

fn color_cyan(s: &str) -> String {
    format!("\x1b[36m{s}\x1b[0m")
}
fn color_bold(s: &str) -> String {
    format!("\x1b[1m{s}\x1b[0m")
}

// ----- presentation helpers --------------------------------------------------

fn spinner(msg: &str) -> ProgressBar {
    if quiet() {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("  {spinner:.green} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

fn success_mark() -> &'static str {
    "\x1b[32m✓\x1b[0m"
}
fn dim(s: &str) -> String {
    format!("\x1b[2m{s}\x1b[0m")
}
fn color_green(s: &str) -> String {
    format!("\x1b[32m{s}\x1b[0m")
}
fn color_yellow(s: &str) -> String {
    format!("\x1b[33m{s}\x1b[0m")
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn format_duration(ms: u128) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.2}s", ms as f64 / 1_000.0)
    } else {
        let total_s = ms / 1_000;
        format!("{}m{:02}s", total_s / 60, total_s % 60)
    }
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
                println!(
                    "    - {}@{} (built with {})",
                    t.name, t.version, t.built_with
                );
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
fn lookup_project_tool(paths: &Paths, cwd: &Path, name: &str) -> Result<Option<(PathBuf, String)>> {
    let Some(root) = project::find_root(cwd) else {
        return Ok(None);
    };
    let lock = Lock::load(&root)?;
    let Some(t) = lock.find_tool(name) else {
        return Ok(None);
    };
    let bin = tool::tool_bin_path(paths, t);
    if bin.exists() {
        Ok(Some((bin, t.version.clone())))
    } else {
        Ok(None)
    }
}
