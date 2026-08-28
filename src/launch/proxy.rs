//! Local API-proxy mode for `llmenv launch claude_code` (#1289). See
//! `docs/superpowers/specs/2026-08-28-launch-api-proxy-design.md`.

use anyhow::Context;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use llmenv_config::{
    ProxyCheck, ProxyCondition, ProxyConditionTarget, ProxyOp, ProxyRule, ProxyTarget,
};

/// Apply every rule in `rules`, in order, to `headers`/`body`. A rule with a
/// `when` clause whose conditions don't all match is skipped. Application
/// failures (missing `Remove`/`Strip` target) are logged and skipped, never
/// fatal — see the design spec's Error handling section.
fn apply_rules(rules: &[ProxyRule], headers: &mut http::HeaderMap, body: &mut serde_json::Value) {
    for rule in rules {
        if !rule
            .when
            .iter()
            .all(|c| condition_matches(c, headers, body))
        {
            continue;
        }
        apply_op(rule, headers, body);
    }
}

fn condition_matches(
    cond: &ProxyCondition,
    headers: &http::HeaderMap,
    body: &serde_json::Value,
) -> bool {
    match &cond.target {
        ProxyConditionTarget::Header { name } => {
            let value = headers.get(name).and_then(|v| v.to_str().ok());
            check_matches_str(&cond.check, value)
        }
        ProxyConditionTarget::Body { path: None } => {
            let Ok(serialized) = serde_json::to_string(body) else {
                return false;
            };
            check_matches_str(&cond.check, Some(&serialized))
        }
        ProxyConditionTarget::Body { path: Some(path) } => {
            let Ok(segments) = llmenv_config::parse_path(path) else {
                tracing::warn!(
                    "launch proxy: unparseable path '{path}' at request time, skipping condition"
                );
                return false;
            };
            let found = llmenv_config::get_path(body, &segments);
            match &cond.check {
                ProxyCheck::Missing => found.is_none(),
                ProxyCheck::Present => found.is_some(),
                ProxyCheck::Equals { value: expected } => found == Some(expected),
                ProxyCheck::Matches { pattern, regex } => {
                    let Some(found) = found else { return false };
                    let text = found
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| found.to_string());
                    matches_pattern(pattern, *regex, &text)
                }
            }
        }
    }
}

fn check_matches_str(check: &ProxyCheck, value: Option<&str>) -> bool {
    match check {
        ProxyCheck::Missing => value.is_none(),
        ProxyCheck::Present => value.is_some(),
        ProxyCheck::Equals { value: expected } => value
            .and_then(|v| expected.as_str().map(|e| e == v))
            .unwrap_or(false),
        ProxyCheck::Matches { pattern, regex } => {
            let Some(value) = value else { return false };
            matches_pattern(pattern, *regex, value)
        }
    }
}

fn matches_pattern(pattern: &str, is_regex: bool, text: &str) -> bool {
    if is_regex {
        match regex::Regex::new(pattern) {
            Ok(re) => re.is_match(text),
            Err(e) => {
                tracing::warn!(
                    "launch proxy: invalid regex '{pattern}' in a when condition, treating as no match: {e}"
                );
                false
            }
        }
    } else {
        text.contains(pattern)
    }
}

fn apply_op(rule: &ProxyRule, headers: &mut http::HeaderMap, body: &mut serde_json::Value) {
    match (&rule.target, &rule.op) {
        (ProxyTarget::Header { name }, ProxyOp::Set { value }) => {
            let Some(s) = value.as_str() else {
                tracing::warn!(
                    "launch proxy: header rule for '{name}' has a non-string value, skipping"
                );
                return;
            };
            let (Ok(header_name), Ok(header_value)) = (
                http::HeaderName::try_from(name.as_str()),
                http::HeaderValue::try_from(s),
            ) else {
                tracing::warn!("launch proxy: invalid header name/value for '{name}', skipping");
                return;
            };
            headers.insert(header_name, header_value);
        }
        (ProxyTarget::Header { name }, ProxyOp::Remove) => {
            headers.remove(name);
        }
        (ProxyTarget::Header { name }, ProxyOp::Strip { pattern, regex }) => {
            let Ok(header_name) = http::HeaderName::try_from(name.as_str()) else {
                tracing::warn!("launch proxy: invalid header name '{name}', skipping");
                return;
            };
            let Some(current) = headers.get(&header_name).and_then(|v| v.to_str().ok()) else {
                tracing::warn!(
                    "launch proxy: strip rule target header '{name}' is absent, skipping"
                );
                return;
            };
            let stripped = strip_pattern(pattern, *regex, current);
            match http::HeaderValue::try_from(stripped) {
                Ok(value) => {
                    headers.insert(header_name, value);
                }
                Err(e) => {
                    tracing::warn!(
                        "launch proxy: strip rule produced an invalid value for header '{name}', leaving it unchanged: {e}"
                    );
                }
            }
        }
        (ProxyTarget::Body { path }, op) => apply_body_op(path, op, body),
    }
}

fn apply_body_op(path: &str, op: &ProxyOp, body: &mut serde_json::Value) {
    let Ok(segments) = llmenv_config::parse_path(path) else {
        tracing::warn!("launch proxy: unparseable path '{path}' at request time, skipping");
        return;
    };
    match op {
        ProxyOp::Set { value } => llmenv_config::set_path(body, &segments, value.clone()),
        ProxyOp::Remove => {
            if !llmenv_config::remove_path(body, &segments) {
                tracing::warn!("launch proxy: remove rule target '{path}' is absent, skipping");
            }
        }
        ProxyOp::Strip { pattern, regex } => {
            let Some(current) = llmenv_config::get_path(body, &segments).and_then(|v| v.as_str())
            else {
                tracing::warn!(
                    "launch proxy: strip rule target '{path}' is absent or not a string, skipping"
                );
                return;
            };
            let stripped = strip_pattern(pattern, *regex, current);
            llmenv_config::set_path(body, &segments, serde_json::Value::String(stripped));
        }
    }
}

fn strip_pattern(pattern: &str, is_regex: bool, text: &str) -> String {
    if is_regex {
        match regex::Regex::new(pattern) {
            Ok(re) => re.replace_all(text, "").into_owned(),
            Err(e) => {
                tracing::warn!(
                    "launch proxy: invalid regex '{pattern}': {e}, leaving text unchanged"
                );
                text.to_string()
            }
        }
    } else {
        text.replace(pattern, "")
    }
}

/// Header carrying the per-session peer-auth token (#1632), required on
/// every request the proxy accepts. `pub(crate)` so `src/launch/mod.rs` can
/// inject the same name into `ANTHROPIC_CUSTOM_HEADERS` without the string
/// literal drifting between the two sites.
pub(crate) const PEER_AUTH_HEADER: &str = "x-llmenv-launch-proxy-token";

/// Bind the local proxy listener on an OS-assigned ephemeral port, loopback
/// only, and generate this session's peer-auth token (#1632) — see
/// `peer_authorized` for why loopback binding alone isn't enough.
///
/// # Errors
/// Returns an error when the bind fails or the token can't be generated.
pub(crate) async fn bind() -> anyhow::Result<(
    tokio::net::TcpListener,
    std::net::SocketAddr,
    crate::launch::socket::LaunchToken,
)> {
    let token = crate::launch::socket::LaunchToken::generate()
        .context("generating launch proxy peer-auth token")?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding launch proxy listener")?;
    let addr = listener
        .local_addr()
        .context("reading launch proxy listener address")?;
    Ok((listener, addr, token))
}

/// Whether `headers` carries the exact [`PEER_AUTH_HEADER`] value `token`
/// expects. `127.0.0.1:0` blocks off-host access but not a different local
/// user on the same host (#1632) — this token closes that gap the same way
/// `socket.rs`'s `LaunchToken` closes it for the notice socket, just without
/// a full HMAC handshake: a static header value is all a stateless
/// `ANTHROPIC_CUSTOM_HEADERS` env var can carry, so there's no live
/// challenge-response to layer on top the way the socket's bidirectional
/// protocol allows.
fn peer_authorized(headers: &http::HeaderMap, token: &crate::launch::socket::LaunchToken) -> bool {
    let Some(value) = headers.get(PEER_AUTH_HEADER).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    constant_time_eq(value.as_bytes(), token.as_str().as_bytes())
}

/// Constant-time byte equality: a mismatch takes the same time regardless of
/// where the first differing byte falls, so a same-host attacker probing the
/// proxy can't use timing to narrow down the token. Mirrors the property
/// `socket.rs`'s HMAC-based `verify_hmac_hex` already gives that module. The
/// length check leaks nothing secret — both the token and any candidate are
/// always exactly `TOKEN_BYTES * 2` hex characters, a length the attacker
/// already knows. `std::hint::black_box` on the accumulator is the barrier:
/// without it, nothing stops an optimizer from proving the fold's result
/// equals a plain `a == b` and replacing it with one, since the fold itself
/// has no other observable effect.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// Accept connections until `shutdown` reports `true` (set when the
/// supervised engine exits — see `src/launch/mod.rs`). Each request is
/// checked against `token` (#1632), rewritten per `rules`, forwarded to
/// `upstream`, and the response streamed back unmodified.
pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    upstream: url::Url,
    rules: std::sync::Arc<Vec<ProxyRule>>,
    token: crate::launch::socket::LaunchToken,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Redirects disabled deliberately: reqwest's default policy follows up
    // to 10 redirects and only strips `authorization`/`cookie`/`cookie2`/
    // `proxy-authorization`/`www-authenticate` on a cross-host hop — not
    // Anthropic's `x-api-key`. A malicious or misconfigured upstream (the
    // chained-through corporate gateway this proxy explicitly supports)
    // could otherwise redirect the API key to an attacker-controlled host.
    // A forwarding proxy should hand a 3xx straight back, not follow it.
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "launch proxy: could not build forwarding client, proxy disabled for this session: {e}"
            );
            return;
        }
    };
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let io = hyper_util::rt::TokioIo::new(stream);
                let client = client.clone();
                let upstream = upstream.clone();
                let rules = std::sync::Arc::clone(&rules);
                let token = token.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req| {
                        handle(
                            req,
                            client.clone(),
                            upstream.clone(),
                            std::sync::Arc::clone(&rules),
                            token.clone(),
                        )
                    });
                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                    {
                        tracing::debug!("launch proxy: connection error: {e}");
                    }
                });
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

type ProxyResponse = hyper::Response<
    http_body_util::combinators::BoxBody<hyper::body::Bytes, std::convert::Infallible>,
>;

/// Request-body cap: `body.collect()` buffers the whole body in memory before
/// it can be rewritten, so an unbounded body is an unbounded allocation. 64
/// MiB comfortably covers any realistic Claude Code request (Anthropic's
/// largest context window is ~200K tokens, well under a tenth of this in
/// bytes) while still bounding a single request's worst case.
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

async fn handle(
    req: hyper::Request<hyper::body::Incoming>,
    client: reqwest::Client,
    upstream: url::Url,
    rules: std::sync::Arc<Vec<ProxyRule>>,
    token: crate::launch::socket::LaunchToken,
) -> Result<ProxyResponse, std::convert::Infallible> {
    let (parts, body) = req.into_parts();
    if !peer_authorized(&parts.headers, &token) {
        return Ok(unauthorized(
            "missing or invalid launch proxy peer-auth token",
        ));
    }
    let limited = http_body_util::Limited::new(body, MAX_REQUEST_BODY_BYTES);
    let body_bytes = match limited.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => return Ok(error_response(&format!("reading request body: {e}"))),
    };
    let mut json_body: serde_json::Value = if body_bytes.is_empty() {
        serde_json::Value::Null
    } else {
        match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(e) => {
                return Ok(error_response(&format!(
                    "request body was not valid JSON: {e}"
                )));
            }
        }
    };
    let mut headers = parts.headers.clone();
    apply_rules(&rules, &mut headers, &mut json_body);

    let incoming_path = parts.uri.path();
    // `Url::join` treats a path starting with `//` as a network-path
    // reference (RFC 3986 §4.2) and replaces the whole authority with
    // whatever follows — a request line of `POST //evil.example/v1/messages`
    // would silently redirect the proxy's own outbound HTTPS request to an
    // attacker-chosen host. Reject it outright rather than joining.
    if incoming_path.starts_with("//") {
        return Ok(error_response(
            "request path must not start with '//' (would override the upstream host)",
        ));
    }
    // `Url::join` also replaces the base URL's own path entirely for a
    // path-absolute reference, dropping any prefix the upstream already had
    // (e.g. a chained corporate gateway at `https://gw.example.com/anthropic`
    // would lose `/anthropic`). Concatenate instead, and carry the query
    // string across separately — `Uri::path()` never includes it.
    let mut target_url = upstream.clone();
    target_url.set_path(&format!(
        "{}{incoming_path}",
        upstream.path().trim_end_matches('/')
    ));
    target_url.set_query(parts.uri.query());

    // `hyper::Method` is a direct re-export of `http::Method` — the same
    // type reqwest uses — so this needs no conversion.
    let mut builder = client.request(parts.method.clone(), target_url.clone());
    for (name, value) in &headers {
        // `host` doesn't belong on the forwarded request (reqwest derives it
        // from the target URL); `content-length` is stale once the body has
        // been rewritten — reqwest recomputes it from the actual body set
        // below, and forwarding the original value here would corrupt the
        // request wiremock/the real Anthropic API receives. The rest are
        // hop-by-hop headers (RFC 9110 §7.6.1) an intermediary must not
        // forward — most importantly `transfer-encoding`, which forwarded
        // alongside a body this proxy just re-serialized (with its own,
        // correct `content-length`) is the classic request-smuggling shape.
        if name == http::header::HOST
            || name == http::header::CONTENT_LENGTH
            || name == http::header::CONNECTION
            || name == http::header::TRANSFER_ENCODING
            || name == http::header::TE
            || name == http::header::UPGRADE
            || name == http::header::PROXY_AUTHORIZATION
            || name.as_str().eq_ignore_ascii_case("keep-alive")
            || name.as_str().eq_ignore_ascii_case(PEER_AUTH_HEADER)
        {
            continue;
        }
        if let Ok(v) = value.to_str() {
            builder = builder.header(name.as_str(), v);
        }
    }
    let outgoing = match serde_json::to_vec(&json_body) {
        Ok(bytes) => bytes,
        Err(e) => {
            return Ok(error_response(&format!(
                "re-serializing rewritten body: {e}"
            )));
        }
    };
    builder = builder.body(outgoing);

    let upstream_resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            // `reqwest::Error`'s `Display` embeds the request URL verbatim,
            // userinfo included — an `ANTHROPIC_BASE_URL` with an embedded
            // credential (a corporate gateway, e.g. `https://user:pass@gw/`)
            // would otherwise leak that credential into both this warning
            // and the 502 body handed back to any local caller.
            tracing::warn!("launch proxy: upstream request failed: {}", e.without_url());
            return Ok(bad_gateway("upstream request failed"));
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    let stream = upstream_resp.bytes_stream().filter_map(|chunk| {
        std::future::ready(match chunk {
            Ok(bytes) => Some(Ok::<_, std::convert::Infallible>(hyper::body::Frame::data(
                bytes,
            ))),
            Err(e) => {
                // The client already got a 2xx status and part of the body
                // by this point — the response is truncated, not merely
                // slow, and that's worth more than debug-level visibility.
                tracing::warn!("launch proxy: upstream response stream interrupted: {e}");
                None
            }
        })
    });
    let body = http_body_util::BodyExt::boxed(http_body_util::StreamBody::new(stream));

    // `reqwest::StatusCode` and `reqwest::header::HeaderMap` are direct
    // re-exports of the same `http` crate types hyper uses (reqwest's
    // `lib.rs` has `pub use http::{StatusCode, Version}` and `pub use
    // http::header`), so both assign with no conversion and no fallback.
    let mut response = hyper::Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = resp_headers;
    Ok(response)
}

fn error_response(msg: &str) -> ProxyResponse {
    response_with_status(hyper::StatusCode::BAD_REQUEST, msg)
}

fn unauthorized(msg: &str) -> ProxyResponse {
    response_with_status(hyper::StatusCode::UNAUTHORIZED, msg)
}

fn bad_gateway(msg: &str) -> ProxyResponse {
    response_with_status(hyper::StatusCode::BAD_GATEWAY, msg)
}

fn response_with_status(status: hyper::StatusCode, msg: &str) -> ProxyResponse {
    tracing::warn!("launch proxy: {msg}");
    let body = http_body_util::Full::new(hyper::body::Bytes::from(msg.to_string()))
        .map_err(|never: std::convert::Infallible| match never {})
        .boxed();
    let mut resp = hyper::Response::new(body);
    *resp.status_mut() = status;
    resp
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn rule(when: Vec<ProxyCondition>, target: ProxyTarget, op: ProxyOp) -> ProxyRule {
        ProxyRule { when, target, op }
    }

    #[test]
    fn set_upserts_missing_body_field() {
        let rules = vec![rule(
            vec![],
            ProxyTarget::Body {
                path: "thinking".into(),
            },
            ProxyOp::Set {
                value: json!({"type": "disabled"}),
            },
        )];
        let mut headers = http::HeaderMap::new();
        let mut body = json!({});
        apply_rules(&rules, &mut headers, &mut body);
        assert_eq!(body, json!({"thinking": {"type": "disabled"}}));
    }

    #[test]
    fn rule_skipped_when_any_when_condition_fails() {
        let rules = vec![rule(
            vec![ProxyCondition {
                target: ProxyConditionTarget::Body { path: None },
                check: ProxyCheck::Matches {
                    pattern: "nope".into(),
                    regex: false,
                },
            }],
            ProxyTarget::Body {
                path: "thinking".into(),
            },
            ProxyOp::Set {
                value: json!({"type": "disabled"}),
            },
        )];
        let mut headers = http::HeaderMap::new();
        let mut body = json!({});
        apply_rules(&rules, &mut headers, &mut body);
        assert_eq!(body, json!({}));
    }

    #[test]
    fn rule_fires_when_all_and_conditions_match() {
        let rules = vec![rule(
            vec![
                ProxyCondition {
                    target: ProxyConditionTarget::Header {
                        name: "x-billing-header".into(),
                    },
                    check: ProxyCheck::Present,
                },
                ProxyCondition {
                    target: ProxyConditionTarget::Body {
                        path: Some("system[0].text".into()),
                    },
                    check: ProxyCheck::Matches {
                        pattern: "security monitor".into(),
                        regex: false,
                    },
                },
                ProxyCondition {
                    target: ProxyConditionTarget::Body {
                        path: Some("thinking".into()),
                    },
                    check: ProxyCheck::Missing,
                },
            ],
            ProxyTarget::Body {
                path: "thinking".into(),
            },
            ProxyOp::Set {
                value: json!({"type": "disabled"}),
            },
        )];
        let mut headers = http::HeaderMap::new();
        headers.insert("x-billing-header", "1".parse().unwrap());
        let mut body = json!({"system": [{"text": "You are a security monitor for autonomous AI coding agents."}]});
        apply_rules(&rules, &mut headers, &mut body);
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
    }

    #[test]
    fn remove_is_noop_when_path_absent() {
        let rules = vec![rule(
            vec![],
            ProxyTarget::Body {
                path: "nope".into(),
            },
            ProxyOp::Remove,
        )];
        let mut headers = http::HeaderMap::new();
        let mut body = json!({"a": 1});
        apply_rules(&rules, &mut headers, &mut body);
        assert_eq!(body, json!({"a": 1}));
    }

    #[test]
    fn strip_removes_regex_match_from_body_text() {
        let rules = vec![rule(
            vec![],
            ProxyTarget::Body {
                path: "system[0].text".into(),
            },
            ProxyOp::Strip {
                pattern: "boilerplate.*".into(),
                regex: true,
            },
        )];
        let mut headers = http::HeaderMap::new();
        let mut body = json!({"system": [{"text": "keep this. boilerplate stuff to cut"}]});
        apply_rules(&rules, &mut headers, &mut body);
        assert_eq!(body["system"][0]["text"], "keep this. ");
    }

    #[tokio::test]
    async fn proxy_forwards_rewritten_request_and_streams_response() {
        let _guard = llmenv_util::testkit::port_guard();
        let upstream = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/messages"))
            .and(wiremock::matchers::body_partial_json(
                json!({"thinking": {"type": "disabled"}}),
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&upstream)
            .await;

        let rules = std::sync::Arc::new(vec![rule(
            vec![],
            ProxyTarget::Body {
                path: "thinking".into(),
            },
            ProxyOp::Set {
                value: json!({"type": "disabled"}),
            },
        )]);
        let (listener, addr, token) = bind().await.unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let upstream_url: url::Url = upstream.uri().parse().unwrap();
        tokio::spawn(serve(listener, upstream_url, rules, token.clone(), rx));

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/v1/messages"))
            .header(PEER_AUTH_HEADER, token.as_str())
            .json(&json!({"model": "claude-x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
    }

    /// A request path starting with `//` must never be forwarded — `Url::join`
    /// treats it as a network-path reference and would replace the upstream
    /// host with whatever follows, letting a local caller redirect the
    /// proxy's outbound HTTPS request to an arbitrary host.
    #[tokio::test]
    async fn proxy_rejects_double_slash_path() {
        let _guard = llmenv_util::testkit::port_guard();
        let upstream = wiremock::MockServer::start().await;
        // No mock mounted: if the rejection ever regressed and the request
        // were forwarded to `upstream` unmodified (rather than hijacked to
        // some other host), this would still fail loudly with wiremock's own
        // "no matching mock" 404 rather than silently succeeding.
        let rules = std::sync::Arc::new(Vec::new());
        let (listener, addr, token) = bind().await.unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let upstream_url: url::Url = upstream.uri().parse().unwrap();
        tokio::spawn(serve(listener, upstream_url, rules, token.clone(), rx));

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}//evil.example.com/v1/messages"))
            .header(PEER_AUTH_HEADER, token.as_str())
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    /// The proxy must never follow a redirect itself — Anthropic's `x-api-key`
    /// header isn't in reqwest's default cross-host redirect strip list, so
    /// auto-following would leak it to whatever host a 3xx points at.
    #[tokio::test]
    async fn proxy_does_not_follow_redirects() {
        let _guard = llmenv_util::testkit::port_guard();
        let upstream = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(302)
                    .insert_header("location", "https://attacker.example/steal"),
            )
            .mount(&upstream)
            .await;

        let rules = std::sync::Arc::new(Vec::new());
        let (listener, addr, token) = bind().await.unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let upstream_url: url::Url = upstream.uri().parse().unwrap();
        tokio::spawn(serve(listener, upstream_url, rules, token.clone(), rx));

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let resp = client
            .post(format!("http://{addr}/v1/messages"))
            .header(PEER_AUTH_HEADER, token.as_str())
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 302);
        assert_eq!(
            resp.headers().get("location").unwrap(),
            "https://attacker.example/steal"
        );
    }

    /// A chained upstream's own path prefix (e.g. a corporate gateway at
    /// `.../anthropic`) must survive, and the request's query string must
    /// not be silently dropped.
    #[tokio::test]
    async fn proxy_preserves_upstream_base_path_and_query_string() {
        let _guard = llmenv_util::testkit::port_guard();
        let upstream = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/anthropic/v1/messages"))
            .and(wiremock::matchers::query_param("beta", "true"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&upstream)
            .await;

        let rules = std::sync::Arc::new(Vec::new());
        let (listener, addr, token) = bind().await.unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let upstream_url: url::Url = format!("{}/anthropic", upstream.uri()).parse().unwrap();
        tokio::spawn(serve(listener, upstream_url, rules, token.clone(), rx));

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/v1/messages?beta=true"))
            .header(PEER_AUTH_HEADER, token.as_str())
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    /// Hop-by-hop headers (RFC 9110 §7.6.1) must not reach the upstream —
    /// forwarding `transfer-encoding` alongside a body this proxy already
    /// re-serialized with its own `content-length` is a request-smuggling
    /// shape.
    #[tokio::test]
    async fn proxy_strips_hop_by_hop_headers() {
        let _guard = llmenv_util::testkit::port_guard();
        let upstream = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&upstream)
            .await;

        let rules = std::sync::Arc::new(Vec::new());
        let (listener, addr, token) = bind().await.unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let upstream_url: url::Url = upstream.uri().parse().unwrap();
        tokio::spawn(serve(listener, upstream_url, rules, token.clone(), rx));

        let client = reqwest::Client::new();
        client
            .post(format!("http://{addr}/v1/messages"))
            .header(PEER_AUTH_HEADER, token.as_str())
            .header("connection", "keep-alive")
            .header("transfer-encoding", "chunked")
            .json(&json!({}))
            .send()
            .await
            .unwrap();

        let received = upstream.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        assert!(!received[0].headers.contains_key("connection"));
        assert!(!received[0].headers.contains_key("transfer-encoding"));
        assert!(
            !received[0].headers.contains_key(PEER_AUTH_HEADER),
            "the proxy's own peer-auth header must never reach the real upstream"
        );
    }

    /// #1632: a request with no peer-auth header at all must be rejected —
    /// loopback binding alone stops off-host access but not a different local
    /// user on the same host.
    #[tokio::test]
    async fn proxy_rejects_missing_peer_auth_token() {
        let _guard = llmenv_util::testkit::port_guard();
        let upstream = wiremock::MockServer::start().await;
        // No mock mounted: a regression that forwards an unauthenticated
        // request should fail loudly (wiremock's "no matching mock" 404),
        // not silently succeed.
        let rules = std::sync::Arc::new(Vec::new());
        let (listener, addr, token) = bind().await.unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let upstream_url: url::Url = upstream.uri().parse().unwrap();
        tokio::spawn(serve(listener, upstream_url, rules, token, rx));

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/v1/messages"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    /// #1632: a request bearing the wrong token must be rejected the same way
    /// as a missing one — a same-host attacker who guesses or brute-forces a
    /// value must not be told "close" via a different status code.
    #[tokio::test]
    async fn proxy_rejects_wrong_peer_auth_token() {
        let _guard = llmenv_util::testkit::port_guard();
        let upstream = wiremock::MockServer::start().await;
        let rules = std::sync::Arc::new(Vec::new());
        let (listener, addr, token) = bind().await.unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let upstream_url: url::Url = upstream.uri().parse().unwrap();
        tokio::spawn(serve(listener, upstream_url, rules, token, rx));

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/v1/messages"))
            .header(PEER_AUTH_HEADER, "0".repeat(64))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    /// #1632: `peer_authorized` must accept exactly the header value that
    /// matches the token and reject any candidate that doesn't, for an
    /// arbitrary hex-shaped candidate — not just the two hand-picked
    /// examples (`proxy_rejects_missing_peer_auth_token` /
    /// `proxy_rejects_wrong_peer_auth_token`) that exercise this indirectly
    /// over HTTP.
    #[test]
    fn peer_authorized_accepts_the_token_and_rejects_any_other_candidate() {
        let token = crate::launch::socket::LaunchToken::generate().unwrap();
        let mut correct_headers = http::HeaderMap::new();
        correct_headers.insert(PEER_AUTH_HEADER, token.as_str().parse().unwrap());
        assert!(peer_authorized(&correct_headers, &token));

        let empty_headers = http::HeaderMap::new();
        assert!(!peer_authorized(&empty_headers, &token));

        let mut wrong_headers = http::HeaderMap::new();
        wrong_headers.insert(PEER_AUTH_HEADER, "0".repeat(64).parse().unwrap());
        assert!(!peer_authorized(&wrong_headers, &token));
    }

    /// A small, bounded-depth arbitrary JSON value generator — request
    /// bodies come from Claude Code (semi-trusted), so `apply_rules` needs
    /// to survive whatever shape shows up, not just the hand-picked example
    /// bodies the other tests use.
    fn arbitrary_json_value_strategy() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            Just(serde_json::Value::Null),
            proptest::bool::ANY.prop_map(serde_json::Value::Bool),
            (-1000i64..1000).prop_map(|n| serde_json::json!(n)),
            "[a-z]{0,8}".prop_map(serde_json::Value::String),
        ];
        leaf.prop_recursive(3, 20, 5, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::Array),
                proptest::collection::hash_map("[a-z]{1,5}", inner, 0..4)
                    .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
            ]
        })
    }

    proptest::proptest! {
        /// `apply_rules` must never panic regardless of the request body's
        /// shape, and its output must always still be valid, round-trippable
        /// JSON — exercises `set`/`strip`/`remove` together against paths
        /// that may or may not exist in a given generated body.
        #[test]
        fn apply_rules_never_panics_and_output_stays_valid_json(
            mut body in arbitrary_json_value_strategy(),
        ) {
            let rules = vec![
                rule(
                    vec![],
                    ProxyTarget::Body { path: "thinking".into() },
                    ProxyOp::Set { value: json!({"type": "disabled"}) },
                ),
                rule(
                    vec![],
                    ProxyTarget::Body { path: "system[0].text".into() },
                    ProxyOp::Strip { pattern: "x".into(), regex: false },
                ),
                rule(
                    vec![],
                    ProxyTarget::Body { path: "a.b.c".into() },
                    ProxyOp::Remove,
                ),
            ];
            let mut headers = http::HeaderMap::new();
            apply_rules(&rules, &mut headers, &mut body);

            let bytes = serde_json::to_vec(&body).unwrap();
            let round_tripped: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            prop_assert_eq!(round_tripped, body);
        }

        /// #1632: `constant_time_eq` must agree with native slice equality
        /// for every input — its only reason to exist is *how* it reaches
        /// that answer (branch-free over the compared bytes), never a
        /// different answer.
        #[test]
        fn constant_time_eq_matches_native_equality(
            a in proptest::collection::vec(any::<u8>(), 0..64),
            b in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            prop_assert_eq!(constant_time_eq(&a, &b), a == b);
        }

        /// #1632: `peer_authorized` must agree with plain string equality
        /// against the token for an arbitrary candidate header value — it
        /// delegates to `constant_time_eq` only for *how* it decides, never
        /// for a different answer than a direct comparison would give.
        #[test]
        fn peer_authorized_matches_plain_equality_for_arbitrary_candidates(
            candidate in "[0-9a-f]{0,80}",
        ) {
            let token = crate::launch::socket::LaunchToken::generate().unwrap();
            let mut headers = http::HeaderMap::new();
            headers.insert(PEER_AUTH_HEADER, candidate.parse().unwrap());
            prop_assert_eq!(peer_authorized(&headers, &token), candidate == token.as_str());
        }
    }
}
