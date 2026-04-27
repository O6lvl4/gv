# gv

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

🚧 Pre-alpha. MVP in progress. Track [milestones](https://github.com/O6lvl4/gv/milestones).

## Install

```bash
# Once published:
brew install O6lvl4/tap/gv
# or
cargo install gv-cli
```

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
