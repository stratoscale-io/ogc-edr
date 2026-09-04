//! Contract tests for the assumptions this server makes about zarr-datafusion.
//!
//! The EDR layer relies on specific behaviour from the scan: which projections
//! are legal, and which predicate forms return correctly-labelled coordinates.
//! These tests pin that down against a fixture whose every value is known, so a
//! change in the backend shows up here rather than as wrong numbers in a
//! coverage.

mod fixture;

use std::sync::Arc;

use arrow::array::{Array, Float32Array, Float64Array};
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::SessionContext;
use zarr_datafusion::datasource::factory::ZarrTableFactory;

async fn context() -> SessionContext {
    let root = std::env::temp_dir().join(format!("ogc-edr-backend-{}", std::process::id()));
    if !root.join(".zmetadata").exists() {
        fixture::write_store(&root).expect("write fixture");
    }
    let state = SessionStateBuilder::new()
        .with_default_features()
        .with_table_factory("ZARR".into(), Arc::new(ZarrTableFactory) as _)
        .build();
    let ctx = SessionContext::new_with_state(state);
    ctx.sql(&format!(
        "CREATE EXTERNAL TABLE era5 STORED AS ZARR LOCATION '{}'",
        root.display()
    ))
    .await
    .unwrap()
    .collect()
    .await
    .unwrap();
    ctx
}

/// Every `(latitude, longitude, value)` a query returns.
async fn cells(ctx: &SessionContext, sql: &str) -> Vec<(f64, f64, f64)> {
    let batches = ctx
        .sql(sql)
        .await
        .expect("plan")
        .collect()
        .await
        .expect("execute");
    let mut out = Vec::new();
    for batch in batches {
        let read = |name: &str| {
            let col = batch
                .column_by_name(name)
                .unwrap_or_else(|| panic!("no column {name}"));
            let cast = arrow::compute::cast(col, &arrow::datatypes::DataType::Float64).unwrap();
            cast.as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .clone()
        };
        let (lat, lon, val) = (read("latitude"), read("longitude"), read("2m_temperature"));
        for i in 0..batch.num_rows() {
            out.push((lat.value(i), lon.value(i), val.value(i)));
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    out
}

/// What the fixture says the value at a cell is.
fn expected(t: usize, lat: f64, lon: f64) -> f64 {
    let y = fixture::LATS
        .iter()
        .position(|v| *v == lat)
        .expect("latitude on the axis");
    let x = fixture::LONS
        .iter()
        .position(|v| *v == lon)
        .expect("longitude on the axis");
    fixture::surface_value(t, y, x) as f64
}

#[tokio::test]
async fn a_contiguous_range_scan_labels_every_cell_correctly() {
    // This is the only predicate shape the server emits for coordinates, so
    // this test guards the correctness of every data response.
    let ctx = context().await;
    let rows = cells(
        &ctx,
        "SELECT time, latitude, longitude, \"2m_temperature\" FROM era5 \
         WHERE latitude BETWEEN -45 AND 45 AND longitude BETWEEN 0 AND 90 \
           AND time = '2024-01-01T01:00:00'",
    )
    .await;

    assert_eq!(rows.len(), 9, "3 latitudes x 3 longitudes");
    for (lat, lon, value) in rows {
        assert_eq!(
            value as f32,
            expected(1, lat, lon) as f32,
            "at ({lat}, {lon})"
        );
    }
}

#[tokio::test]
async fn an_equality_scan_labels_its_cell_correctly() {
    let ctx = context().await;
    let rows = cells(
        &ctx,
        "SELECT time, latitude, longitude, \"2m_temperature\" FROM era5 \
         WHERE latitude = 45 AND longitude = 315 AND time = '2024-01-01T02:00:00'",
    )
    .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2 as f32, expected(2, 45.0, 315.0) as f32);
}

#[tokio::test]
async fn a_surface_variable_must_not_name_the_vertical_coordinate() {
    // Projecting a `(time, latitude, longitude)` variable while naming `level`
    // makes the scan declare a column it then omits from the batch, so the
    // server never mentions the vertical axis in a surface query.
    let ctx = context().await;
    let with_level = ctx
        .sql(
            "SELECT time, level, latitude, longitude, \"2m_temperature\" FROM era5 \
             WHERE latitude = 45 AND longitude = 0 AND time = '2024-01-01T00:00:00'",
        )
        .await
        .expect("plans")
        .collect()
        .await;
    assert!(
        with_level.is_err(),
        "naming `level` beside a surface variable is expected to fail; if this now \
         succeeds the workaround in build_sql can be simplified"
    );

    // Without it, the same selection reads cleanly.
    let without_level = cells(
        &ctx,
        "SELECT time, latitude, longitude, \"2m_temperature\" FROM era5 \
         WHERE latitude = 45 AND longitude = 0 AND time = '2024-01-01T00:00:00'",
    )
    .await;
    assert_eq!(without_level.len(), 1);
    assert_eq!(without_level[0].2 as f32, expected(0, 45.0, 0.0) as f32);
}

#[tokio::test]
async fn variables_of_different_rank_cannot_share_a_scan() {
    // Surface and pressure-level variables are queried separately because the
    // backend rejects a projection that mixes them.
    let ctx = context().await;
    let mixed = ctx
        .sql(
            "SELECT time, level, latitude, longitude, \"2m_temperature\", temperature \
             FROM era5 WHERE latitude = 45 AND longitude = 0 AND time = '2024-01-01T00:00:00'",
        )
        .await;
    let failed = match mixed {
        Err(_) => true,
        Ok(df) => df.collect().await.is_err(),
    };
    assert!(
        failed,
        "mixing ranks in one scan is expected to fail; if this now succeeds the \
         two-query split in execute() can be simplified"
    );
}

#[tokio::test]
async fn a_coordinate_only_projection_with_a_limit_yields_the_axis() {
    // How the catalog reads axis values at startup: this must return the
    // coordinate array itself, not the Cartesian product of every coordinate.
    let ctx = context().await;
    let batches = ctx
        .sql("SELECT latitude FROM era5 LIMIT 5")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let mut values = Vec::new();
    for batch in &batches {
        let cast = arrow::compute::cast(
            batch.column_by_name("latitude").unwrap(),
            &arrow::datatypes::DataType::Float32,
        )
        .unwrap();
        let arr = cast.as_any().downcast_ref::<Float32Array>().unwrap();
        values.extend((0..arr.len()).map(|i| arr.value(i) as f64));
    }
    assert_eq!(values, fixture::LATS.to_vec());
}
