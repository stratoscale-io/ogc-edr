//! Choosing between the JSON and HTML representation of a resource.
//!
//! OGC API Common's HTML conformance class puts both representations on the
//! same URL, so the choice is made per request: an explicit `f=html` wins, and
//! otherwise the `Accept` header decides. A browser asks for `text/html` and
//! gets a page; `curl`, which sends `*/*`, keeps getting JSON.

use std::collections::HashMap;

use axum::extract::{FromRequestParts, Query};
use axum::http::header::ACCEPT;
use axum::http::request::Parts;
use std::convert::Infallible;

/// Which representation to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Representation {
    Json,
    Html,
}

impl Representation {
    pub fn is_html(self) -> bool {
        self == Representation::Html
    }
}

/// Extractor resolving the representation for a request.
#[derive(Debug, Clone, Copy)]
pub struct Wants(pub Representation);

impl<S: Send + Sync> FromRequestParts<S> for Wants {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let format = Query::<HashMap<String, String>>::from_request_parts(parts, state)
            .await
            .ok()
            .and_then(|Query(params)| {
                params
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("f"))
                    .map(|(_, value)| value.trim().to_ascii_lowercase())
            });

        // `f` is explicit and settles it. Any other value — CoverageJSON,
        // GeoJSON, or something unsupported — is left to the data handler,
        // which validates it and reports a usable error.
        if let Some(format) = format {
            let representation = if format == "html" || format == "text/html" {
                Representation::Html
            } else {
                Representation::Json
            };
            return Ok(Wants(representation));
        }

        let accept = parts
            .headers
            .get(ACCEPT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        Ok(Wants(if prefers_html(accept) {
            Representation::Html
        } else {
            Representation::Json
        }))
    }
}

/// Whether an `Accept` header asks for HTML in preference to JSON.
///
/// Only an explicit `text/html` counts: a browser sends
/// `text/html,…,*/*;q=0.8` and wants a page, while `*/*` on its own is a tool
/// taking whatever it is given, and this is a JSON API first. Ties go to JSON.
fn prefers_html(accept: &str) -> bool {
    const JSON_TYPES: &[&str] = &[
        "application/json",
        "application/geo+json",
        "application/prs.coverage+json",
        "application/*",
        "*/*",
    ];

    let (mut html_q, mut json_q) = (0.0f32, 0.0f32);
    for entry in accept.split(',') {
        let mut parts = entry.split(';').map(str::trim);
        let Some(media_type) = parts.next().map(|t| t.to_ascii_lowercase()) else {
            continue;
        };
        let quality = parts
            .find_map(|param| param.strip_prefix("q="))
            .and_then(|q| q.trim().parse::<f32>().ok())
            .unwrap_or(1.0);

        if media_type == "text/html" || media_type == "text/*" {
            html_q = html_q.max(quality);
        }
        if JSON_TYPES.contains(&media_type.as_str()) {
            json_q = json_q.max(quality);
        }
    }
    html_q > json_q
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browser_gets_html() {
        assert!(prefers_html(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,*/*;q=0.8"
        ));
        assert!(prefers_html("text/html"));
    }

    #[test]
    fn a_tool_asking_for_anything_keeps_json() {
        assert!(!prefers_html("*/*"));
        assert!(!prefers_html(""));
        assert!(!prefers_html("application/json"));
        assert!(!prefers_html("application/prs.coverage+json"));
    }

    #[test]
    fn quality_values_decide_a_contest() {
        assert!(!prefers_html("text/html;q=0.1, application/json"));
        assert!(prefers_html("text/html;q=0.9, application/json;q=0.5"));
        // A tie goes to JSON: this is an API that also renders pages.
        assert!(!prefers_html("text/html, application/json"));
    }

    #[test]
    fn malformed_entries_are_ignored_rather_than_fatal() {
        assert!(prefers_html("text/html;q=notanumber"));
        assert!(!prefers_html(";;;"));
    }
}
