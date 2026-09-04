//! The EDR data queries: position, radius, area and cube.

use std::sync::Arc;

use axum::extract::{Path, Query as AxumQuery, State};
use axum::http::Uri;
use axum::response::{IntoResponse, Response};

use crate::api::negotiate::Wants;
use crate::api::{AppState, TypedJson, html};
use crate::edr::params::{Format, Query};
use crate::edr::query::{self, DataRequest, Limits, QueryKind, ResultSet};
use crate::edr::wkt::{self, Geometry};
use crate::edr::{covjson, geojson};
use crate::error::{EdrError, EdrResult};

pub async fn position(
    state: State<Arc<AppState>>,
    path: Path<String>,
    params: AxumQuery<Vec<(String, String)>>,
    wants: Wants,
    uri: Uri,
) -> Response {
    run(state, path, params, wants, uri, QueryKind::Position).await
}

pub async fn radius(
    state: State<Arc<AppState>>,
    path: Path<String>,
    params: AxumQuery<Vec<(String, String)>>,
    wants: Wants,
    uri: Uri,
) -> Response {
    run(state, path, params, wants, uri, QueryKind::Radius).await
}

pub async fn area(
    state: State<Arc<AppState>>,
    path: Path<String>,
    params: AxumQuery<Vec<(String, String)>>,
    wants: Wants,
    uri: Uri,
) -> Response {
    run(state, path, params, wants, uri, QueryKind::Area).await
}

pub async fn cube(
    state: State<Arc<AppState>>,
    path: Path<String>,
    params: AxumQuery<Vec<(String, String)>>,
    wants: Wants,
    uri: Uri,
) -> Response {
    run(state, path, params, wants, uri, QueryKind::Cube).await
}

async fn run(
    State(state): State<Arc<AppState>>,
    Path(collection_id): Path<String>,
    AxumQuery(raw): AxumQuery<Vec<(String, String)>>,
    Wants(wants): Wants,
    uri: Uri,
    kind: QueryKind,
) -> Response {
    // An unknown collection has no form to render, so it is reported the same
    // way in either representation.
    let collection = match state.catalog.collection(&collection_id) {
        Ok(collection) => collection.clone(),
        Err(error) => return error.into_response(),
    };
    let params = Query::new(raw);

    if wants.is_html() {
        let query_string = uri.query().unwrap_or_default().to_string();
        // The page is rendered inside each arm, because a result outcome
        // borrows the request and result that arm owns.
        let render = |outcome| html::query_page(&state, &collection, kind, &params, outcome);
        let page = match answer(&state, &collection, &params, kind).await {
            // No geometry yet: this is the form being opened, not a mistake.
            Err(EdrError::BadRequest(_)) if !has_geometry(&params, kind) => {
                render(html::Outcome::Blank)
            }
            Err(error) => render(html::Outcome::Failed(error.description().to_string())),
            Ok((request, result)) => render(html::Outcome::Data {
                request: &request,
                result: &result,
                query_string,
            }),
        };
        return page.into_response();
    }

    let format = match params.format() {
        Ok(format) => format,
        Err(error) => return error.into_response(),
    };
    match answer(&state, &collection, &params, kind).await {
        Err(error) => error.into_response(),
        Ok((request, result)) => {
            let body = match format {
                Format::CoverageJson => covjson::encode(&request, &result),
                Format::GeoJson => geojson::encode(&request, &result),
            };
            TypedJson(format.media_type(), body).into_response()
        }
    }
}

/// Whether the request carries the geometry its query type needs. Without it
/// the HTML representation shows an empty form rather than an error.
fn has_geometry(params: &Query, kind: QueryKind) -> bool {
    match kind {
        QueryKind::Cube => params.get("bbox").is_some(),
        _ => params.get("coords").is_some(),
    }
}

/// Resolve and run a query, whichever representation asked for it.
async fn answer(
    state: &AppState,
    collection: &std::sync::Arc<crate::catalog::Collection>,
    params: &Query,
    kind: QueryKind,
) -> EdrResult<(DataRequest, ResultSet)> {
    let geometry = geometry_for(kind, params)?;

    let request = query::resolve(
        collection,
        kind,
        &geometry,
        params,
        Limits {
            max_scan_rows: state.config.max_values,
            default_limit: state.config.default_limit,
        },
    )?;

    let started = std::time::Instant::now();
    let result = query::execute(&state.catalog.ctx, &request).await?;
    tracing::info!(
        collection = %collection.id,
        query = kind.path(),
        parameters = request.parameters.len(),
        values = request.selection.value_count(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "served"
    );

    if result.is_empty() {
        return Err(EdrError::NotFound(
            "The store holds no values for that selection; it may fall outside the \
             period the collection actually covers"
                .into(),
        ));
    }
    Ok((request, result))
}

/// `cube` is bounded by `bbox`; the others take a WKT `coords` geometry.
fn geometry_for(kind: QueryKind, params: &Query) -> EdrResult<Geometry> {
    match kind {
        QueryKind::Cube => {
            let ([w, s, e, n], _) = params
                .bbox()?
                .ok_or_else(|| EdrError::BadRequest("The cube query requires 'bbox'".into()))?;
            // Represent the box as a closed ring so it shares the geometry path.
            Ok(Geometry::Polygon(vec![vec![
                wkt::Coord { x: w, y: s },
                wkt::Coord { x: e, y: s },
                wkt::Coord { x: e, y: n },
                wkt::Coord { x: w, y: n },
                wkt::Coord { x: w, y: s },
            ]]))
        }
        _ => wkt::parse(params.required("coords")?),
    }
}
