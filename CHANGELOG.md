# Changelog

All notable changes to gv are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and gv uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
