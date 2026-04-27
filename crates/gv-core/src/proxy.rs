//! Talk to the Go module proxy. Honors GOPROXY (comma-separated, with the
//! "direct" / "off" sentinels filtered out). Falls back to proxy.golang.org
//! when nothing is set.
//!
//! Endpoints used (per https://go.dev/ref/mod#goproxy-protocol):
//!   GET /<encoded-module>/@latest          → JSON {Version, Time}
//!   GET /<encoded-module>/@v/list          → newline-separated versions
//!   GET /<encoded-module>/@v/<version>.info → JSON {Version, Time}
//!   GET /<encoded-module>/@v/<version>.ziphash → text "h1:base64\n"

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

pub const DEFAULT_PROXY: &str = "https://proxy.golang.org";

#[derive(Debug, Clone, Deserialize)]
pub struct VersionInfo {
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "Time")]
    pub time: Option<String>,
}

/// Resolve the active proxy chain. Mirrors the Go runtime's GOPROXY semantics:
/// comma- (or pipe-) separated list, with `direct` / `off` filtered out — the
/// remaining entries are HTTPS endpoints we actually fetch from.
pub fn proxy_chain() -> Vec<String> {
    let raw = std::env::var("GOPROXY").unwrap_or_else(|_| DEFAULT_PROXY.to_string());
    let mut out = Vec::new();
    for chunk in raw.split([',', '|']) {
        let s = chunk.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("direct") || s.eq_ignore_ascii_case("off") {
            continue;
        }
        let trimmed = s.trim_end_matches('/').to_string();
        out.push(trimmed);
    }
    if out.is_empty() {
        out.push(DEFAULT_PROXY.to_string());
    }
    out
}

/// Try each configured proxy in turn for `<base>/<encoded-module>/<suffix>`.
/// Returns the first 2xx response. On 404/410 across the whole chain, treat
/// the resource as missing.
async fn try_chain(
    client: &reqwest::Client,
    encoded: &str,
    suffix: &str,
) -> Result<Option<reqwest::Response>> {
    let mut last_err: Option<anyhow::Error> = None;
    let mut all_404 = true;
    for base in proxy_chain() {
        let url = format!("{base}/{encoded}{suffix}");
        match client.get(&url).send().await {
            Ok(res) if res.status().is_success() => return Ok(Some(res)),
            Ok(res) => {
                let code = res.status().as_u16();
                if code != 404 && code != 410 {
                    all_404 = false;
                    last_err = Some(anyhow!("HTTP {code} from {url}"));
                }
            }
            Err(e) => {
                all_404 = false;
                last_err = Some(anyhow::Error::new(e).context(format!("GET {url}")));
            }
        }
    }
    if all_404 {
        Ok(None)
    } else {
        Err(last_err.unwrap_or_else(|| anyhow!("no proxies configured")))
    }
}

pub async fn latest(client: &reqwest::Client, module: &str) -> Result<VersionInfo> {
    let suffix = "/@latest";
    let res = try_chain(client, &encode_path(module), suffix)
        .await?
        .ok_or_else(|| anyhow!("module {module} not found in any proxy"))?;
    res.json().await.context("parse @latest JSON")
}

/// Walk up `package_path` asking the proxy chain whether each prefix is a
/// module. Returns the first match. Mirrors how `go install` resolves
/// `package@version` to a containing module.
pub async fn find_module(
    client: &reqwest::Client,
    package_path: &str,
) -> Result<(String, VersionInfo)> {
    let mut candidate: &str = package_path;
    loop {
        let encoded = encode_path(candidate);
        match try_chain(client, &encoded, "/@latest").await? {
            Some(res) => {
                let info: VersionInfo = res.json().await.context("parse @latest JSON")?;
                return Ok((candidate.to_string(), info));
            }
            None => match candidate.rfind('/') {
                Some(i) if i > 0 => candidate = &candidate[..i],
                _ => {
                    return Err(anyhow!(
                        "could not resolve any module path containing {package_path}"
                    ))
                }
            },
        }
    }
}

/// Fetch the full version list for a module from the proxy chain. Returns
/// versions in declaration order; callers filter via semver.
pub async fn list_versions(client: &reqwest::Client, module: &str) -> Result<Vec<String>> {
    let res = try_chain(client, &encode_path(module), "/@v/list")
        .await?
        .ok_or_else(|| anyhow!("module {module} not found in any proxy"))?;
    let body = res.text().await?;
    Ok(body
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// Look up the module's directory hash (`h1:...`) from sum.golang.org. This
/// hits a fixed endpoint independent of GOPROXY because the checksum DB is the
/// canonical authority for `go.sum` verification.
pub async fn ziphash(client: &reqwest::Client, module: &str, version: &str) -> Result<String> {
    let sumdb = std::env::var("GOSUMDB").unwrap_or_else(|_| "sum.golang.org".to_string());
    if sumdb.eq_ignore_ascii_case("off") {
        return Err(anyhow!(
            "GOSUMDB=off — cannot retrieve h1: hash. Set GOSUMDB to a verifier or unset it."
        ));
    }
    // GOSUMDB may be just a host or "host+key" or a full URL; we only care
    // about the host portion.
    let host = sumdb
        .split_whitespace()
        .next()
        .unwrap_or("sum.golang.org")
        .trim_end_matches('/');
    let host = host.strip_prefix("https://").unwrap_or(host);
    let host = host.strip_prefix("http://").unwrap_or(host);
    let url = format!("https://{host}/lookup/{}@{}", encode_path(module), version);
    let res = client
        .get(&url)
        .send()
        .await
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
        assert_eq!(
            encode_path("golang.org/x/tools/gopls"),
            "golang.org/x/tools/gopls"
        );
        assert_eq!(
            encode_path("github.com/Microsoft/go-winio"),
            "github.com/!microsoft/go-winio"
        );
    }

    // Note: GOPROXY semantics are exercised through proxy_chain by setting
    // the env var. We avoid a dedicated test here because that env var is
    // process-global and unit tests run in parallel — a custom GOPROXY in
    // the developer's shell would also poison a default-value assertion.
    // The chain logic is small and verified end-to-end via integration runs.
}
