use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::Arc;

use crate::logger::Logger;
use crate::server::{write_response, write_response_with_headers};

// Both renderers' main/src/proxy.ts allowlists combined (kept in sync by
// the `proxy_hosts_match_both_vendors` test) — this is how the renderers
// reach poe.ninja/trade-site data without a CORS-related dead end.
const PROXY_HOSTS: &[&str] = &[
    "www.pathofexile.com",
    "ru.pathofexile.com",
    "pathofexile.tw",
    "poe.kakaogames.com",
    "poe.ninja",
    "www.poeprices.info",
    // poe2 only: its localized trade realms and item-drop data.
    "jp.pathofexile.com",
    "de.pathofexile.com",
    "es.pathofexile.com",
    "br.pathofexile.com",
    "fr.pathofexile.com",
    "api.exiledexchange2.dev",
];

// accept-encoding too: the body goes back to the renderer without its
// content-encoding header (Electron's net.request also hands proxy.ts a
// decoded body), so the browser's "br, zstd" reached poe.ninja and EE2's
// api and the renderer got raw compressed bytes. Dropped, ureq asks only
// for what it decodes itself.
const STRIPPED_REQUEST_HEADERS: &[&str] = &["host", "origin", "content-length", "user-agent", "accept-encoding"];

// Overridden rather than forwarded: our WebView's own User-Agent has
// "Electron/32.0.0" tacked on purely as a local trick to satisfy the
// renderer's navigator.userAgent-based overlay-mode check (IPC.ts). Sent
// externally, that non-standard string looks like a bot signature to
// Cloudflare (which fronts pathofexile.com) and got every request
// rate-limited. Real APT's own proxy.ts does the same override
// (app.userAgentFallback) rather than passing the renderer's UA through.
const OUTBOUND_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";

/// Request headers passed on upstream (proxy.ts's delete loop); keys are
/// already lowercased by server.rs.
fn forwarded_headers(headers: &HashMap<String, String>) -> impl Iterator<Item = (&str, &str)> {
    headers
        .iter()
        .filter(|(key, _)| !key.starts_with("sec-") && !STRIPPED_REQUEST_HEADERS.contains(&key.as_str()))
        .map(|(key, value)| (key.as_str(), value.as_str()))
}

pub fn handle(
    stream: &mut TcpStream,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
    logger: &Arc<Logger>,
) -> std::io::Result<()> {
    let rest = path.strip_prefix("/proxy/").unwrap_or("");
    let host = rest.split('/').next().unwrap_or("");

    if !PROXY_HOSTS.contains(&host) {
        return write_response(stream, 403, "text/plain", b"host not allowed");
    }

    let url = format!("https://{rest}");
    // One shared agent so requests reuse connections instead of a fresh TLS
    // handshake per price check. 4xx/5xx are passed through as responses,
    // not errors: the renderer needs GGG's real status (429) and its
    // rate-limit headers, not a generic 502.
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    let agent = AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            // Bounds how long a hung upstream holds this connection's
            // thread (and the renderer's request).
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build()
            .into()
    });

    // Method passed through like proxy.ts's `method: req.method`; a body
    // is only sent when the client sent one.
    let mut request = ureq::http::Request::builder()
        .method(method)
        .uri(&url)
        .header("user-agent", OUTBOUND_USER_AGENT);
    for (key, value) in forwarded_headers(headers) {
        request = request.header(key, value);
    }
    let result = if body.is_empty() {
        request.body(()).map_err(ureq::Error::from).and_then(|r| agent.run(r))
    } else {
        request.body(body).map_err(ureq::Error::from).and_then(|r| agent.run(r))
    };

    match result {
        Ok(mut response) => {
            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            // The renderer's own client-side RateLimiter (common.ts,
            // adjustRateLimits) learns the real GGG-side limits from these
            // response headers and paces itself accordingly. Without them
            // it falls back to a tiny hardcoded default (1 request/5s) and
            // never learns better — which made even two price checks a few
            // seconds apart trip its own "Retry after N seconds" error,
            // looking like a real rate-limit even though the actual
            // request went through fine.
            let rate_limit_headers: Vec<(String, String)> = response
                .headers()
                .iter()
                .filter(|(name, _)| name.as_str().to_ascii_lowercase().starts_with("x-rate-limit"))
                .filter_map(|(name, value)| {
                    Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
                })
                .collect();
            let rate_limit_headers: Vec<(&str, &str)> = rate_limit_headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let bytes = response.body_mut().read_to_vec().unwrap_or_default();
            write_response_with_headers(stream, status, &content_type, &rate_limit_headers, &bytes)
        }
        Err(err) => {
            logger.error(&format!("[proxy] request to {host} failed: {err}"));
            write_response(stream, 502, "text/plain", b"proxy error")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_hosts_match_both_vendors() {
        let upstream: std::collections::BTreeSet<String> = crate::game::vendor_sources("main/src/proxy.ts")
            .iter()
            .flat_map(|source| crate::game::ts_string_values(source, "host"))
            .collect();
        let ours: std::collections::BTreeSet<String> = PROXY_HOSTS.iter().map(|s| s.to_string()).collect();
        assert_eq!(ours, upstream, "PROXY_HOSTS out of sync with upstream proxy.ts");
    }

    #[test]
    fn forwards_only_non_identifying_headers() {
        let headers: HashMap<String, String> = [
            "host", "origin", "content-length", "user-agent", "accept-encoding", "sec-fetch-mode", "accept", "cookie",
        ]
        .iter()
        .map(|k| (k.to_string(), "v".to_string()))
        .collect();
        let mut forwarded: Vec<_> = forwarded_headers(&headers).map(|(k, _)| k).collect();
        forwarded.sort();
        assert_eq!(forwarded, ["accept", "cookie"]);
    }
}
