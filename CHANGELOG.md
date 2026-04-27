# Changelog

All notable changes to gv are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and gv uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] — 2026-04-27

### Added

- **GOPROXY honored** — `gv` reads `$GOPROXY` (with `direct` / `off` filtered)
  and tries each entry in order. Mirrors how the Go runtime itself routes
  module fetches. Required for users behind a private proxy
  (`GOPROXY=https://goproxy.internal,https://proxy.golang.org`).
- **GOSUMDB honored** — host portion of the env var is used for h1: hash
  lookups; `GOSUMDB=off` errors out clearly so reproducibility isn't
  silently bypassed.
- **Semver constraints in `gv.toml`** — pin tools by range:
  `gopls = "^v0.18"`, `golangci-lint = "~v1.64"`, `dlv = ">=v1.20,<v2"`.
  `gv` walks the proxy's `/list` endpoint and picks the highest matching
  release (pre-releases skipped).
- **`gv outdated`** — read-only drift report. Prints `NAME / LOCKED /
  LATEST / STATUS` for the toolchain plus every pinned tool, in parallel.
  Exits 2 when anything is behind, so it slots straight into CI gating.
- **`gv migrate-tools [--from FILE] [--dry-run]`** — discovers files with a
  `//go:build tools` (or `// +build tools`) constraint, parses the blank
  imports, resolves each to a Go module via the proxy, and pins them in
  `gv.toml`. Migration helper for projects coming from the legacy
  tools.go pattern.
- **`gv completions <SHELL>`** — emits bash / zsh / fish / elvish /
  PowerShell completions on stdout via clap_complete.

## [0.2.1] — 2026-04-27

### Added

- **`gv tool {list,ls,registry,add,remove}`** — first-class tool subcommand
  group. `list` prints `NAME / REQUESTED / LOCKED / STATUS` from gv.toml
  ⊕ gv.lock; `registry` lists the built-in name → package map; `remove`
  drops a tool from gv.toml + gv.lock (binary lingers in the store until
  `gv cache prune`). `gv add tool` keeps working as an alias.
- **`install.ps1`** — PowerShell installer mirroring `install.sh` for
  Windows users (downloads tar.gz, sha256-verifies, drops gv.exe into
  `%LOCALAPPDATA%\gv\bin`).
- **CI matrix grows windows-latest** so fmt + clippy + build + test run
  on every push for all three target families.

## [0.2.0] — 2026-04-27

### Added

- **`gv init`** — bootstrap a `gv.toml` in the current directory. Honors
  `go.mod` toolchain / `.go-version` for the toolchain pin or falls back to
  the latest stable Go release. `--with foo,bar` preselects tools, `--go
  X.Y.Z` overrides the toolchain pin, `--force` overwrites an existing file.
- **`gv self-update`** — fetch the latest `O6lvl4/gv` release, sha256-verify,
  atomic-replace the running binary plus its sibling `gv-shim`. `--check`
  reports without installing. Skips if already on the newest release.
- **Windows support** (phase 2 MVP) — `x86_64-pc-windows-msvc` is now built
  in the release matrix. The toolchain installer auto-detects `.zip` vs
  `.tar.gz` archives via the file extension. Tool binaries inherit the
  host's `.exe` suffix. `gv link` / `gv unlink` on Windows copy the shim
  with a sidecar marker file (no symlink privilege required) and refuse to
  remove unmanaged binaries.

### Performance

- **Parallel resolve + install** in `gv sync` and `gv add tool`. Tool resolution
  fans out via `try_join_all`; tool builds run on `tokio::task::spawn_blocking`
  pools. Three-tool sync went from ~50 s sequential to ~31 s wall on a
  3-tool project (CPU 175%, network-bound on `go install` downloads).
- **uv-style summary lines** with `indicatif` spinners per tool and timings:
  `Resolved N tools in 234 ms` / `Built N tools in 4.5 s` plus a `+` (new) /
  `~` (changed) / `=` (unchanged) marker per tool.
- **`gv tree`** — hierarchical view of the resolved environment: toolchain
  with its source, then each pinned tool with package, module hash,
  built-with version, and binary path. Highlights `present` vs `missing`.
- **`gv upgrade [TOOL...] [--toolchain]`** — re-resolves `@latest` for the
  named tools (or all pinned tools when no name is given). Only re-installs
  what actually moved; reports `name: old → new` cleanly. `--toolchain`
  also bumps the Go release to the latest stable.
- **`gv cache info`** — disk usage breakdown across `store/`, `versions/`,
  `tools/`, `cache/`, `config/` with humanized sizes.
- **`gv cache prune [--dry-run]`** — removes content-addressed store entries
  no longer referenced by any `versions/` symlink.

## [0.1.0] — pre-release

### Added

- `gv install <version>` — download Go from go.dev, sha256-verify, extract into
  the content-addressed store
- `gv list` / `gv list --remote` — installed and remote toolchains
- `gv current` — print the active version *and* the resolution source
- `gv which [tool]` — full path to the resolved binary
- `gv use-global <version>` — set `~/.config/gv/global`
- `gv run <cmd> [args...]` — run with `GOROOT` + `GOTOOLCHAIN=local` injected;
  resolves project-pinned tools first
- `gv add tool <name>[@<version>]` — pin a Go tool in `gv.toml`, install it,
  record it in `gv.lock`
- `gv sync [--frozen]` — reconcile installs with the lock; `--frozen` is the
  CI mode and refuses to update the lock
- `gv link` / `gv unlink` — install/remove `gv-shim` symlinks (`go`, `gofmt`,
  …) in `~/.local/bin`. Refuses to clobber non-symlink files unless `--force`
- `gv doctor` — health check (paths, installed versions, project state)
- `gv-shim` — sub-millisecond `execve` dispatch based on `argv[0]`

### Reproducibility

- Reads `go.mod`'s `toolchain` line as a first-class source (no `gv`-specific
  config needed for projects that already pin)
- `gv.lock` records: Go archive sha256, per-tool module hash from
  `sum.golang.org/lookup` (the same hash that ends up in `go.sum`), the Go
  toolchain that built each tool, and the resulting binary sha256
- `gv sync --frozen` rejects drift between `gv.toml` and `gv.lock`

### Distribution

- `install.sh` — one-shot `curl ... | sh` installer with sha256 verification
- GitHub Actions release matrix builds:
  - `aarch64-apple-darwin`
  - `x86_64-apple-darwin`
  - `x86_64-unknown-linux-musl`
  - `aarch64-unknown-linux-musl`
- Homebrew formula template under `packaging/homebrew/gv.rb.template`

### Internals

- Pure Rust: tokio + reqwest with `rustls-tls` (no OpenSSL)
- Content-addressed store at `~/.local/share/gv/store/<sha-prefix>/` with a
  `versions/` symlink farm
- Module-path resolution walks up the proxy until a module is found, mirroring
  how `go install` resolves package → module
- `gv-shim` builds with `panic = "abort"`, `lto = "fat"`, `opt-level = "z"`
  for ~400 KB on macOS arm64
