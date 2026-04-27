# gv

[![ci](https://github.com/O6lvl4/gv/actions/workflows/ci.yml/badge.svg)](https://github.com/O6lvl4/gv/actions/workflows/ci.yml)
[![release](https://github.com/O6lvl4/gv/actions/workflows/release.yml/badge.svg)](https://github.com/O6lvl4/gv/actions/workflows/release.yml)
[![license](https://img.shields.io/github/license/O6lvl4/gv)](LICENSE)

> Go version & toolchain manager. uv-grade speed. Single binary. Reproducible.

`gv` is to Go what `uv` is to Python: one fast Rust binary that owns toolchains,
project tooling, and reproducible installs — without the activation overhead and
shell pollution of legacy version managers.

## Why gv

- **Reads `go.mod`'s `toolchain` line as a first-class source.** Existing managers ignore it; `gv` makes it canonical. Zero migration cost.
- **Replaces `go install` for global tools.** Pin `gopls`, `golangci-lint`, `dlv`, `mockgen`, `sqlc` in `gv.toml`, lock to `gv.lock`, reproduce on CI.
- **Content-addressed store.** SDKs and tools deduped by sha256 across projects (pnpm/uv-style).
- **No shell activation required.** Optional 1ms `execve` shim for IDE compatibility.
- **Parallel downloads, parallel extraction.** Tokio + reqwest + rustls. No OpenSSL.
- **`GOTOOLCHAIN=local` enforced.** `gv` owns toolchain selection — no surprise downloads from the Go runtime.

## Status

🚧 Pre-alpha. Working today:

- Toolchain install / list / current / use-global / doctor
- Content-addressed store + symlink farm
- `go.mod` `toolchain` line read as a first-class source
- `.go-version` + `~/.config/gv/global` fallbacks
- **Tool pinning**: `gv add tool gopls` resolves via `proxy.golang.org`, fetches
  the directory hash from `sum.golang.org` (the canonical Go checksum DB),
  builds via the resolved Go toolchain, records everything in `gv.lock`
- `gv sync --frozen` for CI: refuses to touch the lock, fails if `gv.toml`
  drifts ahead of `gv.lock`
- `gv-shim` 400KB binary for IDE-side `execve` dispatch

Track [milestones](https://github.com/O6lvl4/gv/milestones).

## Install

```bash
# macOS / Linux — install.sh downloads the latest release for your platform
curl -fsSL https://raw.githubusercontent.com/O6lvl4/gv/main/install.sh | sh

# pin a version
GV_VERSION=v0.2.0 curl -fsSL https://raw.githubusercontent.com/O6lvl4/gv/main/install.sh | sh

# from source
cargo install --git https://github.com/O6lvl4/gv gv-cli
```

```powershell
# Windows — install.ps1 mirrors install.sh in PowerShell
iwr https://raw.githubusercontent.com/O6lvl4/gv/main/install.ps1 | iex

# pin a version
$env:GV_VERSION = "v0.2.0"
iwr https://raw.githubusercontent.com/O6lvl4/gv/main/install.ps1 | iex
```

After installing, optionally hook the toolchain shim:

```bash
gv link            # creates ~/.local/bin/{go,gofmt} → gv-shim
gv link --tools go,gofmt,godoc
```

This makes `go build` etc. dispatch through `gv-shim`, which honors
`go.mod`'s `toolchain` line and `.go-version` automatically.

### Homebrew

```bash
brew install O6lvl4/tap/gv
```

The tap lives at [O6lvl4/homebrew-tap](https://github.com/O6lvl4/homebrew-tap)
and is bumped automatically on every stable release by the `bump-tap` job in
`.github/workflows/release.yml` (driven by the
[`packaging/homebrew/gv.rb.template`](packaging/homebrew/gv.rb.template)
template). Pre-release tags (those containing `-`, e.g. `v0.2.0-rc1`) skip the
bump.

> **Maintainer note**: the bump job authenticates with a deploy key.
> `O6lvl4/homebrew-tap` carries a read-write SSH deploy key whose private
> half is stored as the `TAP_DEPLOY_KEY` secret on `O6lvl4/gv`. Rotating is
> a two-step `gh repo deploy-key` + `gh secret set` dance — no PATs needed.

## Quickstart

```bash
gv add 1.25.0                      # add toolchain (writes go.mod toolchain line)
gv add tool gopls                  # pin a tool
gv add tool golangci-lint@v1.64    # version-pinned tool
gv sync                            # reconcile installs with gv.lock
gv run go test ./...               # run with the resolved toolchain
gv run gopls                       # tools work the same way
gv tree                            # visualize resolution
gv current                         # explain why this version is active
gv doctor                          # health check
```

## Resolution order

1. `GV_VERSION` env var
2. `go.mod` `toolchain` line (walking up from CWD)
3. `.go-version` (walking up from CWD)
4. `~/.config/gv/global`
5. Latest installed

`gv current` always prints the chosen version *and* the reason.

## Design

See [docs/DESIGN.md](docs/DESIGN.md).

## License

MIT
