//! Outgoing webhook HTTP client, hardened against SSRF: the gateway POSTs to
//! admin-supplied URLs, so it must never be steered towards its own network
//! (cloud metadata, Redis, Postgres, the Docker bridge…).
//!
//! - every DNS answer is filtered through [`domain::is_public_ip`] at
//!   connection time (no DNS-rebinding window between check and use);
//! - IP-literal hosts are checked before sending;
//! - redirects are never followed: a 3xx counts as a failed attempt (a
//!   301→GET to a catch-all page would otherwise be marked DELIVERED, and a
//!   307/308 would relay the signed body to another host);
//! - `https://` only.
//!
//! In `APP_ENV=development` plain `http://` and private addresses stay
//! allowed, so a receiver on localhost can be tested.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use config::WEBHOOK_HTTP_TIMEOUT_SECS;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::Url;

/// Local-only DNS suffixes refused even before resolution.
const LOCAL_SUFFIXES: [&str; 9] = [
    "localhost", "local", "internal", "lan", "home", "localdomain", "arpa", "test", "invalid",
];

#[derive(Clone)]
pub struct WebhookClient {
    http: reqwest::Client,
    allow_private: bool,
}

impl WebhookClient {
    /// `allow_private` (development only) lifts the https and public-address
    /// rules.
    pub fn new(allow_private: bool) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(WEBHOOK_HTTP_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("crypto-payment-gateway-webhooks");
        if !allow_private {
            builder = builder
                .dns_resolver(Arc::new(PublicOnlyResolver))
                .https_only(true);
        }
        Ok(Self {
            http: builder.build().context("webhook HTTP client")?,
            allow_private,
        })
    }

    pub fn allows_private(&self) -> bool {
        self.allow_private
    }

    /// Validate `url` (see [`validate_webhook_url`]) and start a POST to it.
    pub fn post(&self, url: &str) -> Result<reqwest::RequestBuilder, String> {
        let url = validate_webhook_url(url, self.allow_private)?;
        Ok(self.http.post(url))
    }
}

/// Static checks of a webhook URL: `https` (or `http` when
/// `allow_private`), no credentials in the URL, and a host that is not an
/// internal name or a non-public IP literal. DNS names are checked again at
/// connection time by the resolver. Returns a client-safe error message.
pub fn validate_webhook_url(raw: &str, allow_private: bool) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|_| "Invalid webhook URL".to_string())?;
    match url.scheme() {
        "https" => {}
        "http" if allow_private => {}
        _ => return Err("Webhook URL must use https://".into()),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Webhook URL must not contain credentials (user:password@)".into());
    }
    if url.fragment().is_some() {
        return Err("Webhook URL must not contain a fragment".into());
    }
    let host = url
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "Webhook URL must have a host".to_string())?;
    if !allow_private {
        match literal_ip(host) {
            Some(ip) if !domain::is_public_ip(ip) => {
                return Err("Webhook URL must not target a private or reserved address".into())
            }
            Some(_) => {}
            None if is_local_name(host) => {
                return Err("Webhook URL must use a public host name".into())
            }
            None => {}
        }
    }
    Ok(url)
}

/// The IP of an IP-literal host (`host_str` keeps IPv6 brackets).
fn literal_ip(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// Resolve the URL's host and require at least one public address (when
/// `allow_private` is false) — the creation-time check that gives the admin
/// an immediate error instead of failed deliveries.
pub async fn check_webhook_host_resolves_publicly(url: &Url, allow_private: bool) -> Result<(), String> {
    if allow_private {
        return Ok(());
    }
    let Some(name) = url.host_str().filter(|host| literal_ip(host).is_none()) else {
        return Ok(()); // IP literals were checked statically.
    };
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name, port))
        .await
        .map_err(|_| format!("Webhook host {name} does not resolve"))?
        .collect();
    if addrs.iter().any(|addr| domain::is_public_ip(addr.ip())) {
        Ok(())
    } else {
        Err(format!("Webhook host {name} does not resolve to a public address"))
    }
}

/// A single-label name, or one under a local-only suffix.
pub fn is_local_name(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    !host.contains('.')
        || LOCAL_SUFFIXES
            .iter()
            .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

/// DNS resolver that only ever hands public addresses to the connector.
struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            let resolved = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let public: Vec<SocketAddr> = resolved
                .filter(|addr| domain::is_public_ip(addr.ip()))
                .collect();
            if public.is_empty() {
                return Err(format!("{host} does not resolve to a public address").into());
            }
            let addrs: Addrs = Box::new(public.into_iter());
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_public_urls_without_credentials_are_accepted() {
        assert!(validate_webhook_url("https://hooks.example.com/pay?token=x", false).is_ok());
        for bad in [
            "http://hooks.example.com/pay",
            "ftp://hooks.example.com",
            "https://user:pass@hooks.example.com/",
            "https://hooks.example.com/#frag",
            "https://127.0.0.1/hook",
            "https://169.254.169.254/latest/meta-data",
            "https://10.0.0.5:8080/",
            "https://[::1]/",
            "https://[::ffff:192.168.1.1]/",
            "https://localhost/hook",
            "https://redis/",
            "https://db.internal/",
            "https://printer.local/",
            "https://1.0.0.127.in-addr.arpa/",
            "not a url",
        ] {
            assert!(validate_webhook_url(bad, false).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn development_allows_local_receivers() {
        assert!(validate_webhook_url("http://localhost:4000/hook", true).is_ok());
        assert!(validate_webhook_url("http://127.0.0.1:4000/hook", true).is_ok());
        // Credentials stay forbidden everywhere.
        assert!(validate_webhook_url("http://u:p@localhost/hook", true).is_err());
    }

    #[test]
    fn local_names_are_detected() {
        for name in ["localhost", "api", "x.local", "db.internal.", "a.b.test"] {
            assert!(is_local_name(name), "{name}");
        }
        for name in ["example.com", "hooks.example.co.uk"] {
            assert!(!is_local_name(name), "{name}");
        }
    }
}
