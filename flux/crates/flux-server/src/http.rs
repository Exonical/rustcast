//! HTTP server for API endpoints.
//!
//! Provides:
//! - `/api/status` — server status JSON

use axum::{
    Router,
    response::IntoResponse,
    routing::get,
};

/// Build the axum router.
#[allow(dead_code)]
pub fn build_router() -> Router {
    Router::new()
        .route("/api/status", get(api_status))
}

/// Server status API.
async fn api_status() -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "status": "running",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
