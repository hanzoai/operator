//! Minimal OCI Distribution (Docker Registry HTTP API v2) tag lister.
//!
//! Just `GET /v2/<name>/tags/list`, with the standard `WWW-Authenticate: Bearer`
//! token-challenge dance and optional dockerconfig basic auth. `reqwest` (already
//! a dependency) does the HTTP + TLS; pulling a full OCI client crate
//! (`oci-distribution`/`oci-client`) to perform one authenticated GET would be
//! dependency bloat for no gain — listing tags is not the complex, well-tested
//! thing worth a framework, image *pulling* would be, and we never pull.
//!
//! Targets `registry.hanzo.ai` (the canonical fleet registry) but works against
//! any v2 registry (ghcr.io, Docker Hub) unchanged.

use base64::Engine;
use serde::Deserialize;

use crate::core::{OperatorError, Result};

/// A parsed image repository: registry host + repository path.
#[derive(Debug, PartialEq, Eq)]
pub struct ImageRef {
    pub host: String,
    pub path: String,
}

/// Split `registry.hanzo.ai/hanzo/cloud` → host `registry.hanzo.ai`, path
/// `hanzo/cloud`. A leading `https://` is tolerated. When the first segment
/// carries no `.`/`:` (so it isn't a host) we assume the canonical fleet
/// registry — our CRs always name a host, so this is only a safety net.
pub fn parse_ref(image_repository: &str) -> ImageRef {
    let s = image_repository
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_matches('/');
    match s.split_once('/') {
        Some((host, path)) if host.contains('.') || host.contains(':') => ImageRef {
            host: host.to_string(),
            path: path.trim_matches('/').to_string(),
        },
        _ => ImageRef {
            host: "registry.hanzo.ai".to_string(),
            path: s.to_string(),
        },
    }
}

#[derive(Deserialize)]
struct TagList {
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

/// List every tag of `image_repository`. `dockerconfig_json` (a
/// `.dockerconfigjson` payload) supplies basic auth for private repos; `None`
/// lists anonymously. Follows the bearer-token challenge and `Link` pagination.
pub async fn list_tags(
    image_repository: &str,
    dockerconfig_json: Option<&str>,
) -> Result<Vec<String>> {
    let r = parse_ref(image_repository);
    let basic = dockerconfig_json.and_then(|j| basic_for_host(j, &r.host));
    let client = reqwest::Client::builder()
        .user_agent("hanzo-operator")
        .build()?;

    let mut url = format!("https://{}/v2/{}/tags/list?n=1000", r.host, r.path);
    let mut bearer: Option<String> = None;
    let mut tags: Vec<String> = Vec::new();

    loop {
        let mut req = client.get(&url);
        if let Some(b) = &bearer {
            req = req.bearer_auth(b);
        } else if let Some(a) = &basic {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Basic {a}"));
        }
        let resp = req.send().await?;

        // First 401 → solve the bearer challenge and retry the SAME url once.
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED && bearer.is_none() {
            let challenge = resp
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|h| h.to_str().ok())
                .map(str::to_string)
                .ok_or_else(|| {
                    OperatorError::Reconcile(format!(
                        "registry {} returned 401 with no WWW-Authenticate challenge",
                        r.host
                    ))
                })?;
            bearer = Some(fetch_bearer(&client, &challenge, basic.as_deref()).await?);
            continue;
        }

        let next = next_page(&resp, &r.host);
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            let head: String = body.chars().take(200).collect();
            return Err(OperatorError::Reconcile(format!(
                "registry {} list {}: HTTP {status}: {head}",
                r.host, r.path
            )));
        }
        let list: TagList = serde_json::from_str(&body).map_err(|e| {
            OperatorError::Reconcile(format!("registry {} tag list decode: {e}", r.host))
        })?;
        if let Some(t) = list.tags {
            tags.extend(t);
        }
        match next {
            Some(n) => url = n,
            None => break,
        }
    }
    Ok(tags)
}

/// Resolve a `WWW-Authenticate: Bearer realm=...,service=...,scope=...`
/// challenge into an access token.
async fn fetch_bearer(
    client: &reqwest::Client,
    challenge: &str,
    basic: Option<&str>,
) -> Result<String> {
    let rest = challenge
        .trim()
        .strip_prefix("Bearer ")
        .or_else(|| challenge.trim().strip_prefix("bearer "))
        .ok_or_else(|| OperatorError::Reconcile("unsupported auth scheme (not Bearer)".into()))?;
    let realm = challenge_param(rest, "realm")
        .ok_or_else(|| OperatorError::Reconcile("bearer challenge missing realm".into()))?;

    let mut params: Vec<(&str, String)> = Vec::new();
    if let Some(s) = challenge_param(rest, "service") {
        params.push(("service", s));
    }
    if let Some(s) = challenge_param(rest, "scope") {
        params.push(("scope", s));
    }
    // Url::parse_with_params percent-encodes correctly (scope carries `:`),
    // avoiding RequestBuilder::query (whose serde_urlencoded backing isn't in
    // our trimmed reqwest feature set).
    let url = reqwest::Url::parse_with_params(&realm, params.iter().map(|(k, v)| (*k, v.as_str())))
        .map_err(|e| OperatorError::Reconcile(format!("token realm url: {e}")))?;

    let mut req = client.get(url);
    if let Some(a) = basic {
        req = req.header(reqwest::header::AUTHORIZATION, format!("Basic {a}"));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(OperatorError::Reconcile(format!(
            "token endpoint {realm}: HTTP {}",
            resp.status()
        )));
    }
    let tr: TokenResponse = resp.json().await.map_err(OperatorError::from)?;
    tr.token
        .or(tr.access_token)
        .ok_or_else(|| OperatorError::Reconcile("token endpoint returned no token".into()))
}

/// Extract a quoted parameter (`key="value"`) from the challenge parameter list.
fn challenge_param(params: &str, key: &str) -> Option<String> {
    for part in params.split(',') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(key) {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                return Some(rest.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

/// Parse the `Link: <url>; rel="next"` pagination header into an absolute URL.
fn next_page(resp: &reqwest::Response, host: &str) -> Option<String> {
    let link = resp.headers().get(reqwest::header::LINK)?.to_str().ok()?;
    // `</v2/x/tags/list?n=1000&last=z>; rel="next"`
    let start = link.find('<')?;
    let end = link[start..].find('>')? + start;
    if !link[end..].contains("rel=\"next\"") && !link[end..].contains("rel=next") {
        return None;
    }
    let target = &link[start + 1..end];
    if target.starts_with("http") {
        Some(target.to_string())
    } else {
        Some(format!("https://{host}{target}"))
    }
}

/// Pull the base64 `auth` (i.e. `base64(user:pass)`) for `host` out of a
/// `.dockerconfigjson` payload. Matches the host key with or without an
/// `https://` prefix.
fn basic_for_host(dockerconfig_json: &str, host: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(dockerconfig_json).ok()?;
    let auths = v.get("auths")?.as_object()?;
    for (k, entry) in auths {
        let kh = k
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/');
        if kh == host {
            if let Some(a) = entry.get("auth").and_then(|a| a.as_str()) {
                return Some(a.to_string());
            }
            // Some configs carry username/password instead of a joined `auth`.
            if let (Some(u), Some(p)) = (
                entry.get("username").and_then(|u| u.as_str()),
                entry.get("password").and_then(|p| p.as_str()),
            ) {
                return Some(base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}")));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ref_splits_host_and_path() {
        assert_eq!(
            parse_ref("registry.hanzo.ai/hanzo/cloud"),
            ImageRef {
                host: "registry.hanzo.ai".into(),
                path: "hanzo/cloud".into()
            }
        );
        assert_eq!(
            parse_ref("https://ghcr.io/hanzoai/studio"),
            ImageRef {
                host: "ghcr.io".into(),
                path: "hanzoai/studio".into()
            }
        );
        assert_eq!(
            parse_ref("registry.hanzo.ai:5000/hanzo/cms"),
            ImageRef {
                host: "registry.hanzo.ai:5000".into(),
                path: "hanzo/cms".into()
            }
        );
    }

    #[test]
    fn parse_ref_hostless_falls_back_to_fleet_registry() {
        assert_eq!(
            parse_ref("hanzo/cloud"),
            ImageRef {
                host: "registry.hanzo.ai".into(),
                path: "hanzo/cloud".into()
            }
        );
    }

    #[test]
    fn challenge_param_extracts_quoted_values() {
        let c = r#"realm="https://auth.hanzo.ai/token",service="registry.hanzo.ai",scope="repository:hanzo/cloud:pull""#;
        assert_eq!(
            challenge_param(c, "realm").as_deref(),
            Some("https://auth.hanzo.ai/token")
        );
        assert_eq!(
            challenge_param(c, "service").as_deref(),
            Some("registry.hanzo.ai")
        );
        assert_eq!(
            challenge_param(c, "scope").as_deref(),
            Some("repository:hanzo/cloud:pull")
        );
        assert_eq!(challenge_param(c, "missing"), None);
    }

    #[test]
    fn basic_for_host_reads_auth_or_userpass() {
        let cfg = r#"{"auths":{"registry.hanzo.ai":{"auth":"YWJjOnh5eg=="}}}"#;
        assert_eq!(
            basic_for_host(cfg, "registry.hanzo.ai").as_deref(),
            Some("YWJjOnh5eg==")
        );
        assert_eq!(basic_for_host(cfg, "ghcr.io"), None);

        let cfg2 =
            r#"{"auths":{"https://registry.hanzo.ai/":{"username":"abc","password":"xyz"}}}"#;
        // base64("abc:xyz") == "YWJjOnh5eg=="
        assert_eq!(
            basic_for_host(cfg2, "registry.hanzo.ai").as_deref(),
            Some("YWJjOnh5eg==")
        );
    }
}
