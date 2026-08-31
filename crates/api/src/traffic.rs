//! Request-recording middleware for the API traffic log.
//!
//! Sits *outside* the auth guards (see the layer order in `router`) so it
//! observes rejected requests too, and reads the caller identity the guards
//! copy into the response extensions. Recording is a non-blocking
//! `try_send`; the only thing awaited is the inner handler.

use std::net::SocketAddr;
use std::time::Instant;

use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use chrono::Utc;
use db::ApiTrafficEvent;
use domain::ApiSurface;
use jobs::TrafficRecorder;

use crate::middleware::{CurrentApiKey, CurrentStaff};

/// Route recorded for `/api/...` requests that matched no route (404s) —
/// never the raw path, to keep cardinality bounded.
pub const UNMATCHED_ROUTE: &str = "<unmatched>";

pub async fn record_traffic(
    State(recorder): State<TrafficRecorder>,
    request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let created_at = Utc::now();
    let surface = ApiSurface::from_path(request.uri().path());
    let method = request.method().to_string();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned());
    let ip = client_ip(&request);

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

/// First `X-Forwarded-For` hop, else `X-Real-IP`, else the peer address.
/// Proxy headers are trusted because the documented deployment sits behind
/// nginx; callers hitting the API port directly can forge them.
fn client_ip(request: &Request) -> Option<String> {
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    header("x-forwarded-for")
        .or_else(|| header("x-real-ip"))
        .or_else(|| {
            request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0.ip().to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn app(recorder: TrafficRecorder) -> Router {
        Router::new()
            .route("/api/v1/things/{id}", get(|| async { StatusCode::CREATED }))
            .route("/api/admin/v1/stats", get(|| async { StatusCode::OK }))
            .route("/healthz", get(|| async { StatusCode::OK }))
            .layer(axum::middleware::from_fn_with_state(recorder, record_traffic))
    }

    #[tokio::test]
    async fn records_matched_route_template_and_status() {
        let (recorder, mut rx) = TrafficRecorder::channel(8);
        let response = app(recorder)
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/things/abc-123")
                    .header("x-forwarded-for", "203.0.113.4, 10.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
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
    async fn tags_admin_surface_and_falls_back_to_real_ip() {
        let (recorder, mut rx) = TrafficRecorder::channel(8);
        app(recorder)
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/admin/v1/stats")
                    .header("x-real-ip", "198.51.100.7")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let event = rx.try_recv().unwrap();
        assert_eq!(event.surface, ApiSurface::Admin);
        assert_eq!(event.route, "/api/admin/v1/stats");
        assert_eq!(event.ip.as_deref(), Some("198.51.100.7"));
        assert!(event.api_key_id.is_none());
        assert!(event.staff_id.is_none());
    }

    #[tokio::test]
    async fn records_unmatched_api_paths_without_raw_path() {
        let (recorder, mut rx) = TrafficRecorder::channel(8);
        let response = app(recorder)
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
        app(recorder)
            .oneshot(HttpRequest::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
    }
}
