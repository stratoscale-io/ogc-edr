//! Builds a tiny ERA5-shaped Zarr v2 store on disk.
//!
//! The fixture mirrors the parts of ARCO-ERA5 that the server has to cope with:
//! longitudes on 0…360, latitudes descending from north, a vertical `level`
//! axis, CF-encoded time, a root attribute declaring a validity window narrower
//! than the time axis, and both surface `(time, latitude, longitude)` and
//! pressure-level `(time, level, latitude, longitude)` variables.

use std::path::Path;

use serde_json::{Map, Value, json};

pub const TIMES: usize = 4;
pub const LEVELS: [f64; 3] = [500.0, 850.0, 1000.0];
/// Descending from the north pole, the order ERA5 stores latitude in.
pub const LATS: [f64; 5] = [90.0, 45.0, 0.0, -45.0, -90.0];
/// A complete 0…360 axis at 45° spacing, so it wraps the globe exactly as
/// ERA5's 1440-cell axis does — the two ends are neighbours across the prime
/// meridian.
pub const LONS: [f64; 8] = [0.0, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0];

/// Surface value at a grid position — distinct for every cell so a misplaced
/// value is visible in an assertion.
pub fn surface_value(t: usize, y: usize, x: usize) -> f32 {
    280.0 + t as f32 * 10.0 + y as f32 + x as f32 * 0.01
}

/// Pressure-level value at a grid position.
pub fn level_value(t: usize, z: usize, y: usize, x: usize) -> f32 {
    200.0 + t as f32 * 10.0 + z as f32 * 100.0 + y as f32 + x as f32 * 0.01
}

/// Write the store and return its path.
pub fn write_store(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let mut consolidated = Map::new();

    let root_attrs = json!({
        // Deliberately narrower than the time axis: the collection's temporal
        // extent must follow this, not the axis.
        "valid_time_start": "2024-01-01",
        "valid_time_stop": "2024-01-01",
    });
    std::fs::write(root.join(".zgroup"), br#"{"zarr_format":2}"#)?;
    std::fs::write(root.join(".zattrs"), serde_json::to_vec(&root_attrs)?)?;
    consolidated.insert(".zgroup".into(), json!({ "zarr_format": 2 }));
    consolidated.insert(".zattrs".into(), root_attrs);

    // Coordinates. Time is hours since the CF epoch, four hourly steps on
    // 2024-01-01; the two dates either side of the validity window are not on
    // the axis, so the extent has to come from the root attributes.
    // 2024-01-01T00:00:00Z is 45_290 days after 1900-01-01.
    let epoch_hours = 45_290_i64 * 24;
    let times: Vec<i64> = (0..TIMES).map(|i| epoch_hours + i as i64).collect();
    array_i64(
        root,
        &mut consolidated,
        "time",
        &times,
        json!({
            "_ARRAY_DIMENSIONS": ["time"],
            "units": "hours since 1900-01-01 00:00:00",
            "calendar": "proleptic_gregorian",
        }),
    )?;
    array_i64(
        root,
        &mut consolidated,
        "level",
        &LEVELS.iter().map(|v| *v as i64).collect::<Vec<_>>(),
        json!({ "_ARRAY_DIMENSIONS": ["level"], "units": "Hectopascal(hPa)", "long_name": "level" }),
    )?;
    array_f32(
        root,
        &mut consolidated,
        "latitude",
        &[LATS.len()],
        &LATS.iter().map(|v| *v as f32).collect::<Vec<_>>(),
        json!({ "_ARRAY_DIMENSIONS": ["latitude"], "units": "degrees_north", "long_name": "latitude" }),
    )?;
    array_f32(
        root,
        &mut consolidated,
        "longitude",
        &[LONS.len()],
        &LONS.iter().map(|v| *v as f32).collect::<Vec<_>>(),
        json!({ "_ARRAY_DIMENSIONS": ["longitude"], "units": "degrees_east", "long_name": "longitude" }),
    )?;

    // Surface variables, (time, latitude, longitude).
    let mut surface = Vec::new();
    let mut precip = Vec::new();
    for t in 0..TIMES {
        for y in 0..LATS.len() {
            for x in 0..LONS.len() {
                surface.push(surface_value(t, y, x));
                precip.push(t as f32 * 0.001);
            }
        }
    }
    let surface_dims = json!(["time", "latitude", "longitude"]);
    array_f32(
        root,
        &mut consolidated,
        "2m_temperature",
        &[TIMES, LATS.len(), LONS.len()],
        &surface,
        json!({
            "_ARRAY_DIMENSIONS": surface_dims,
            "units": "K",
            "long_name": "2 metre temperature",
            "short_name": "t2m",
        }),
    )?;
    array_f32(
        root,
        &mut consolidated,
        "total_precipitation",
        &[TIMES, LATS.len(), LONS.len()],
        &precip,
        json!({
            "_ARRAY_DIMENSIONS": surface_dims,
            "units": "m",
            "long_name": "Total precipitation",
        }),
    )?;

    // A pressure-level variable, (time, level, latitude, longitude).
    let mut levelled = Vec::new();
    for t in 0..TIMES {
        for z in 0..LEVELS.len() {
            for y in 0..LATS.len() {
                for x in 0..LONS.len() {
                    levelled.push(level_value(t, z, y, x));
                }
            }
        }
    }
    array_f32(
        root,
        &mut consolidated,
        "temperature",
        &[TIMES, LEVELS.len(), LATS.len(), LONS.len()],
        &levelled,
        json!({
            "_ARRAY_DIMENSIONS": ["time", "level", "latitude", "longitude"],
            "units": "K",
            "long_name": "Temperature",
            "standard_name": "air_temperature",
        }),
    )?;

    std::fs::write(
        root.join(".zmetadata"),
        serde_json::to_vec(&json!({
            "zarr_consolidated_format": 1,
            "metadata": Value::Object(consolidated),
        }))?,
    )?;
    Ok(())
}

fn array_f32(
    root: &Path,
    consolidated: &mut Map<String, Value>,
    name: &str,
    shape: &[usize],
    values: &[f32],
    attrs: Value,
) -> std::io::Result<()> {
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    write_array(root, consolidated, name, shape, "<f4", &bytes, attrs)
}

fn array_i64(
    root: &Path,
    consolidated: &mut Map<String, Value>,
    name: &str,
    values: &[i64],
    attrs: Value,
) -> std::io::Result<()> {
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    write_array(
        root,
        consolidated,
        name,
        &[values.len()],
        "<i8",
        &bytes,
        attrs,
    )
}

/// One uncompressed, single-chunk Zarr v2 array.
fn write_array(
    root: &Path,
    consolidated: &mut Map<String, Value>,
    name: &str,
    shape: &[usize],
    dtype: &str,
    bytes: &[u8],
    attrs: Value,
) -> std::io::Result<()> {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir)?;

    let zarray = json!({
        "chunks": shape,
        "compressor": Value::Null,
        "dtype": dtype,
        "fill_value": Value::Null,
        "filters": Value::Null,
        "order": "C",
        "shape": shape,
        "zarr_format": 2,
    });
    std::fs::write(dir.join(".zarray"), serde_json::to_vec(&zarray)?)?;
    std::fs::write(dir.join(".zattrs"), serde_json::to_vec(&attrs)?)?;
    // A single chunk: "0", "0.0.0", …
    let chunk_key = vec!["0"; shape.len()].join(".");
    std::fs::write(dir.join(chunk_key), bytes)?;

    consolidated.insert(format!("{name}/.zarray"), zarray);
    consolidated.insert(format!("{name}/.zattrs"), attrs);
    Ok(())
}
