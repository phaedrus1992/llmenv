//! HTTP fetch for the umans usage endpoint.
//!
//! The Bearer token goes only to addresses that passed the SSRF check. The request
//! therefore connects to the vetted addresses, follows no redirect, and ignores proxy
//! settings. A proxy resolves the host itself, which would bypass the address pin.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;

/// Largest usage body accepted, in bytes.
const MAX_BODY: usize = 65_536;

/// Largest error-body preview copied into an error message, in characters.
const ERROR_PREVIEW_CHARS: usize = 512;

/// GET `url` with a Bearer token and return the raw body bytes.
///
/// `addrs` must be the addresses that `validate_url_production` returned for `host`.
/// Uses a single-threaded tokio runtime, so this must not run inside another runtime.
pub(super) fn fetch_usage_body(
    url: &str,
    token: &str,
    host: &str,
    addrs: &[SocketAddr],
) -> anyhow::Result<Vec<u8>> {
    // An empty pin would leave the client with no route, so refuse before any connect.
    anyhow::ensure!(
        !addrs.is_empty(),
        "no vetted addresses for umans host {host}; refusing to send the token"
    );
    let url = url.to_owned();
    let host = host.to_owned();
    let addrs = addrs.to_vec();
    let auth = format!("Bearer {token}");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    rt.block_on(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .no_proxy()
            .resolve_to_addrs(&host, &addrs)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building reqwest client")?;
        let mut resp = client
            .get(&url)
            .header("Authorization", auth)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if status.is_redirection() {
            anyhow::bail!(
                "umans usage API returned {status}; redirects are not followed, \
                 so set api_endpoint to the final URL"
            );
        }
        if !status.is_success() {
            let preview = match read_capped(&mut resp).await {
                Ok(body) => String::from_utf8_lossy(&body)
                    .chars()
                    .take(ERROR_PREVIEW_CHARS)
                    .collect(),
                Err(err) => format!("(body unreadable: {err:#})"),
            };
            anyhow::bail!("umans usage API returned {status}: {preview}");
        }
        read_capped(&mut resp).await
    })
}

/// Read the whole body, stopping as soon as it passes `MAX_BODY`.
///
/// The size check runs per chunk, so a response without `Content-Length` cannot
/// grow the buffer past the limit.
async fn read_capped(resp: &mut reqwest::Response) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.context("reading umans usage response")? {
        if body.len().saturating_add(chunk.len()) > MAX_BODY {
            anyhow::bail!("umans usage response too large (limit {MAX_BODY} bytes)");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Serve one canned HTTP response to one accepted connection.
    fn serve_response(response: Vec<u8>) -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match std::io::Read::read(&mut stream, &mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&chunk[..n]),
                }
            }
            let _ = std::io::Write::write_all(&mut stream, &response);
        });
        addr
    }

    fn http_response(status_line: &str, extra_headers: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status_line}\r\n{extra_headers}Connection: close\r\n\r\n")
            .into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn redirect_is_refused_not_followed() {
        // A followed redirect would reach a second connection, which this server never accepts.
        let addr = serve_response(http_response(
            "302 Found",
            "Location: /elsewhere\r\nContent-Length: 0\r\n",
            b"",
        ));
        let url = format!("http://{addr}/v1/usage");
        let err = match fetch_usage_body(&url, "token", "127.0.0.1", &[addr]) {
            Ok(_) => panic!("a 302 redirect must not be followed"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("redirects are not followed"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn connects_only_to_the_vetted_addresses() {
        // The host does not resolve, so the request succeeds only through the pinned address.
        let addr = serve_response(http_response("200 OK", "Content-Length: 2\r\n", b"{}"));
        let url = format!("http://pinned.invalid:{}/v1/usage", addr.port());
        let body = fetch_usage_body(&url, "token", "pinned.invalid", &[addr])
            .expect("the pinned address must serve the request");
        assert_eq!(body, b"{}");
    }

    #[test]
    fn empty_vetted_address_set_is_refused_before_connecting() {
        let err = fetch_usage_body(
            "http://pinned.invalid/v1/usage",
            "token",
            "pinned.invalid",
            &[],
        )
        .expect_err("an empty pin must not send the token");
        assert!(
            err.to_string().contains("no vetted addresses"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn oversized_body_without_content_length_is_refused() {
        let big = vec![b'a'; MAX_BODY + 1];
        let addr = serve_response(http_response("200 OK", "", &big));
        let url = format!("http://{addr}/v1/usage");
        let err = fetch_usage_body(&url, "token", "127.0.0.1", &[addr])
            .expect_err("a body over the limit must be refused");
        assert!(
            err.to_string().contains("too large"),
            "unexpected error: {err:#}"
        );
    }
}
