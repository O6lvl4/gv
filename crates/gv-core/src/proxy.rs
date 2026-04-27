//! Talk to the Go module proxy (proxy.golang.org by default).
//!
//! Endpoints used:
//!   GET /<encoded-module>/@latest          → JSON {Version, Time}
//!   GET /<encoded-module>/@v/<version>.info → JSON {Version, Time}
//!   GET /<encoded-module>/@v/<version>.ziphash → text "h1:base64\n"

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

const PROXY_BASE: &str = "https://proxy.golang.org";

#[derive(Debug, Clone, Deserialize)]
pub struct VersionInfo {
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "Time")]
    pub time: Option<String>,
}

pub async fn latest(client: &reqwest::Client, module: &str) -> Result<VersionInfo> {
    let url = format!("{PROXY_BASE}/{}/@latest", encode_path(module));
    let res = client.get(&url).send().await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?;
    Ok(res.json().await?)
}

/// Find the longest prefix of `package_path` that the proxy recognizes as a
/// module. Returns (module, version_info_for_latest).
///
/// The Go module proxy doesn't give us a package→module lookup, so we walk
/// up the path trying @latest at each prefix. This mirrors how `go install`
/// resolves modules itself.
pub async fn find_module(client: &reqwest::Client, package_path: &str) -> Result<(String, VersionInfo)> {
    let mut candidate: &str = package_path;
    loop {
        let url = format!("{PROXY_BASE}/{}/@latest", encode_path(candidate));
        let res = client.get(&url).send().await
            .with_context(|| format!("GET {url}"))?;
        let status = res.status();
        if status.is_success() {
            let info: VersionInfo = res.json().await?;
            return Ok((candidate.to_string(), info));
        } else if status.as_u16() == 404 || status.as_u16() == 410 {
            // Try the parent path.
            match candidate.rfind('/') {
                Some(i) if i > 0 => candidate = &candidate[..i],
                _ => return Err(anyhow!("could not resolve any module path containing {package_path}")),
            }
        } else {
            return Err(anyhow!("unexpected status {status} for {url}"));
        }
    }
}

/// Look up the module's directory hash (`h1:...`) from the Go checksum
/// database at sum.golang.org. This is the canonical source — the same hash
/// that ends up in your `go.sum`.
pub async fn ziphash(client: &reqwest::Client, module: &str, version: &str) -> Result<String> {
    let url = format!(
        "https://sum.golang.org/lookup/{}@{}",
        encode_path(module),
        version
    );
    let res = client.get(&url).send().await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?;
    let body = res.text().await?;
    parse_sumdb_lookup(&body, module, version)
}

/// Parse the sum.golang.org `lookup` response body. Format:
///   <signed-tree-id>
///   <module> <version> h1:<hash>
///   <module> <version>/go.mod h1:<hash>
///
///   <signature>
fn parse_sumdb_lookup(body: &str, module: &str, version: &str) -> Result<String> {
    let prefix = format!("{module} {version} ");
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix(&prefix) {
            // First match (without `/go.mod`) is the module zip hash.
            return Ok(rest.trim().to_string());
        }
    }
    Err(anyhow!(
        "could not find h1 hash for {module}@{version} in sumdb response"
    ))
}

/// Module proxy case-encoding: every uppercase letter is replaced by `!` plus
/// the lowercase form. See https://go.dev/ref/mod#goproxy-protocol.
pub fn encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_encoding() {
        assert_eq!(encode_path("github.com/Foo/Bar"), "github.com/!foo/!bar");
        assert_eq!(encode_path("golang.org/x/tools/gopls"), "golang.org/x/tools/gopls");
        assert_eq!(encode_path("github.com/Microsoft/go-winio"), "github.com/!microsoft/go-winio");
    }
}
