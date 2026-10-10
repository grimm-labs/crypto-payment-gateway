//! Request-recording middleware for the API traffic log.
//!
//! Sits *outside* the auth guards (see the layer order in `router`) so it
//! observes rejected requests too, and reads the caller identity the guards
//! copy into the response extensions. Recording is a non-blocking
//! `try_send`; the only thing awaited is the inner handler.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use chrono::Utc;
use db::ApiTrafficEvent;
use domain::ApiSurface;
use ipnet::IpNet;
use jobs::TrafficRecorder;

use crate::middleware::{CurrentApiKey, CurrentStaff};

/// Route recorded for `/api/...` requests that matched no route (404s) —
/// never the raw path, to keep cardinality bounded.
pub const UNMATCHED_ROUTE: &str = "<unmatched>";

/// Middleware state: the recorder plus the reverse proxies whose
/// forwarding headers are trusted (`TRUSTED_PROXIES`).
#[derive(Clone)]
pub struct TrafficContext {
    recorder: TrafficRecorder,
    trusted_proxies: Arc<Vec<IpNet>>,
}

impl TrafficContext {
    pub fn new(recorder: TrafficRecorder, trusted_proxies: Vec<IpNet>) -> Self {
        Self {
            recorder,
            trusted_proxies: Arc::new(trusted_proxies),
        }
    }
}

pub async fn record_traffic(
    State(context): State<TrafficContext>,
    request: Request,
    next: Next,
) -> Response {
    let TrafficContext {
        recorder,
        trusted_proxies,
    } = context;
    let started = Instant::now();
    let created_at = Utc::now();
    let surface = ApiSurface::from_path(request.uri().path());
    let method = request.method().to_string();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned());
    let ip = client_ip(&request, &trusted_proxies);

    let response = next.run(request).await;

    let Some(surface) = surface else {
        return response;
    };
    recorder.record(ApiTrafficEvent {
        surface,
        method,
        route: route.unwrap_or_else(|| UNMATCHED_ROUTE.to_string()),
        status: response.status().as_u16() as i16,
        duration_ms: started.elapsed().as_secs_f64() * 1000.0,
        api_key_id: response
            .extensions()
            .get::<CurrentApiKey>()
            .map(|k| k.id.clone()),
        staff_id: response
            .extensions()
            .get::<CurrentStaff>()
            .map(|s| s.id.clone()),
        ip,
        created_at,
    });
    response
}

fn client_ip(request: &Request, trusted_proxies: &[IpNet]) -> Option<String> {
    let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    resolve_client_ip(
        peer,
        header("x-forwarded-for"),
        header("x-real-ip"),
        trusted_proxies,
    )
    .map(|ip| ip.to_string())
}

/// Forwarding headers are client-forgeable, so they only count when the TCP
/// peer is a trusted proxy. Then the *last* `X-Forwarded-For` entry is the
/// client (the only hop the proxy itself appended — earlier ones came from
/// the client), falling back to `X-Real-IP`, then to the peer. Otherwise the
/// socket address is the client and the headers are ignored.
fn resolve_client_ip(
    peer: Option<IpAddr>,
    forwarded_for: Option<&str>,
    real_ip: Option<&str>,
    trusted_proxies: &[IpNet],
) -> Option<IpAddr> {
    let peer = peer?;
    if !trusted_proxies.iter().any(|net| net.contains(&peer)) {
        return Some(peer);
    }
    let parse = |value: &str| value.trim().parse::<IpAddr>().ok();
    forwarded_for
        .and_then(|header| header.split(',').next_back())
        .and_then(parse)
        .or_else(|| real_ip.and_then(parse))
        .or(Some(peer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use ipnet::IpNet;
    use tower::ServiceExt;

    const PROXY: &str = "10.0.0.5";

    fn app(recorder: TrafficRecorder, trusted: &[&str]) -> Router {
        let trusted: Vec<IpNet> = trusted.iter().map(|c| c.parse().unwrap()).collect();
        Router::new()
            .route("/api/v1/things/{id}", get(|| async { StatusCode::CREATED }))
            .route("/api/admin/v1/stats", get(|| async { StatusCode::OK }))
            .route("/healthz", get(|| async { StatusCode::OK }))
            .layer(axum::middleware::from_fn_with_state(
                TrafficContext::new(recorder, trusted),
                record_traffic,
            ))
    }

    fn from_peer(peer: &str, builder: axum::http::request::Builder) -> HttpRequest<Body> {
        let mut request = builder.body(Body::empty()).unwrap();
        let addr: SocketAddr = format!("{peer}:40000").parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(addr));
        request
    }

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    fn nets(cidrs: &[&str]) -> Vec<IpNet> {
        cidrs.iter().map(|c| c.parse().unwrap()).collect()
    }

    #[test]
    fn untrusted_peer_ignores_forwarding_headers() {
        let resolved = resolve_client_ip(
            Some(ip("203.0.113.9")),
            Some("1.2.3.4, 5.6.7.8"),
            Some("9.9.9.9"),
            &nets(&["10.0.0.0/8"]),
        );
        assert_eq!(resolved, Some(ip("203.0.113.9")));
    }

    #[test]
    fn no_trusted_proxies_means_socket_address() {
        let resolved = resolve_client_ip(Some(ip(PROXY)), Some("1.2.3.4"), None, &[]);
        assert_eq!(resolved, Some(ip(PROXY)));
    }

    #[test]
    fn trusted_proxy_uses_last_forwarded_hop() {
        // Earlier entries are client-supplied; only the last one was
        // appended by the proxy itself.
        let resolved = resolve_client_ip(
            Some(ip(PROXY)),
            Some("6.6.6.6, 198.51.100.7"),
            None,
            &nets(&["10.0.0.0/8"]),
        );
        assert_eq!(resolved, Some(ip("198.51.100.7")));
    }

    #[test]
    fn trusted_proxy_without_usable_forwarded_for_falls_back() {
        let trusted = nets(&["10.0.0.0/8"]);
        assert_eq!(
            resolve_client_ip(Some(ip(PROXY)), None, Some("198.51.100.7"), &trusted),
            Some(ip("198.51.100.7"))
        );
        assert_eq!(
            resolve_client_ip(Some(ip(PROXY)), Some("garbage"), None, &trusted),
            Some(ip(PROXY))
        );
        assert_eq!(resolve_client_ip(None, Some("1.2.3.4"), None, &trusted), None);
    }

    #[tokio::test]
    async fn records_matched_route_template_and_status() {
        let (recorder, mut rx) = TrafficRecorder::channel(8);
        let response = app(recorder, &["10.0.0.0/8"])
            .oneshot(from_peer(
                PROXY,
                HttpRequest::builder()
                    .uri("/api/v1/things/abc-123")
                    .header("x-forwarded-for", "6.6.6.6, 203.0.113.4"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let event = rx.try_recv().expect("one event recorded");
        assert_eq!(event.surface, ApiSurface::Merchant);
        assert_eq!(event.method, "GET");
        assert_eq!(event.route, "/api/v1/things/{id}");
        assert_eq!(event.status, 201);
        assert_eq!(event.ip.as_deref(), Some("203.0.113.4"));
        assert!(event.duration_ms >= 0.0);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn spoofed_headers_from_untrusted_peer_are_ignored() {
        let (recorder, mut rx) = TrafficRecorder::channel(8);
        app(recorder, &[])
            .oneshot(from_peer(
                "203.0.113.50",
                HttpRequest::builder()
                    .uri("/api/admin/v1/stats")
                    .header("x-forwarded-for", "1.1.1.1")
                    .header("x-real-ip", "198.51.100.7"),
            ))
            .await
            .unwrap();
        let event = rx.try_recv().unwrap();
        assert_eq!(event.surface, ApiSurface::Admin);
        assert_eq!(event.route, "/api/admin/v1/stats");
        assert_eq!(event.ip.as_deref(), Some("203.0.113.50"));
        assert!(event.api_key_id.is_none());
        assert!(event.staff_id.is_none());
    }

    #[tokio::test]
    async fn records_unmatched_api_paths_without_raw_path() {
        let (recorder, mut rx) = TrafficRecorder::channel(8);
        let response = app(recorder, &[])
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/nope/secret-value")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.route, UNMATCHED_ROUTE);
        assert_eq!(event.status, 404);
        assert!(event.ip.is_none());
    }

    #[tokio::test]
    async fn skips_health_checks() {
        let (recorder, mut rx) = TrafficRecorder::channel(8);
        app(recorder, &[])
            .oneshot(HttpRequest::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
    }
}
