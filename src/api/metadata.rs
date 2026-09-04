//! Landing page, conformance, and collection metadata.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::header::HOST;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::api::negotiate::Wants;
use crate::api::{AppState, TypedJson, html, json_response, link};
use crate::catalog::Collection;
use crate::edr::covjson::parameter_json;
use crate::edr::query::format_time;
use crate::error::EdrResult;

const JSON: &str = "application/json";
const HTML: &str = "text/html";

/// The conformance classes this server actually implements. Position, radius,
/// area and cube are served; trajectory, corridor, items, locations and
/// instances are not, and are deliberately absent.
pub const CONFORMANCE: &[&str] = &[
    "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/core",
    "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/json",
    "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/collections",
    "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/core",
    "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/collections",
    "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/json",
    "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/geojson",
    "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/covjson",
    "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/position",
    "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/radius",
    "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/area",
    "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/cube",
];

/// Query types with a `/collections/{id}/{query}` endpoint.
pub const QUERY_TYPES: &[(&str, &str)] = &[
    (
        "position",
        "Data at a position, or at each of several positions",
    ),
    ("radius", "Data within a radius of a position"),
    ("area", "Data within a polygon"),
    ("cube", "Data within a bounding box"),
];

pub const OUTPUT_FORMATS: &[&str] = &["CoverageJSON", "GeoJSON", "HTML"];

pub async fn landing(
    State(state): State<Arc<AppState>>,
    Wants(wants): Wants,
    headers: HeaderMap,
) -> Response {
    if wants.is_html() {
        let host = headers.get(HOST).and_then(|value| value.to_str().ok());
        return html::landing(&state, host).into_response();
    }
    json_response(json!({
        "title": "ERA5 Environmental Data Retrieval API",
        "description": "OGC API - Environmental Data Retrieval over ECMWF ERA5 reanalysis \
                        held as cloud-optimised Zarr, queried with zarr-datafusion.",
        "links": [
            link(state.href("/"), "self", JSON, "This document"),
            link(state.href("/api"), "service-desc", "application/vnd.oai.openapi+json;version=3.0", "API definition"),
            link(state.href("/conformance"), "conformance", JSON, "Conformance classes"),
            link(state.href("/collections"), "data", JSON, "Collections"),
            link(state.href("/"), "alternate", HTML, "This document as HTML"),
        ],
    }))
    .into_response()
}

pub async fn conformance(State(state): State<Arc<AppState>>, Wants(wants): Wants) -> Response {
    if wants.is_html() {
        return html::conformance(&state).into_response();
    }
    json_response(json!({
        "conformsTo": CONFORMANCE,
        "links": [
            link(state.href("/conformance"), "self", JSON, "Conformance classes"),
            link(state.href("/conformance"), "alternate", HTML, "This document as HTML"),
        ],
    }))
    .into_response()
}

pub async fn collections(State(state): State<Arc<AppState>>, Wants(wants): Wants) -> Response {
    if wants.is_html() {
        return html::collections(&state).into_response();
    }
    let collections: Vec<Value> = state
        .catalog
        .collections
        .values()
        .map(|c| collection_json(&state, c))
        .collect();

    json_response(json!({
        "links": [
            link(state.href("/collections"), "self", JSON, "Collections"),
            link(state.href("/collections"), "alternate", HTML, "This document as HTML"),
        ],
        "collections": collections,
    }))
    .into_response()
}

pub async fn collection(
    State(state): State<Arc<AppState>>,
    Path(collection_id): Path<String>,
    Wants(wants): Wants,
    headers: HeaderMap,
) -> EdrResult<Response> {
    let collection = state.catalog.collection(&collection_id)?;
    if wants.is_html() {
        let host = headers.get(HOST).and_then(|value| value.to_str().ok());
        return Ok(html::collection(&state, collection, host).into_response());
    }
    Ok(json_response(collection_json(&state, collection)).into_response())
}

/// The EDR collection object: extents, the query endpoints, and the parameters.
fn collection_json(state: &AppState, collection: &Collection) -> Value {
    let base = format!("/collections/{}", collection.id);
    let (t_start, t_end) = collection.time_extent();

    let mut extent = Map::new();
    extent.insert(
        "spatial".into(),
        json!({
            "bbox": [collection.bbox()],
            "crs": "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563]],\
                    PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
        }),
    );
    extent.insert(
        "temporal".into(),
        json!({
            "interval": [[format_time(t_start), format_time(t_end)]],
            "values": [format!("{}/{}", format_time(t_start), format_time(t_end))],
            "trs": "TIMECRS[\"DateTime\",TDATUM[\"Gregorian Calendar\"],\
                    CS[TemporalDateTime,1],AXIS[\"Time (T)\",future]]",
        }),
    );
    if let Some(z) = collection.z.as_ref() {
        extent.insert(
            "vertical".into(),
            json!({
                "interval": [[z.values.first().copied(), z.values.last().copied()]],
                "values": z.values,
                "vrs": format!(
                    "VERTCRS[\"{}\",VERT_CS[\"{}\"]]",
                    collection.z_name.clone().unwrap_or_else(|| "z".into()),
                    collection.z_units.clone().unwrap_or_else(|| "unknown".into()),
                ),
            }),
        );
    }

    let data_queries: Map<String, Value> = QUERY_TYPES
        .iter()
        .map(|(name, title)| {
            (
                (*name).to_string(),
                json!({
                    "link": {
                        "href": state.href(&format!("{base}/{name}")),
                        "rel": "data",
                        "type": "application/prs.coverage+json",
                        "title": title,
                        "variables": {
                            "title": title,
                            "query_type": name,
                            "output_formats": OUTPUT_FORMATS,
                            "default_output_format": "CoverageJSON",
                            "crs_details": [{
                                "crs": "CRS84",
                                "wkt": "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",\
                                        SPHEROID[\"WGS 84\",6378137,298.257223563]],\
                                        PRIMEM[\"Greenwich\",0],\
                                        UNIT[\"degree\",0.0174532925199433]]",
                            }],
                        }
                    }
                }),
            )
        })
        .collect();

    let parameter_names: Map<String, Value> = collection
        .parameters
        .values()
        .map(|p| (p.name.clone(), parameter_json(p)))
        .collect();

    json!({
        "id": collection.id,
        "title": collection.title,
        "description": collection.description,
        "keywords": collection.keywords,
        "extent": Value::Object(extent),
        "links": [
            link(state.href(&base), "self", JSON, &collection.title),
            link(state.href(&base), "alternate", HTML, "This document as HTML"),
            link(state.href("/collections"), "collection", JSON, "Collections"),
        ],
        "data_queries": Value::Object(data_queries),
        "crs": ["CRS84"],
        "output_formats": OUTPUT_FORMATS,
        "parameter_names": Value::Object(parameter_names),
    })
}

/// A hand-written OpenAPI document covering the endpoints this server serves.
pub async fn api_definition(State(state): State<Arc<AppState>>) -> TypedJson {
    let collection_ids: Vec<&String> = state.catalog.collections.keys().collect();

    let common_params = json!([
        { "name": "parameter-name", "in": "query", "required": true,
          "description": "Comma-separated parameter names.",
          "schema": { "type": "string" } },
        { "name": "datetime", "in": "query",
          "description": "RFC 3339 instant or interval; '..' leaves an end open. \
                          Defaults to the latest available time step.",
          "schema": { "type": "string" } },
        { "name": "z", "in": "query",
          "description": "Vertical level: a value, a comma-separated list, 'min/max', \
                          'R{count}/{start}/{step}', or 'all'.",
          "schema": { "type": "string" } },
        { "name": "crs", "in": "query", "schema": { "type": "string", "default": "CRS84" } },
        { "name": "f", "in": "query",
          "schema": { "type": "string", "enum": OUTPUT_FORMATS, "default": "CoverageJSON" } },
        { "name": "limit", "in": "query",
          "description": "Maximum number of data values in the response.",
          "schema": { "type": "integer", "minimum": 1 } },
    ]);

    let mut paths = Map::new();
    paths.insert(
        "/".into(),
        json!({ "get": { "summary": "Landing page", "responses": { "200": { "description": "OK" } } } }),
    );
    paths.insert(
        "/conformance".into(),
        json!({ "get": { "summary": "Conformance classes", "responses": { "200": { "description": "OK" } } } }),
    );
    paths.insert(
        "/collections".into(),
        json!({ "get": { "summary": "Collections", "responses": { "200": { "description": "OK" } } } }),
    );
    paths.insert(
        "/collections/{collectionId}".into(),
        json!({ "get": {
            "summary": "Collection metadata",
            "parameters": [collection_id_param(&collection_ids)],
            "responses": { "200": { "description": "OK" }, "404": { "description": "Unknown collection" } }
        }}),
    );

    for (name, title) in QUERY_TYPES {
        let mut parameters = vec![collection_id_param(&collection_ids)];
        match *name {
            "cube" => parameters.push(json!({
                "name": "bbox", "in": "query", "required": true,
                "description": "west,south,east,north (optionally with min/max z as six values).",
                "schema": { "type": "string" }
            })),
            _ => parameters.push(json!({
                "name": "coords", "in": "query", "required": true,
                "description": "WKT geometry in CRS84.",
                "schema": { "type": "string" }
            })),
        }
        if *name == "radius" {
            parameters.push(json!({
                "name": "within", "in": "query", "required": true,
                "schema": { "type": "number" }
            }));
            parameters.push(json!({
                "name": "within-units", "in": "query",
                "schema": { "type": "string", "enum": ["m", "km", "mi", "nmi", "ft"], "default": "km" }
            }));
        }
        if matches!(*name, "area" | "cube") {
            for axis in ["x", "y", "z"] {
                parameters.push(json!({
                    "name": format!("resolution-{axis}"), "in": "query",
                    "description": "Thin the axis to this many points.",
                    "schema": { "type": "integer", "minimum": 1 }
                }));
            }
        }
        parameters.extend(common_params.as_array().expect("array").iter().cloned());

        paths.insert(
            format!("/collections/{{collectionId}}/{name}"),
            json!({ "get": {
                "summary": title,
                "parameters": parameters,
                "responses": {
                    "200": { "description": "CoverageJSON or GeoJSON" },
                    "400": { "description": "Invalid query" },
                    "404": { "description": "Nothing selected" },
                    "413": { "description": "Query selects too much data" }
                }
            }}),
        );
    }

    TypedJson(
        "application/vnd.oai.openapi+json;version=3.0",
        json!({
            "openapi": "3.0.3",
            "info": {
                "title": "ERA5 Environmental Data Retrieval API",
                "version": env!("CARGO_PKG_VERSION"),
                "description": "OGC API - EDR over ERA5 Zarr, backed by zarr-datafusion.",
            },
            "servers": [{ "url": if state.config.base_url.is_empty() { "/" } else { &state.config.base_url } }],
            "paths": Value::Object(paths),
        }),
    )
}

fn collection_id_param(ids: &[&String]) -> Value {
    json!({
        "name": "collectionId",
        "in": "path",
        "required": true,
        "schema": { "type": "string", "enum": ids },
    })
}
