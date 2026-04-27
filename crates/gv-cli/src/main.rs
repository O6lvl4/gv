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

#[derive(Debug, Parser)]
#[command(
    name = "gv",
    version,
    about = "Go version & toolchain manager. uv-grade speed.",
    propagate_version = true
)]
struct Cli {
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
    /// Print the resolved environment as a tree.
    Tree,
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
    /// Health check.
    Doctor,
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
    },
}

const STD_GO_TOOLS: &[&str] = &["go", "gofmt"];

#[derive(Debug, Subcommand)]
enum AddCmd {
    /// Pin a tool. Use `name` (registry lookup) or `name@version`.
    Tool { spec: String },
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
        Cmd::Link {
            bin_dir,
            shim,
            tools,
            force,
        } => cmd_link(bin_dir, shim, tools, force),
        Cmd::Unlink { bin_dir, tools } => cmd_unlink(bin_dir, tools),
        Cmd::Tree => cmd_tree(&paths),
        Cmd::Upgrade { names, toolchain } => cmd_upgrade(&paths, platform, names, toolchain).await,
        Cmd::Cache { op } => match op {
            CacheCmd::Info => cmd_cache_info(&paths),
            CacheCmd::Prune { dry_run } => cmd_cache_prune(&paths, dry_run),
        },
        Cmd::Doctor => cmd_doctor(&paths, platform),
    }
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

    for name in &tools {
        let dest = bin_dir.join(name);
        if dest.exists() || dest.is_symlink() {
            if !force {
                let is_our_link = std::fs::read_link(&dest)
                    .map(|p| p == shim)
                    .unwrap_or(false);
                if is_our_link {
                    println!("✓ {name} already links to gv-shim");
                    continue;
                } else {
                    println!(
                        "! {name} exists at {} and is not our shim — skipping (use --force)",
                        dest.display()
                    );
                    continue;
                }
            }
            std::fs::remove_file(&dest).ok();
        }
        symlink(&shim, &dest)
            .with_context(|| format!("link {} -> {}", dest.display(), shim.display()))?;
        println!("✓ linked {name} → gv-shim");
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

    for name in &tools {
        let dest = bin_dir.join(name);
        if !dest.exists() && !dest.is_symlink() {
            continue;
        }
        // Only remove if it's a symlink (don't blow away a real binary by accident).
        if dest.is_symlink() {
            std::fs::remove_file(&dest).with_context(|| format!("remove {}", dest.display()))?;
            println!("✓ unlinked {name}");
        } else {
            println!(
                "! {name} at {} is a real file (not a symlink) — leaving it",
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
            let exe = if toolchain_bin.exists() {
                toolchain_bin
            } else {
                PathBuf::from(cmd)
            };
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
        println!(
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
    println!(
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

    println!(
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

// ----- gv tree --------------------------------------------------------------

fn cmd_tree(paths: &Paths) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let root = project::find_root(&cwd);

    println!("{}", color_bold("gv tree"));

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
    Ok(ExitCode::SUCCESS)
}

fn source_label(r: &resolve::Resolved) -> String {
    use ToolchainSource::*;
    match r.source {
        EnvVar => "GV_VERSION".into(),
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

// ----- gv cache -------------------------------------------------------------

fn cmd_cache_info(paths: &Paths) -> Result<ExitCode> {
    let entries = [
        ("store    ", paths.store()),
        ("versions ", paths.versions()),
        ("tools    ", paths.data.join("tools")),
        ("cache    ", paths.cache.clone()),
        ("config   ", paths.config.clone()),
    ];
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
    Ok(ExitCode::SUCCESS)
}

fn cmd_cache_prune(paths: &Paths, dry_run: bool) -> Result<ExitCode> {
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
