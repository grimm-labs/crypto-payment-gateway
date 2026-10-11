//! The only place this crate talks to reqwest directly.
//!
//! reqwest appends ` for url (…)` to its send, status and decode errors, and
//! provider URLs carry API keys (Alchemy in the path, Helius in the
//! `api-key` query). Callers log these errors with `{err:#}`, so every error
//! leaving here has its URL stripped (`without_url`). `clippy.toml` forbids
//! the raw reqwest methods in the rest of the crate so this cannot be
//! bypassed by accident.

use anyhow::Result;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;

/// Send the request and reject non-2xx answers.
#[allow(clippy::disallowed_methods)]
pub(crate) async fn send(request: RequestBuilder) -> Result<Response> {
    let response = request.send().await.map_err(redact)?;
    response.error_for_status().map_err(redact)
}

/// `send`, then decode the JSON body.
#[allow(clippy::disallowed_methods)]
pub(crate) async fn send_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T> {
    send(request).await?.json().await.map_err(redact)
}

/// `send`, then read the body as text.
#[allow(clippy::disallowed_methods)]
pub(crate) async fn send_text(request: RequestBuilder) -> Result<String> {
    send(request).await?.text().await.map_err(redact)
}

/// Send the request and return the status and body of any HTTP answer, 2xx
/// or not — for providers that explain a refusal in the body (Esplora
/// broadcasts, 404 for unknown transactions). Transport errors are still
/// errors, URL-free.
#[allow(clippy::disallowed_methods)]
pub(crate) async fn send_any(request: RequestBuilder) -> Result<(StatusCode, String)> {
    let response = request.send().await.map_err(redact)?;
    let status = response.status();
    let body = response.text().await.map_err(redact)?;
    Ok((status, body))
}

/// The HTTP status of a non-2xx answer behind `err` (through any context),
/// `None` for transport or decoding errors. Works on the redacted errors:
/// only the URL is dropped, the status is kept.
pub(crate) fn status(err: &anyhow::Error) -> Option<StatusCode> {
    err.chain().find_map(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
    })
}

/// True when `err` is an HTTP 429 answer.
pub(crate) fn is_rate_limited(err: &anyhow::Error) -> bool {
    status(err) == Some(StatusCode::TOO_MANY_REQUESTS)
}

fn redact(err: reqwest::Error) -> anyhow::Error {
    anyhow::Error::new(err.without_url())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const SECRET: &str = "SECRET-PROVIDER-KEY";

    /// One-shot HTTP server on the loopback interface (no external network)
    /// answering every connection with `response`.
    async fn serve_once(response: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        // Same shapes as the providers: key in the path (Alchemy) and in the
        // query (Helius).
        format!("http://{addr}/v2/{SECRET}?api-key={SECRET}")
    }

    fn assert_redacted(err: &anyhow::Error) {
        assert!(!format!("{err:#}").contains(SECRET), "{err:#}");
        assert!(!format!("{err:?}").contains(SECRET), "{err:?}");
    }

    #[tokio::test]
    async fn status_errors_drop_the_url_but_keep_the_429() {
        let url = serve_once(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )
        .await;
        let err = send_json::<serde_json::Value>(reqwest::Client::new().get(url))
            .await
            .unwrap_err();
        assert_redacted(&err);
        assert!(is_rate_limited(&err));
        assert!(is_rate_limited(&err.context("TronGrid request failed")));
    }

    #[tokio::test]
    async fn other_statuses_are_not_rate_limits() {
        let url = serve_once(
            "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )
        .await;
        let err = send_text(reqwest::Client::new().get(url)).await.unwrap_err();
        assert_redacted(&err);
        assert!(!is_rate_limited(&err));
    }

    #[tokio::test]
    async fn decode_errors_drop_the_url() {
        let url = serve_once(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 8\r\nconnection: close\r\n\r\nnot json",
        )
        .await;
        let err = send_json::<serde_json::Value>(reqwest::Client::new().get(url))
            .await
            .unwrap_err();
        assert_redacted(&err);
    }

    #[tokio::test]
    async fn connection_errors_drop_the_url() {
        // Bind then release a loopback port: nothing listens on it anymore.
        let addr = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let url = format!("http://{addr}/v2/{SECRET}?api-key={SECRET}");
        let err = send(reqwest::Client::new().get(url)).await.unwrap_err();
        assert_redacted(&err);
        assert!(!is_rate_limited(&err));
    }
}
