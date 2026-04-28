//! Tiny exec shim. argv[0] determines the tool name (go, gofmt, ...).
//! Resolves the toolchain from CWD and `execve`s the real binary.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("gv-shim: {msg}");
            ExitCode::from(127)
        }
    }
}

fn try_main() -> Result<(), String> {
    let argv0 = std::env::args_os()
        .next()
        .ok_or_else(|| "missing argv[0]".to_string())?;
    let argv0_path = PathBuf::from(&argv0);
    let tool_name = argv0_path
        .file_name()
        .ok_or_else(|| "could not determine tool name from argv[0]".to_string())?
        .to_owned();

    let paths = gv_core::paths::discover().map_err(|e| e.to_string())?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let resolved = gv_core::resolve::resolve(&paths, &cwd)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no Go version resolved".to_string())?;

    let bin_dir = paths.version_dir(&resolved.version).join("bin");
    let bin = bin_dir.join(&tool_name);
    let bin = if bin.exists() {
        bin
    } else {
        // Honor the host's exe suffix: someone may have linked `go` as the
        // shim name on a Windows host where the real binary is `go.exe`.
        let suffix = std::env::consts::EXE_SUFFIX;
        let mut candidate = tool_name.clone();
        if !suffix.is_empty() && !tool_name.to_string_lossy().ends_with(suffix) {
            candidate.push(suffix);
        }
        let with_suffix = bin_dir.join(&candidate);
        if with_suffix.exists() {
            with_suffix
        } else {
            return Err(format!(
                "{} not found at {} — run `gv install {}`",
                tool_name.to_string_lossy(),
                bin.display(),
                resolved.version
            ));
        }
    };

    let args: Vec<OsString> = std::env::args_os().skip(1).collect();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(&bin);
        cmd.args(&args);
        cmd.env("GOROOT", paths.version_dir(&resolved.version));
        cmd.env("GOTOOLCHAIN", "local");
        let err = cmd.exec();
        Err(format!("execve failed: {err}"))
    }

    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(&bin)
            .args(&args)
            .env("GOROOT", paths.version_dir(&resolved.version))
            .env("GOTOOLCHAIN", "local")
            .status()
            .map_err(|e| e.to_string())?;
        std::process::exit(status.code().unwrap_or(1));
    }
}
