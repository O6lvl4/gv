//! Built-in registry of common Go tools. Keeps `gv add tool gopls` short.
//!
//! Users can always specify a full package path explicitly in `gv.toml`,
//! in which case the registry is bypassed.

#[derive(Debug, Clone, Copy)]
pub struct RegistryEntry {
    pub name: &'static str,
    pub package: &'static str,
}

const ENTRIES: &[RegistryEntry] = &[
    RegistryEntry { name: "gopls",            package: "golang.org/x/tools/gopls" },
    RegistryEntry { name: "goimports",        package: "golang.org/x/tools/cmd/goimports" },
    RegistryEntry { name: "staticcheck",      package: "honnef.co/go/tools/cmd/staticcheck" },
    RegistryEntry { name: "golangci-lint",    package: "github.com/golangci/golangci-lint/cmd/golangci-lint" },
    RegistryEntry { name: "dlv",              package: "github.com/go-delve/delve/cmd/dlv" },
    RegistryEntry { name: "mockgen",          package: "go.uber.org/mock/mockgen" },
    RegistryEntry { name: "sqlc",             package: "github.com/sqlc-dev/sqlc/cmd/sqlc" },
    RegistryEntry { name: "buf",              package: "github.com/bufbuild/buf/cmd/buf" },
    RegistryEntry { name: "air",              package: "github.com/air-verse/air" },
    RegistryEntry { name: "protoc-gen-go",    package: "google.golang.org/protobuf/cmd/protoc-gen-go" },
    RegistryEntry { name: "protoc-gen-go-grpc", package: "google.golang.org/grpc/cmd/protoc-gen-go-grpc" },
    RegistryEntry { name: "wire",             package: "github.com/google/wire/cmd/wire" },
    RegistryEntry { name: "swag",             package: "github.com/swaggo/swag/cmd/swag" },
    RegistryEntry { name: "migrate",          package: "github.com/golang-migrate/migrate/v4/cmd/migrate" },
    RegistryEntry { name: "goreleaser",       package: "github.com/goreleaser/goreleaser/v2" },
];

pub fn lookup(name: &str) -> Option<RegistryEntry> {
    ENTRIES.iter().copied().find(|e| e.name == name)
}

pub fn all() -> &'static [RegistryEntry] {
    ENTRIES
}
