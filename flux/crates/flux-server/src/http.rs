//! HTTP server for API endpoints.
//!
//! Provides:
//! - `/api/status` — server status JSON

use axum::{
    Router,
    http::header,
    response::{IntoResponse, Response},
    routing::get,
};

/// Build the axum router.
#[allow(dead_code)]
pub fn build_router() -> Router {
    Router::new()
        .route("/api/status", get(api_status))
        .route("/metrics", get(metrics))
}

/// Server status API.
async fn api_status() -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "status": "running",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn metrics() -> Response {
    match crate::metrics::encode() {
        Ok(body) => (
            [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
            body,
        )
            .into_response(),
        Err(error) => {
            tracing::warn!("Metrics encoding failed: {}", error);
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
