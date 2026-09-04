//! HTTP routing and shared response helpers.

pub mod data;
pub mod html;
pub mod metadata;
pub mod negotiate;

use std::sync::Arc;

use axum::Router;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

use crate::catalog::Catalog;
use crate::config::Config;

pub struct AppState {
    pub catalog: Catalog,
    pub config: Config,
}

impl AppState {
    /// Absolute or root-relative href for an API path.
    pub fn href(&self, path: &str) -> String {
        format!("{}{}", self.config.base_url, path)
    }

    /// An href a reader can paste into a shell.
    ///
    /// Root-relative links are right for navigation but useless in a `curl`
    /// example, so a configured base URL is used when there is one and the
    /// request's own `Host` otherwise.
    pub fn absolute_href(&self, path: &str, host: Option<&str>) -> String {
        if !self.config.base_url.is_empty() {
            return self.href(path);
        }
        match host {
            Some(host) => format!("http://{host}{path}"),
            None => self.href(path),
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(metadata::landing))
        .route(html::STYLESHEET_PATH.as_str(), get(stylesheet))
        .route("/conformance", get(metadata::conformance))
        .route("/api", get(metadata::api_definition))
        .route("/collections", get(metadata::collections))
        .route("/collections/{collection_id}", get(metadata::collection))
        .route("/collections/{collection_id}/position", get(data::position))
        .route("/collections/{collection_id}/radius", get(data::radius))
        .route("/collections/{collection_id}/area", get(data::area))
        .route("/collections/{collection_id}/cube", get(data::cube))
        .fallback(not_found)
        .with_state(state)
}

/// water.css plus this application's rules, as one response. The path carries
/// a hash of the content, so caching it hard is safe: an edit changes the URL.
async fn stylesheet() -> Response {
    (
        [
            (CONTENT_TYPE, "text/css; charset=utf-8"),
            (CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        html::STYLESHEET,
    )
        .into_response()
}

async fn not_found() -> Response {
    crate::error::EdrError::NotFound("No such resource".into()).into_response()
}

/// A JSON response with an explicit media type — EDR clients dispatch on it.
pub struct TypedJson(pub &'static str, pub Value);

impl IntoResponse for TypedJson {
    fn into_response(self) -> Response {
        ([(CONTENT_TYPE, self.0)], axum::Json(self.1)).into_response()
    }
}

pub fn json_response(value: Value) -> TypedJson {
    TypedJson("application/json", value)
}

pub fn link(href: String, rel: &str, media_type: &str, title: &str) -> Value {
    json!({
        "href": href,
        "rel": rel,
        "type": media_type,
        "title": title,
    })
}
