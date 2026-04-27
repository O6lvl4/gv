# gv — Design

> Goals, non-goals, and rationale. Code should reflect this. If they diverge,
> code wins and this document gets updated.

## Mission

Go's tooling story has lagged Python's renaissance. `uv` proved that a single
fast Rust binary can replace a fleet of legacy tools (pyenv, virtualenv, pip,
pip-tools). `gv` is the equivalent move for Go: one binary that owns toolchain
selection, project tooling, and reproducible installs — without shell-activation
overhead, plugin sprawl, or surprise downloads.

## Non-goals

- Replacing `go.mod` / Go modules. `go.mod` stays canonical.
- Replacing the official `go` binary. We dispatch to it.
- Cross-language scope. (See `mise`/`asdf` for that.)
- Reinventing `go install`. We extend it with version pinning and a lockfile.

## First principles

1. **`go.mod` is the source of truth.** If a project pins a toolchain in
   `go.mod`, `gv` honors it without any `gv`-specific config file.
2. **Predictable resolution.** `gv current` always prints the active version
   *and* the reason. Surprise is the bug.
3. **Reproducible installs.** Every SDK and tool is sha256-pinned in `gv.lock`.
4. **No shell activation.** `gv run` is the primary path. Shims exist for
   IDE compatibility but are sub-millisecond and stateless.
5. **Content-addressed.** Same SDK across N projects = one copy on disk.
6. **No OpenSSL.** rustls + reqwest. Static binary, single file install.

## Resolution order

`gv` resolves the active version by walking these in order:

1. `GV_VERSION` env var
2. `go.mod` `toolchain` line (CWD walk-up)
3. `.go-version` (CWD walk-up)
4. `~/.config/gv/global`
5. Latest installed toolchain

The selected version and the *source* are always available via `gv current`.

## Filesystem layout

```
~/.local/share/gv/
├── store/
│   └── <sha256-prefix>/      # content-addressed, sha-keyed
│       ├── bin/go
│       ├── src/...
│       └── .gv-installed
├── versions/
│   ├── go1.25.0  -> ../store/abcd…
│   └── go1.24.3  -> ../store/ef01…
└── bin/
    └── go        # shim; argv[0] = tool name
~/.config/gv/
└── global        # one line: "go1.25.0"
~/.cache/gv/
└── <download-tmp>
```

## Project layout (planned for Day 2+)

```
project/
├── go.mod        # toolchain go1.25.0   ← canonical
├── gv.toml       # gv-specific extensions
└── gv.lock       # auto-generated; commit this
```

`gv.toml` is optional; without it `gv` works off `go.mod` alone. With it:

```toml
[tools]
gopls = "latest"
golangci-lint = "v1.64"
dlv = "*"

[scripts]
test = "go test ./..."
lint = "golangci-lint run"
```

## Roadmap (MVP → 0.1)

- [x] Day 1: skeleton, `gv install <version>`, `gv list`, content-addressed store
- [ ] Day 2: `gv.toml` + `gv.lock` + `go.mod` toolchain bidirectional sync
- [ ] Day 3: shim binary, `gv run`, full resolution chain (env / go.mod / .go-version / global)
- [ ] Day 4: tools (`gv add tool`, `gv sync`), `go install` replacement
- [ ] Day 5: `gv tree`, `gv doctor`, `gv current`
- [ ] Day 6: cache, workspace, `--frozen`, `gv prune`
- [ ] Day 7: Homebrew tap, GitHub Actions release matrix, demo gif

## Comparison to existing tools

| Tool      | Lang      | Activation | Lockfile | Tool pinning | Reads `go.mod` toolchain |
|-----------|-----------|------------|----------|--------------|--------------------------|
| goenv     | bash      | yes        | no       | no           | no                       |
| g         | Go        | no         | no       | no           | no                       |
| asdf      | bash      | yes        | partial  | no           | no                       |
| mise      | Rust      | yes        | partial  | partial      | no                       |
| **gv**    | **Rust**  | **no**     | **yes**  | **yes**      | **yes**                  |
