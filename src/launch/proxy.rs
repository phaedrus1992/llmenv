//! Local API-proxy mode for `llmenv launch claude_code` (#1289). See
//! `docs/superpowers/specs/2026-08-28-launch-api-proxy-design.md`.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "bind/serve have no production caller until src/launch/mod.rs wires them into run()"
    )
)]

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
pub(crate) fn apply_rules(
    rules: &[ProxyRule],
    headers: &mut http::HeaderMap,
    body: &mut serde_json::Value,
) {
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
        regex::Regex::new(pattern).is_ok_and(|re| re.is_match(text))
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
            if let Ok(value) = http::HeaderValue::try_from(stripped) {
                headers.insert(header_name, value);
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

/// Bind the local proxy listener on an OS-assigned ephemeral port, loopback
/// only.
///
/// # Errors
/// Returns an error when the bind fails.
pub(crate) async fn bind() -> anyhow::Result<(tokio::net::TcpListener, std::net::SocketAddr)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding launch proxy listener")?;
    let addr = listener
        .local_addr()
        .context("reading launch proxy listener address")?;
    Ok((listener, addr))
}

/// Accept connections until `shutdown` reports `true` (set when the
/// supervised engine exits — see `src/launch/mod.rs`). Each request is
/// rewritten per `rules`, forwarded to `upstream`, and the response streamed
/// back unmodified.
pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    upstream: url::Url,
    rules: std::sync::Arc<Vec<ProxyRule>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let client = reqwest::Client::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let io = hyper_util::rt::TokioIo::new(stream);
                let client = client.clone();
                let upstream = upstream.clone();
                let rules = std::sync::Arc::clone(&rules);
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req| {
                        handle(req, client.clone(), upstream.clone(), std::sync::Arc::clone(&rules))
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

async fn handle(
    req: hyper::Request<hyper::body::Incoming>,
    client: reqwest::Client,
    upstream: url::Url,
    rules: std::sync::Arc<Vec<ProxyRule>>,
) -> Result<ProxyResponse, std::convert::Infallible> {
    let (parts, body) = req.into_parts();
    let body_bytes = match body.collect().await {
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

    let Ok(target_url) = upstream.join(parts.uri.path()) else {
        return Ok(error_response("could not build upstream URL"));
    };
    let mut builder = client.request(reqwest_method(&parts.method), target_url);
    for (name, value) in &headers {
        // `host` doesn't belong on the forwarded request (reqwest derives it
        // from the target URL); `content-length` is stale once the body has
        // been rewritten — reqwest recomputes it from the actual body set
        // below, and forwarding the original value here would corrupt the
        // request wiremock/the real Anthropic API receives.
        if name == http::header::HOST || name == http::header::CONTENT_LENGTH {
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
        Err(e) => return Ok(bad_gateway(&format!("upstream request failed: {e}"))),
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    let stream = upstream_resp.bytes_stream().filter_map(|chunk| {
        std::future::ready(match chunk {
            Ok(bytes) => Some(Ok::<_, std::convert::Infallible>(hyper::body::Frame::data(
                bytes,
            ))),
            Err(e) => {
                tracing::debug!("launch proxy: upstream stream error: {e}");
                None
            }
        })
    });
    let body = http_body_util::BodyExt::boxed(http_body_util::StreamBody::new(stream));

    let mut response = hyper::Response::new(body);
    *response.status_mut() =
        hyper::StatusCode::from_u16(status.as_u16()).unwrap_or(hyper::StatusCode::BAD_GATEWAY);
    for (name, value) in &resp_headers {
        if let (Ok(name), Ok(value)) = (
            hyper::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            hyper::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    Ok(response)
}

fn reqwest_method(method: &hyper::Method) -> reqwest::Method {
    reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::POST)
}

fn error_response(msg: &str) -> ProxyResponse {
    tracing::warn!("launch proxy: {msg}");
    let body = http_body_util::Full::new(hyper::body::Bytes::from(msg.to_string()))
        .map_err(|never: std::convert::Infallible| match never {})
        .boxed();
    let mut resp = hyper::Response::new(body);
    *resp.status_mut() = hyper::StatusCode::BAD_REQUEST;
    resp
}

fn bad_gateway(msg: &str) -> ProxyResponse {
    tracing::warn!("launch proxy: {msg}");
    let body = http_body_util::Full::new(hyper::body::Bytes::from(msg.to_string()))
        .map_err(|never: std::convert::Infallible| match never {})
        .boxed();
    let mut resp = hyper::Response::new(body);
    *resp.status_mut() = hyper::StatusCode::BAD_GATEWAY;
    resp
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
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
        let (listener, addr) = bind().await.unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let upstream_url: url::Url = upstream.uri().parse().unwrap();
        tokio::spawn(serve(listener, upstream_url, rules, rx));

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/v1/messages"))
            .json(&json!({"model": "claude-x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
    }
}
