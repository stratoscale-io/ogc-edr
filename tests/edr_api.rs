//! End-to-end tests against a local Zarr fixture: every request goes through
//! the real router, catalog and zarr-datafusion scan.

mod fixture;

use std::sync::Arc;

use axum::body::Body;
use axum::http::header::{ACCEPT, HOST};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ogc_edr::api::{AppState, router};
use ogc_edr::catalog::Catalog;
use ogc_edr::config::{CollectionConfig, Config};
use serde_json::Value;
use tower::ServiceExt;

/// Build the store once per process and share the opened catalog.
async fn app() -> axum::Router {
    let root = std::env::temp_dir().join(format!("ogc-edr-fixture-{}", std::process::id()));
    if !root.join(".zmetadata").exists() {
        fixture::write_store(&root).expect("write fixture");
    }
    let config = Config {
        bind: "127.0.0.1:0".into(),
        base_url: String::new(),
        max_values: 1_000_000,
        default_limit: 100_000,
        collections: vec![CollectionConfig {
            id: "era5".into(),
            title: Some("ERA5".into()),
            description: Some("Test fixture".into()),
            location: root.to_string_lossy().into_owned(),
            keywords: vec!["test".into()],
            parameters: None,
        }],
    };
    let catalog = Catalog::open(&config.collections)
        .await
        .expect("open catalog");
    router(Arc::new(AppState { catalog, config }))
}

/// Fetch a path with an explicit `Accept`, returning the content type and body.
async fn fetch(path: &str, accept: &str) -> (StatusCode, String, String) {
    let response = app()
        .await
        .oneshot(
            Request::builder()
                .uri(path)
                .header(ACCEPT, accept)
                .header(HOST, "edr.example:3000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn get(path: &str) -> (StatusCode, Value) {
    let response = app()
        .await
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .expect("request");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "non-JSON response for {path}: {e}\n{}",
            String::from_utf8_lossy(&bytes)
        )
    });
    (status, body)
}

async fn ok(path: &str) -> Value {
    let (status, body) = get(path).await;
    assert_eq!(status, StatusCode::OK, "{path} -> {body}");
    body
}

fn floats(range: &Value) -> Vec<Option<f64>> {
    range["values"]
        .as_array()
        .expect("values array")
        .iter()
        .map(|v| v.as_f64())
        .collect()
}

#[tokio::test]
async fn landing_and_conformance_advertise_what_is_implemented() {
    let landing = ok("/").await;
    let rels: Vec<&str> = landing["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["rel"].as_str().unwrap())
        .collect();
    assert!(rels.contains(&"self") && rels.contains(&"data") && rels.contains(&"conformance"));

    let conformance = ok("/conformance").await;
    let classes: Vec<&str> = conformance["conformsTo"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    for required in [
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/core",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/position",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/area",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/cube",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/radius",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/covjson",
    ] {
        assert!(classes.contains(&required), "missing {required}");
    }
    // Query types with no endpoint must not be advertised.
    for absent in ["trajectory", "corridor", "locations", "items"] {
        assert!(
            !classes.iter().any(|c| c.ends_with(absent)),
            "advertises unimplemented {absent}"
        );
    }
}

#[tokio::test]
async fn collection_metadata_reports_real_extents_and_units() {
    let collection = ok("/collections/era5").await;
    assert_eq!(collection["id"], "era5");

    // A wrapping longitude axis is reported as global, in CRS84.
    let bbox = collection["extent"]["spatial"]["bbox"][0]
        .as_array()
        .unwrap();
    let bbox: Vec<f64> = bbox.iter().map(|v| v.as_f64().unwrap()).collect();
    assert_eq!(bbox, vec![-180.0, -90.0, 180.0, 90.0]);

    // The temporal extent follows the declared validity window, not the axis.
    let interval = &collection["extent"]["temporal"]["interval"][0];
    assert_eq!(interval[0], "2024-01-01T00:00:00Z");
    assert_eq!(interval[1], "2024-01-01T03:00:00Z");

    let levels = collection["extent"]["vertical"]["values"]
        .as_array()
        .unwrap();
    assert_eq!(levels.len(), 3);
    assert_eq!(
        collection["extent"]["vertical"]["vrs"],
        "VERTCRS[\"level\",VERT_CS[\"Hectopascal(hPa)\"]]"
    );

    // Units and labels come from the store's own CF attributes.
    let t2m = &collection["parameter_names"]["2m_temperature"];
    assert_eq!(t2m["unit"]["symbol"]["value"], "K");
    assert_eq!(t2m["label"]["en"], "2 metre temperature");
    assert_eq!(
        collection["parameter_names"]["total_precipitation"]["unit"]["symbol"]["value"],
        "m"
    );
    assert_eq!(
        collection["parameter_names"]["temperature"]["observedProperty"]["id"],
        "http://vocab.nerc.ac.uk/standard_name/air_temperature/"
    );

    // Every advertised query type has an endpoint.
    for query in ["position", "radius", "area", "cube"] {
        assert!(
            collection["data_queries"][query]["link"]["href"]
                .as_str()
                .unwrap()
                .ends_with(query)
        );
    }
}

#[tokio::test]
async fn position_returns_the_nearest_cell_as_a_point_series() {
    // Latitude 40 snaps to 45. Longitude -10 is 350 on the stored axis, whose
    // nearest stored value is 315 — but measured around the circle 0 is closer,
    // and that is the cell EDR should return.
    let body = ok("/collections/era5/position?coords=POINT(-10%2040)\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z/2024-01-01T03:00:00Z")
    .await;

    assert_eq!(body["domain"]["domainType"], "PointSeries");
    assert_eq!(body["domain"]["axes"]["x"]["values"][0], 0.0);
    assert_eq!(body["domain"]["axes"]["y"]["values"][0], 45.0);
    assert_eq!(
        body["domain"]["axes"]["t"]["values"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    // A standalone coverage must carry its own parameters block.
    assert_eq!(
        body["parameters"]["2m_temperature"]["unit"]["symbol"]["value"],
        "K"
    );

    // latitude 45 is index 1, longitude 0 is index 0.
    let expected: Vec<Option<f64>> = (0..fixture::TIMES)
        .map(|t| Some(fixture::surface_value(t, 1, 0) as f64))
        .collect();
    assert_eq!(floats(&body["ranges"]["2m_temperature"]), expected);
}

#[tokio::test]
async fn omitting_datetime_returns_the_latest_step() {
    let body =
        ok("/collections/era5/position?coords=POINT(0%2045)&parameter-name=2m_temperature").await;
    assert_eq!(body["domain"]["domainType"], "Point");
    assert_eq!(
        body["domain"]["axes"]["t"]["values"][0],
        "2024-01-01T03:00:00Z"
    );
}

#[tokio::test]
async fn multipoint_returns_a_coverage_collection() {
    let body = ok(
        "/collections/era5/position?coords=MULTIPOINT((0%2045),(90%200))\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z",
    )
    .await;

    assert_eq!(body["type"], "CoverageCollection");
    let coverages = body["coverages"].as_array().unwrap();
    assert_eq!(coverages.len(), 2);
    // Parameters are declared once, at the collection level.
    assert!(body["parameters"]["2m_temperature"].is_object());
    assert!(coverages[0]["parameters"].is_null());

    let mut seen: Vec<(f64, f64, f64)> = coverages
        .iter()
        .map(|c| {
            (
                c["domain"]["axes"]["x"]["values"][0].as_f64().unwrap(),
                c["domain"]["axes"]["y"]["values"][0].as_f64().unwrap(),
                c["ranges"]["2m_temperature"]["values"][0].as_f64().unwrap(),
            )
        })
        .collect();
    seen.sort_by(|a, b| a.0.total_cmp(&b.0));
    // (lat 45, lon 0) is y=1, x=0; (lat 0, lon 90) is y=2, x=2.
    assert_eq!(seen[0], (0.0, 45.0, fixture::surface_value(0, 1, 0) as f64));
    assert_eq!(seen[1], (90.0, 0.0, fixture::surface_value(0, 2, 2) as f64));
}

#[tokio::test]
async fn a_cube_across_the_prime_meridian_returns_an_ordered_grid() {
    let body = ok("/collections/era5/cube?bbox=-50,-50,50,50\
         &parameter-name=2m_temperature&datetime=2024-01-01T01:00:00Z")
    .await;

    assert_eq!(body["domain"]["domainType"], "Grid");
    // Longitudes from both ends of the 0…360 axis, presented in CRS84 order.
    let xs: Vec<f64> = body["domain"]["axes"]["x"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    assert_eq!(xs, vec![-45.0, 0.0, 45.0]);
    let ys: Vec<f64> = body["domain"]["axes"]["y"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    assert_eq!(ys, vec![-45.0, 0.0, 45.0]);

    let range = &body["ranges"]["2m_temperature"];
    assert_eq!(range["shape"], serde_json::json!([1, 3, 3]));
    let values = floats(range);
    assert!(
        values.iter().all(Option::is_some),
        "grid has holes: {values:?}"
    );

    // Fixture indices: lat -45 is y=3, 0 is y=2, 45 is y=1;
    // lon -45 is x=7 (315), 0 is x=0, 45 is x=1.
    let expected: Vec<Option<f64>> = [3usize, 2, 1]
        .iter()
        .flat_map(|&y| {
            [7usize, 0, 1]
                .iter()
                .map(move |&x| Some(fixture::surface_value(1, y, x) as f64))
        })
        .collect();
    assert_eq!(values, expected);
}

#[tokio::test]
async fn an_area_matches_the_equivalent_cube() {
    let area = ok(
        "/collections/era5/area?coords=POLYGON((0%200,90%200,90%2045,0%2045,0%200))\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z",
    )
    .await;
    let cube = ok("/collections/era5/cube?bbox=0,0,90,45\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z")
    .await;
    assert_eq!(
        area["ranges"], cube["ranges"],
        "a rectangular polygon must equal its cube"
    );
    assert!(
        floats(&area["ranges"]["2m_temperature"])
            .iter()
            .all(Option::is_some)
    );
}

#[tokio::test]
async fn an_area_masks_cells_outside_the_polygon() {
    // A triangle over the same bounding box leaves the far corner empty.
    let body = ok(
        "/collections/era5/area?coords=POLYGON((0%200,90%200,90%2045,0%200))\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z",
    )
    .await;
    let values = floats(&body["ranges"]["2m_temperature"]);
    assert!(values.iter().any(Option::is_some), "nothing selected");
    assert!(values.iter().any(Option::is_none), "no cell was masked out");
}

#[tokio::test]
async fn vertical_selections_drive_the_z_axis() {
    let body = ok("/collections/era5/position?coords=POINT(0%2045)\
         &parameter-name=temperature&datetime=2024-01-01T00:00:00Z&z=500,1000")
    .await;

    assert_eq!(body["domain"]["domainType"], "VerticalProfile");
    let zs: Vec<f64> = body["domain"]["axes"]["z"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    assert_eq!(zs, vec![500.0, 1000.0]);

    let range = &body["ranges"]["temperature"];
    assert_eq!(range["axisNames"], serde_json::json!(["t", "z", "y", "x"]));
    // Levels 500 and 1000 are fixture z-indices 0 and 2; lat 45 is y=1, lon 0 is x=0.
    assert_eq!(
        floats(range),
        vec![
            Some(fixture::level_value(0, 0, 1, 0) as f64),
            Some(fixture::level_value(0, 2, 1, 0) as f64),
        ]
    );

    // Omitting z returns every level.
    let all = ok("/collections/era5/position?coords=POINT(0%2045)\
         &parameter-name=temperature&datetime=2024-01-01T00:00:00Z")
    .await;
    assert_eq!(
        all["domain"]["axes"]["z"]["values"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn surface_and_pressure_level_parameters_can_be_requested_together() {
    // These have different dimensionality and cannot share one scan.
    let body = ok("/collections/era5/position?coords=POINT(0%2045)\
         &parameter-name=2m_temperature,temperature&datetime=2024-01-01T00:00:00Z&z=850")
    .await;

    let surface = &body["ranges"]["2m_temperature"];
    assert_eq!(surface["axisNames"], serde_json::json!(["t", "y", "x"]));
    assert_eq!(
        floats(surface),
        vec![Some(fixture::surface_value(0, 1, 0) as f64)]
    );

    let levelled = &body["ranges"]["temperature"];
    assert_eq!(
        levelled["axisNames"],
        serde_json::json!(["t", "z", "y", "x"])
    );
    assert_eq!(
        floats(levelled),
        vec![Some(fixture::level_value(0, 1, 1, 0) as f64)]
    );
}

#[tokio::test]
async fn radius_keeps_only_cells_within_the_distance() {
    let near = ok(
        "/collections/era5/radius?coords=POINT(0%200)&within=100&within-units=km\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z",
    )
    .await;
    let wide = ok(
        "/collections/era5/radius?coords=POINT(0%200)&within=6000&within-units=km\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z",
    )
    .await;

    let count = |b: &Value| {
        floats(&b["ranges"]["2m_temperature"])
            .iter()
            .filter(|v| v.is_some())
            .count()
    };
    assert!(count(&near) >= 1);
    assert!(
        count(&wide) > count(&near),
        "a wider radius must cover more cells"
    );
}

#[tokio::test]
async fn geojson_output_carries_one_feature_per_cell_and_time() {
    let body = ok(
        "/collections/era5/cube?bbox=0,0,45,45\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z/2024-01-01T01:00:00Z&f=GeoJSON",
    )
    .await;

    assert_eq!(body["type"], "FeatureCollection");
    let features = body["features"].as_array().unwrap();
    // 2 lats x 2 lons x 2 times.
    assert_eq!(features.len(), 8);
    let first = &features[0];
    assert_eq!(first["geometry"]["type"], "Point");
    assert!(first["properties"]["datetime"].is_string());
    assert!(first["properties"]["2m_temperature"].is_number());
}

#[tokio::test]
async fn resolution_thins_the_grid_but_keeps_the_edges() {
    let body = ok(
        "/collections/era5/cube?bbox=0,-90,180,90\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z&resolution-x=2&resolution-y=2",
    )
    .await;
    let xs: Vec<f64> = body["domain"]["axes"]["x"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    let ys: Vec<f64> = body["domain"]["axes"]["y"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    assert_eq!(xs, vec![0.0, 180.0]);
    assert_eq!(ys, vec![-90.0, 90.0]);
}

#[tokio::test]
async fn invalid_requests_are_rejected_with_an_edr_exception() {
    let cases = [
        ("/collections/nope", StatusCode::NOT_FOUND),
        (
            "/collections/era5/position?coords=POINT(0%2051.5)",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/collections/era5/position?coords=POINT(0%2045)&parameter-name=nope",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/collections/era5/position?coords=CIRCLE(0%200,5)&parameter-name=2m_temperature",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/collections/era5/position?coords=POLYGON((0%200,45%200,45%2045,0%200))&parameter-name=2m_temperature",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/collections/era5/position?coords=POINT(0%2045)&parameter-name=2m_temperature&f=netcdf",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/collections/era5/position?coords=POINT(0%2045)&parameter-name=2m_temperature&crs=EPSG:3857",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/collections/era5/position?coords=POINT(0%2045)&parameter-name=2m_temperature&z=500",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/collections/era5/position?coords=POINT(0%2045)&parameter-name=temperature&z=512",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/collections/era5/position?coords=POINT(0%2045)&parameter-name=2m_temperature&datetime=1850-01-01T00:00:00Z",
            StatusCode::NOT_FOUND,
        ),
        (
            "/collections/era5/area?parameter-name=2m_temperature",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/collections/era5/cube?parameter-name=2m_temperature",
            StatusCode::BAD_REQUEST,
        ),
        ("/collections/era5/trajectory", StatusCode::NOT_FOUND),
    ];
    for (path, expected) in cases {
        let (status, body) = get(path).await;
        assert_eq!(status, expected, "{path} -> {body}");
        assert_eq!(body["code"], expected.as_u16(), "{path}");
        assert!(
            body["description"].as_str().is_some_and(|d| !d.is_empty()),
            "{path} has no description"
        );
    }
}

#[tokio::test]
async fn an_oversized_request_is_refused_rather_than_served() {
    let (status, body) = get("/collections/era5/cube?bbox=-180,-90,180,90\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z&limit=5")
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body["description"].as_str().unwrap().contains("limit of 5"));
}

#[tokio::test]
async fn the_same_url_serves_html_to_a_browser_and_json_to_a_tool() {
    // A browser's Accept header.
    let (status, content_type, body) = fetch(
        "/",
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/html"), "{content_type}");
    assert!(body.starts_with("<!DOCTYPE html>"), "{body}");

    // Anything else keeps the JSON representation.
    let (_, content_type, body) = fetch("/", "*/*").await;
    assert_eq!(content_type, "application/json");
    assert!(body.starts_with('{'));
}

#[tokio::test]
async fn the_f_parameter_overrides_the_accept_header() {
    let (_, content_type, _) = fetch("/?f=html", "*/*").await;
    assert!(
        content_type.starts_with("text/html"),
        "f=html must win over Accept"
    );

    let (_, content_type, _) = fetch("/?f=json", "text/html").await;
    assert_eq!(
        content_type, "application/json",
        "f=json must win over Accept"
    );
}

#[tokio::test]
async fn the_landing_page_links_onward_and_names_the_collection() {
    let (_, _, body) = fetch("/", "text/html").await;

    for href in ["/collections", "/conformance", "/api"] {
        assert!(
            body.contains(&format!("href=\"{href}\"")),
            "no link to {href}"
        );
    }
    // The stylesheet is served by this binary, not fetched from a CDN.
    assert!(body.contains("href=\"/static/style."), "no stylesheet link");
    assert!(!body.contains("//cdn."), "must not depend on a CDN");

    // With one collection it is named rather than counted.
    assert!(body.contains("ERA5"), "collection not named");
    assert!(
        body.contains("/collections/era5"),
        "no link into the collection"
    );

    // The example command is runnable as printed, using the request's own host.
    assert!(
        body.contains("http://edr.example:3000/collections/era5/position"),
        "the example command is not an absolute URL"
    );
}

#[tokio::test]
async fn the_json_landing_page_advertises_its_html_alternate() {
    let landing = ok("/").await;
    let alternate = landing["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == "alternate")
        .expect("no alternate link");
    assert_eq!(alternate["type"], "text/html");
}

#[tokio::test]
async fn the_stylesheet_is_served_and_cacheable() {
    let response = app()
        .await
        .oneshot(
            Request::builder()
                .uri(ogc_edr::api::html::STYLESHEET_PATH.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/css; charset=utf-8"
    );
    // The path carries the crate version, so it can be cached indefinitely.
    assert!(
        response
            .headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("immutable")
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let css = String::from_utf8_lossy(&body);
    assert!(css.contains("prefers-color-scheme"), "water.css is missing");
    assert!(css.contains(".resources"), "app overrides are missing");

    // Side-by-side controls are spaced by margin, not flex `gap`. Safari
    // before 14.1 ignores `gap` on a flex container, so the fields render
    // flush with nothing to show that spacing was ever intended.
    let clean = strip_comments(&css);
    for (selector, body) in rules(&clean) {
        assert!(
            !(body.contains("display: flex") && body.contains("gap")),
            "{selector} spaces a flex container with `gap`: {body}"
        );
    }
    for gutter in ["p.pair > span", "p.filter > * + *"] {
        let body = rules(&clean)
            .into_iter()
            .find(|(selector, _)| selector == gutter)
            .unwrap_or_else(|| panic!("no {gutter} rule"))
            .1;
        assert!(
            body.contains("margin-left"),
            "{gutter} has no gutter: {body}"
        );
    }

    // A later, more specific rule must not quietly reset the gutter it sits
    // inside. `p.filter input` outranks `p.filter > * + *`, so a blanket
    // `margin: 0` there would pull the box flush against its label.
    for (selector, body) in rules(&clean) {
        if selector.starts_with("p.filter ") || selector.starts_with("p.pair ") {
            assert!(
                !body.contains("margin: 0"),
                "{selector} resets the gutter set for it: {body}"
            );
        }
    }

    // Rules must target something the markup actually carries.
    let (_, _, page) = fetch("/collections/era5/position", "text/html").await;
    for (selector, _) in rules(&clean) {
        if let Some(attribute) = selector
            .strip_prefix("p.filter [")
            .and_then(|rest| rest.strip_suffix("]"))
        {
            assert!(
                page.contains(attribute),
                "{selector} matches nothing in the markup"
            );
        }

        // Content in a table must not be able to overflow its column. water.css
        // sets `table-layout: fixed`, which gives every column an equal share
        // whatever it holds, so anything unwrappable spills into its neighbour —
        // which is what a 63-character parameter name did to the description
        // beside it.
        let table_rule = rules(&clean)
            .into_iter()
            .find(|(selector, _)| selector == ".table-scroll table")
            .expect("no .table-scroll table rule")
            .1;
        assert!(
            table_rule.contains("table-layout: auto"),
            "tables inherit `table-layout: fixed` from water.css: {table_rule}"
        );
        for (selector, body) in rules(&clean) {
            if selector.starts_with("table#parameters") {
                assert!(
                    !body.contains("white-space: nowrap"),
                    "{selector} stops long names wrapping, so they overflow: {body}"
                );
            }
        }
    }

    // Every custom property the overrides use must be one water.css defines.
    for property in ["--border", "--background-alt", "--text-muted"] {
        assert!(
            css.contains(&format!("{property}:")),
            "{property} is never defined"
        );
    }
}

#[tokio::test]
async fn every_metadata_resource_has_an_html_representation() {
    for path in ["/", "/conformance", "/collections", "/collections/era5"] {
        let (status, content_type, body) = fetch(path, "text/html").await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(
            content_type.starts_with("text/html"),
            "{path}: {content_type}"
        );
        assert!(body.starts_with("<!DOCTYPE html>"), "{path} is not a page");

        // And the JSON representation is still one request away.
        let (_, content_type, body) = fetch(path, "*/*").await;
        assert_eq!(content_type, "application/json", "{path}");
        assert!(body.starts_with('{'), "{path}");
    }
}

#[tokio::test]
async fn every_json_metadata_resource_links_to_its_html_twin() {
    for path in ["/", "/conformance", "/collections", "/collections/era5"] {
        let (_, body) = get(path).await;
        let alternate = body["links"]
            .as_array()
            .unwrap_or_else(|| panic!("{path} has no links"))
            .iter()
            .find(|l| l["rel"] == "alternate")
            .unwrap_or_else(|| panic!("{path} has no alternate link"));
        assert_eq!(alternate["type"], "text/html", "{path}");
    }
}

#[tokio::test]
async fn the_conformance_page_lists_the_classes_it_declares() {
    let (_, _, body) = fetch("/conformance", "text/html").await;
    let (_, json) = get("/conformance").await;

    for class in json["conformsTo"].as_array().unwrap() {
        let class = class.as_str().unwrap();
        assert!(body.contains(class), "page omits {class}");
    }
    // What is not implemented is stated, not silently missing.
    assert!(body.contains("trajectory") && body.contains("corridor"));
}

#[tokio::test]
async fn the_collections_page_links_into_each_collection() {
    let (_, _, body) = fetch("/collections", "text/html").await;
    assert!(body.contains("href=\"/collections/era5\""));
    assert!(body.contains("ERA5"));
    // Breadcrumbs place the page.
    assert!(body.contains("Breadcrumb"), "no breadcrumb nav");
    assert!(body.contains("aria-current=\"page\""), "no current crumb");
}

#[tokio::test]
async fn the_collection_page_shows_extents_and_query_endpoints() {
    let (_, _, body) = fetch("/collections/era5", "text/html").await;

    // Extents, matching the JSON representation.
    assert!(body.contains("2024-01-01T00:00:00Z"), "no temporal extent");
    assert!(
        body.contains("2024-01-01T03:00:00Z"),
        "no temporal extent end"
    );
    assert!(
        body.contains("-180") && body.contains("180"),
        "no spatial extent"
    );
    // The vertical axis and its units.
    assert!(body.contains("Hectopascal(hPa)"), "no vertical units");
    for level in ["500", "850", "1000"] {
        assert!(body.contains(level), "level {level} missing");
    }

    // Each query type is linked.
    for query in ["position", "radius", "area", "cube"] {
        assert!(
            body.contains(&format!("href=\"/collections/era5/{query}\"")),
            "no link to {query}"
        );
    }
}

#[tokio::test]
async fn the_collection_page_tabulates_every_parameter() {
    let (_, _, body) = fetch("/collections/era5", "text/html").await;
    let (_, json) = get("/collections/era5").await;
    let parameters = json["parameter_names"].as_object().unwrap();

    for (name, parameter) in parameters {
        assert!(
            body.contains(name),
            "parameter {name} missing from the table"
        );
        let label = parameter["label"]["en"].as_str().unwrap();
        assert!(body.contains(label), "label of {name} missing");
    }
    // Units come from the store, and a variable without them says so.
    assert!(body.contains("<td>K</td>"), "units column not rendered");

    // Surface and pressure-level variables are distinguished, because it
    // decides whether `z` applies.
    assert!(
        body.contains("varies with level"),
        "vertical variables not marked"
    );
    assert!(body.contains("surface"), "surface variables not marked");

    // The filter needs a lowercased search key on every row.
    let rows = body.matches("data-search=").count();
    assert_eq!(rows, parameters.len(), "every row needs a search key");
    assert!(
        body.contains(r#"data-filter="parameters""#),
        "no filter control"
    );

    // The scroll box lives on a wrapper: `display: block` on the table itself
    // would stop the header cells lining up with the body.
    assert!(
        body.contains(r#"class="table-scroll catalogue""#),
        "table is not in a scroll box"
    );
}

#[tokio::test]
async fn pages_are_self_contained_and_carry_the_request_host() {
    let (_, _, body) = fetch("/collections/era5", "text/html").await;
    // No third-party origins: the stylesheet and script ship with the binary.
    assert!(!body.contains("//cdn."), "depends on a CDN");
    assert!(!body.contains("<script src"), "loads an external script");
    // The example command is runnable as printed.
    assert!(
        body.contains("http://edr.example:3000/collections/era5/position"),
        "example command is not absolute"
    );
}

#[tokio::test]
async fn a_bare_query_url_shows_the_form_rather_than_an_error() {
    for query in ["position", "radius", "area", "cube"] {
        let (status, content_type, body) =
            fetch(&format!("/collections/era5/{query}"), "text/html").await;

        assert_eq!(status, StatusCode::OK, "{query}");
        assert!(content_type.starts_with("text/html"), "{query}");
        assert!(body.contains("<form"), "{query} has no form");
        // The form submits to itself, staying on the HTML representation.
        assert!(
            body.contains(&format!("action=\"/collections/era5/{query}\"")),
            "{query} form does not post to itself"
        );
        assert!(body.contains("name=\"f\" value=\"html\""), "{query}");
        assert!(
            !body.contains("role=\"alert\""),
            "{query} shows an error too soon"
        );

        // Every parameter is offered as a checkbox, grouped by whether `z`
        // applies, with a filter over them.
        assert!(
            body.contains(r#"type="checkbox""#) && body.contains(r#"name="parameter-name""#),
            "{query} does not offer checkboxes"
        );
        assert!(
            body.contains(r#"data-filter="parameters""#),
            "{query} has no parameter filter"
        );
        assert!(body.contains("2m_temperature"), "{query}");
    }

    // The same URL without an HTML Accept is still an error, as the API says.
    let (status, _) = get("/collections/era5/position").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn each_query_type_asks_for_its_own_geometry() {
    let field = |body: &str, name: &str| body.contains(&format!("name=\"{name}\""));

    let (_, _, cube) = fetch("/collections/era5/cube", "text/html").await;
    assert!(field(&cube, "bbox") && !field(&cube, "coords"));
    assert!(field(&cube, "resolution-x") && field(&cube, "resolution-y"));

    let (_, _, radius) = fetch("/collections/era5/radius", "text/html").await;
    assert!(field(&radius, "coords") && field(&radius, "within"));
    assert!(radius.contains("name=\"within-units\""));

    let (_, _, position) = fetch("/collections/era5/position", "text/html").await;
    assert!(field(&position, "coords") && !field(&position, "bbox"));
    assert!(!field(&position, "within"), "position has no radius");
    assert!(
        !field(&position, "resolution-x"),
        "position has no resolution"
    );

    // The vertical field appears because this collection has a level axis.
    assert!(field(&position, "z"));

    // Time is two pickers, not a hand-typed timestamp.
    assert!(field(&position, "datetime-from") && field(&position, "datetime-to"));
    assert!(
        !field(&position, "datetime"),
        "the raw datetime field is gone"
    );
}

#[tokio::test]
async fn running_a_query_renders_a_table_of_values() {
    let (status, _, body) = fetch(
        "/collections/era5/position?coords=POINT(0%2045)&parameter-name=2m_temperature\
         &datetime=2024-01-01T00:00:00Z/2024-01-01T03:00:00Z&f=html",
        "*/*",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert!(body.contains("Result"), "no result section");
    // Time varies, so it earns a column; the fixed position is stated once.
    assert!(body.contains("<th>Time</th>"), "no time column");
    assert!(
        !body.contains("<th>Latitude</th>"),
        "a fixed axis became a column"
    );
    assert!(body.contains("45, 0"), "position not stated");

    // Every timestep and its value.
    for t in 0..fixture::TIMES {
        let expected = format!("{:.4}", fixture::surface_value(t, 1, 0));
        assert!(
            body.contains(&expected),
            "value for step {t} missing: {expected}"
        );
    }

    // The submitted values come back in the form.
    assert!(
        body.contains("value=\"POINT(0 45)\""),
        "coords not preserved"
    );
    assert!(
        body.contains(r#"value="2m_temperature" checked"#),
        "parameter not re-ticked"
    );

    // And the same result is one link away as data.
    assert!(body.contains("f=CoverageJSON"), "no CoverageJSON link");
    assert!(body.contains("f=GeoJSON"), "no GeoJSON link");
}

#[tokio::test]
async fn a_grid_result_gives_every_varying_axis_a_column() {
    let (_, _, body) = fetch(
        "/collections/era5/cube?bbox=-50,-50,50,50&parameter-name=2m_temperature\
         &datetime=2024-01-01T01:00:00Z&f=html",
        "*/*",
    )
    .await;

    assert!(body.contains("<th>Latitude</th>") && body.contains("<th>Longitude</th>"));
    // A single timestep is stated above the table, not repeated down it.
    assert!(!body.contains("<th>Time</th>"));
    assert!(body.contains("2024-01-01T01:00:00Z"));
}

#[tokio::test]
async fn a_vertical_result_labels_the_level_column_with_the_axis_name() {
    let (_, _, body) = fetch(
        "/collections/era5/position?coords=POINT(0%2045)&parameter-name=temperature\
         &z=500,1000&datetime=2024-01-01T00:00:00Z&f=html",
        "*/*",
    )
    .await;

    assert!(
        body.contains("<th>level</th>"),
        "level column not named for the axis"
    );
    for level in ["500", "1000"] {
        assert!(body.contains(level), "level {level} missing");
    }
}

#[tokio::test]
async fn a_rejected_query_re_renders_the_form_with_the_reason() {
    let (status, content_type, body) = fetch(
        "/collections/era5/position?coords=POINT(0%2045)&parameter-name=nope&f=html",
        "*/*",
    )
    .await;

    // The page is the form again, carrying the explanation — not a JSON error.
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/html"));
    assert!(body.contains("role=\"alert\""), "no error shown");
    assert!(
        body.contains("is not a parameter of collection"),
        "the reason is not explained: {body}"
    );
    assert!(body.contains("<form"), "the form is gone");
    // What was typed survives, so it can be corrected rather than retyped.
    assert!(body.contains("value=\"POINT(0 45)\""));

    // Asking for the same thing as data still gets the status code.
    let (status, json) =
        get("/collections/era5/position?coords=POINT(0%2045)&parameter-name=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["code"], 400);
}

#[tokio::test]
async fn parameter_rows_link_into_the_query_form() {
    let (_, _, body) = fetch("/collections/era5", "text/html").await;
    // `&` is escaped in an attribute value, as HTML requires; a browser
    // resolves it back to a plain separator.
    assert!(
        body.contains("/collections/era5/position?parameter-name=2m_temperature&amp;f=html"),
        "parameter rows do not link into the form"
    );

    // And that link lands on a form with the parameter already chosen.
    let (status, _, form) = fetch(
        "/collections/era5/position?parameter-name=2m_temperature&f=html",
        "*/*",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(form.contains(r#"value="2m_temperature" checked"#));
    assert!(
        !form.contains("role=\"alert\""),
        "a prefilled form is not an error"
    );
}

#[tokio::test]
async fn html_is_advertised_as_an_output_format() {
    let (_, collection) = get("/collections/era5").await;
    let formats: Vec<&str> = collection["output_formats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(formats.contains(&"HTML"), "{formats:?}");
}

#[tokio::test]
async fn a_small_limit_caps_the_answer_without_restricting_the_read() {
    // `limit` says how much to return, not how much the server may read. A
    // request whose scan is affordable must not be refused for asking for less
    // of it — and the reason given back must be about the answer's size.
    let (status, body) = get("/collections/era5/cube?bbox=-180,-90,180,90\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z&limit=5")
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let description = body["description"].as_str().unwrap();
    assert!(
        description.contains("selects") && !description.contains("too spread out"),
        "a small limit must not read as a scan refusal: {description}"
    );

    // The whole grid at one step is within both ceilings once `limit` allows it.
    let (status, body) = get("/collections/era5/cube?bbox=-180,-90,180,90\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z&limit=100000")
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_region_across_the_meridian_is_not_refused_for_its_widened_read() {
    // The fixture's longitude axis wraps, so this box takes cells from both
    // ends and is read as the range spanning them. That widening is the
    // server's business, not a reason to reject a modest request.
    let (status, body) = get("/collections/era5/cube?bbox=-50,-50,50,50\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z/2024-01-01T03:00:00Z")
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["ranges"]["2m_temperature"]["shape"][0], 4,
        "four timesteps"
    );
}

#[tokio::test]
async fn a_multi_select_submits_repeated_keys_and_every_one_counts() {
    // An HTML <select multiple> sends one `parameter-name=` per chosen option,
    // where EDR spells the same request `parameter-name=a,b`. Both must mean
    // the same thing — silently keeping only the last would return a subset of
    // what was asked for, with nothing to say so.
    let repeated = ok("/collections/era5/position?coords=POINT(0%2045)\
         &parameter-name=2m_temperature&parameter-name=total_precipitation\
         &datetime=2024-01-01T00:00:00Z")
    .await;
    let comma_separated = ok("/collections/era5/position?coords=POINT(0%2045)\
         &parameter-name=2m_temperature,total_precipitation\
         &datetime=2024-01-01T00:00:00Z")
    .await;

    assert_eq!(repeated["ranges"], comma_separated["ranges"]);
    let returned = repeated["ranges"].as_object().unwrap();
    assert_eq!(
        returned.len(),
        2,
        "both parameters must come back: {returned:?}"
    );
    assert!(returned.contains_key("2m_temperature"));
    assert!(returned.contains_key("total_precipitation"));
}

#[tokio::test]
async fn a_submitted_form_ticks_back_every_parameter_it_ran() {
    let (_, _, body) = fetch(
        "/collections/era5/position?f=html&coords=POINT(0%2045)\
         &parameter-name=2m_temperature&parameter-name=total_precipitation\
         &datetime=&z=",
        "*/*",
    )
    .await;

    for name in ["2m_temperature", "total_precipitation"] {
        assert!(
            body.contains(&format!(r#"value="{name}" checked"#)),
            "{name} is not re-ticked in the form"
        );
        assert!(
            body.contains(&format!("<th>{name}</th>")),
            "{name} has no column"
        );
    }
}

#[tokio::test]
async fn blank_fields_from_a_submitted_form_read_as_absent() {
    // A browser submits every field, so untouched ones arrive empty. They must
    // fall back to their defaults rather than being parsed as given values.
    let (status, _, body) = fetch(
        "/collections/era5/position?f=html&coords=POINT(0%2045)\
         &parameter-name=2m_temperature&datetime=&z=",
        "*/*",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains("role=\"alert\""),
        "empty fields were treated as input"
    );
    // Empty `datetime` means the latest step.
    assert!(body.contains("2024-01-01T03:00:00Z"));
}

#[tokio::test]
async fn the_stylesheet_url_changes_whenever_the_stylesheet_does() {
    // The stylesheet is served `immutable` for a year, so its URL must be a
    // function of its bytes. Keyed on anything that changes less often — the
    // crate version, say — an edit would stay invisible behind the cache.
    use ogc_edr::api::html::{STYLESHEET, STYLESHEET_PATH};

    let path = STYLESHEET_PATH.as_str();
    assert!(path.starts_with("/static/style."), "{path}");
    assert!(path.ends_with(".css"), "{path}");

    let fingerprint = path
        .trim_start_matches("/static/style.")
        .trim_end_matches(".css");
    assert_eq!(fingerprint.len(), 16, "not a full-width hash: {path}");
    assert!(
        fingerprint.chars().all(|c| c.is_ascii_hexdigit()),
        "not a hash: {path}"
    );
    // It is the content that is fingerprinted, not the version.
    assert!(
        !path.contains(env!("CARGO_PKG_VERSION")),
        "the URL is keyed on the crate version: {path}"
    );

    // And the page asks for exactly that URL.
    let (_, _, body) = fetch("/", "text/html").await;
    assert!(
        body.contains(&format!("href=\"{path}\"")),
        "the page does not link the fingerprinted stylesheet"
    );

    // Sanity: the served bytes are the ones that were fingerprinted.
    let response = app()
        .await
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let served = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(served.len(), STYLESHEET.len());
}

/// Strip CSS comments, so a property named in prose is not read as a rule.
fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start..].find("*/") {
            Some(end) => rest = &rest[start + end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Split a stylesheet into (selector, declarations) pairs.
fn rules(css: &str) -> Vec<(String, String)> {
    css.split('}')
        .filter_map(|block| block.split_once('{'))
        .map(|(selector, body)| {
            (
                selector
                    .rsplit(['{', '}'])
                    .next()
                    .unwrap_or(selector)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                body.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect()
}

#[tokio::test]
async fn browsing_and_choosing_show_the_same_parameter_table() {
    // Both pages present the same facts about the same parameters, so they are
    // one component. Choosing adds a checkbox column; nothing else may drift.
    let (_, _, browse) = fetch("/collections/era5", "text/html").await;
    let (_, _, choose) = fetch("/collections/era5/position", "text/html").await;

    let table = |page: &str| {
        let start = page.find(r#"<table id="parameters">"#).expect("no table");
        let end = page[start..].find("</table>").expect("unterminated") + start;
        page[start..end].to_string()
    };
    let (browse_table, choose_table) = (table(&browse), table(&choose));

    // The same columns, in the same order, plus the checkbox column.
    let headers = |t: &str| {
        regex_free_headers(t)
            .into_iter()
            .filter(|h| !h.is_empty())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        headers(&browse_table),
        ["Name", "Description", "Units", "Vertical"]
    );
    assert_eq!(
        headers(&choose_table),
        ["Chosen", "Name", "Description", "Units", "Vertical"]
    );

    // The same rows, carrying the same search keys.
    let keys = |t: &str| {
        t.match_indices("data-search=\"")
            .map(|(i, _)| {
                let rest = &t[i + 13..];
                rest[..rest.find('"').unwrap()].to_string()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        keys(&browse_table),
        keys(&choose_table),
        "the rows have diverged"
    );
    assert!(!keys(&browse_table).is_empty());

    // Both are filtered by the same control, wired to the same table.
    for page in [&browse, &choose] {
        assert!(page.contains(r#"data-filter="parameters""#));
        assert!(page.contains(r#"data-filter-count="parameters""#));
        assert!(page.contains(r#"class="table-scroll catalogue""#));
    }

    // Only the choosing page has checkboxes; only the browsing page links out.
    assert!(choose_table.contains(r#"type="checkbox""#));
    assert!(!browse_table.contains(r#"type="checkbox""#));
    assert!(browse_table.contains("/position?parameter-name="));
    assert!(!choose_table.contains("/position?parameter-name="));
}

#[tokio::test]
async fn the_filter_can_narrow_to_surface_or_vertical_variables() {
    // The checkbox list used to group parameters under Surface and Pressure
    // level headings. The unified table states it in a column instead, so the
    // search key has to carry it or that way of narrowing would be lost.
    for path in ["/collections/era5", "/collections/era5/position"] {
        let (_, _, body) = fetch(path, "text/html").await;
        assert!(
            body.contains(r#"data-search="2m_temperature 2 metre temperature surface""#),
            "{path}: a surface variable is not searchable as one"
        );
        assert!(
            body.contains("varies with level"),
            "{path}: vertical variables are not marked"
        );
    }
}

/// Header cell text, in order, without pulling in a regex dependency.
fn regex_free_headers(table: &str) -> Vec<String> {
    let head = match (table.find("<thead>"), table.find("</thead>")) {
        (Some(start), Some(end)) => &table[start..end],
        _ => return Vec::new(),
    };
    head.split("<th")
        .skip(1)
        .map(|cell| {
            let inner = cell.split_once('>').map(|(_, rest)| rest).unwrap_or("");
            let text = inner.split("</th>").next().unwrap_or("");
            // Drop any nested markup, such as the visually hidden label.
            let mut out = String::new();
            let mut depth = 0;
            for c in text.chars() {
                match c {
                    '<' => depth += 1,
                    '>' => depth -= 1,
                    _ if depth == 0 => out.push(c),
                    _ => {}
                }
            }
            out.trim().to_string()
        })
        .collect()
}

#[tokio::test]
async fn example_commands_come_with_a_copy_button() {
    for path in ["/", "/collections/era5"] {
        let (_, _, body) = fetch(path, "text/html").await;

        // The button names the element it copies, and that element exists.
        let id = body
            .split_once(r#"data-copy=""#)
            .unwrap_or_else(|| panic!("{path} has no copy button"))
            .1
            .split_once('"')
            .unwrap()
            .0
            .to_string();
        assert!(
            body.contains(&format!(r#"<code id="{id}">"#)),
            "{path}: the copy button points at nothing"
        );

        // Hidden until the script reveals it, so a browser without one is not
        // shown a button that cannot work.
        assert!(
            body.contains(r#"<button class="copy" type="button" hidden"#),
            "{path}: the copy button is not hidden for no-script"
        );

        // What is copied is the element's own text, so it cannot drift from
        // what is displayed. Check the displayed command is the real thing.
        let command = body
            .split_once(&format!(r#"<code id="{id}">"#))
            .unwrap()
            .1
            .split_once("</code>")
            .unwrap()
            .0;
        // Apostrophes need no escaping in element text, so the command reads
        // exactly as it will run.
        assert!(
            command.starts_with("curl -G 'http://edr.example:3000/"),
            "{command}"
        );
        for part in ["coords=POINT", "parameter-name=", "datetime="] {
            assert!(command.contains(part), "{path}: command lacks {part}");
        }
        // A command that is one line in the markup is one line on the clipboard.
        assert_eq!(command.matches('\n').count(), 3, "{path}: {command}");
    }
}

#[tokio::test]
async fn the_copy_script_ships_with_the_page() {
    let (_, _, body) = fetch("/", "text/html").await;
    assert!(!body.contains("<script src"), "loads an external script");
    // Both behaviours travel in the one inline script the shell emits.
    assert_eq!(body.matches("<script>").count(), 1);
    assert!(body.contains("data-copy"), "no copy binding");
    assert!(body.contains("navigator.clipboard"), "no clipboard call");
    // The clipboard API needs a secure context; a fallback must exist for
    // plain HTTP.
    assert!(
        body.contains("execCommand"),
        "no fallback for insecure contexts"
    );
}

#[tokio::test]
async fn the_time_pickers_are_bounded_by_the_collections_own_axis() {
    let (_, _, body) = fetch("/collections/era5/position", "text/html").await;

    // Both fields are real date-and-time pickers.
    assert_eq!(body.matches(r#"type="datetime-local""#).count(), 2);

    // Bounded by the extent, so an instant outside the data cannot be picked.
    let (_, collection) = get("/collections/era5").await;
    let interval = &collection["extent"]["temporal"]["interval"][0];
    let (start, end) = (interval[0].as_str().unwrap(), interval[1].as_str().unwrap());
    // The picker format is minute precision with no zone marker.
    let as_picker = |rfc3339: &str| rfc3339[..16].to_string();
    assert!(
        body.contains(&format!(r#"min="{}""#, as_picker(start))),
        "no lower bound from the time axis"
    );
    assert!(
        body.contains(&format!(r#"max="{}""#, as_picker(end))),
        "no upper bound from the time axis"
    );

    // And stepped by the store's own cadence, so an instant between two
    // timesteps cannot be chosen. The fixture is hourly.
    assert!(
        body.contains(r#"step="3600""#),
        "picker is not stepped to the axis"
    );
}

#[tokio::test]
async fn the_pickers_drive_a_query_and_come_back_filled_in() {
    let (status, _, body) = fetch(
        "/collections/era5/position?f=html&coords=POINT(0%2045)\
         &parameter-name=2m_temperature\
         &datetime-from=2024-01-01T00:00&datetime-to=2024-01-01T02:00",
        "*/*",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Three hourly steps came back. Scoped to the result table: the form also
    // states the collection's full extent, which runs past this interval.
    // From the Result heading onwards: the parameter table comes first on the
    // page and has a `<tbody>` of its own.
    let table = body.split_once("<h2>Result</h2>").expect("no result").1;
    for hour in ["T00:00:00Z", "T01:00:00Z", "T02:00:00Z"] {
        assert!(table.contains(hour), "missing step {hour}");
    }
    assert!(
        !table.contains("T03:00:00Z"),
        "returned a step beyond the interval"
    );

    // The pickers hold what was asked for.
    assert!(
        body.contains(r#"value="2024-01-01T00:00""#),
        "'from' not preserved"
    );
    assert!(
        body.contains(r#"value="2024-01-01T02:00""#),
        "'to' not preserved"
    );

    // Links out use the specification's spelling, not the form's.
    assert!(
        body.contains("datetime=2024-01-01T00:00/2024-01-01T02:00"),
        "the data links do not canonicalise the pickers"
    );
    assert!(
        !body.contains("datetime-from=2024-01-01T00:00&amp;f=Coverage"),
        "a data link leaks the form's field names"
    );
}

#[tokio::test]
async fn arriving_with_an_api_datetime_fills_the_same_pickers() {
    // A URL copied from a curl example, or a shared link, uses `datetime`.
    // Opening it as a page must fill the two boxes rather than lose the value.
    let (_, _, body) = fetch(
        "/collections/era5/position?f=html&coords=POINT(0%2045)\
         &parameter-name=2m_temperature\
         &datetime=2024-01-01T00:00:00Z/2024-01-01T02:00:00Z",
        "*/*",
    )
    .await;
    assert!(
        body.contains(r#"value="2024-01-01T00:00""#),
        "'from' not filled"
    );
    assert!(
        body.contains(r#"value="2024-01-01T02:00""#),
        "'to' not filled"
    );

    // An open-ended interval leaves that box empty.
    let (_, _, body) = fetch(
        "/collections/era5/position?f=html&coords=POINT(0%2045)\
         &parameter-name=2m_temperature&datetime=2024-01-01T00:00:00Z/..",
        "*/*",
    )
    .await;
    assert!(
        body.contains(r#"value="2024-01-01T00:00""#),
        "'from' not filled"
    );
    assert!(
        body.contains(r#"name="datetime-to" value="""#),
        "'to' should be empty"
    );
}
