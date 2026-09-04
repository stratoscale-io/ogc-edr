//! Store introspection: what collections exist, what they contain, and the
//! DataFusion session that queries them.
//!
//! A collection is a single Zarr store registered as one DataFusion table. At
//! startup we learn three things about it:
//!
//! 1. the Arrow schema and Zarr metadata, from zarr-datafusion's table factory;
//! 2. the coordinate axis values, by selecting each coordinate column on its own
//!    (zarr-datafusion answers a coordinate-only projection with `LIMIT` from
//!    the coordinate arrays alone, without expanding the Cartesian product);
//! 3. the CF attributes (`units`, `long_name`, …), read straight from the
//!    store's consolidated metadata — DataFusion's schema does not carry them,
//!    and EDR needs them for `parameter_names`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use arrow::array::{Array as ArrowArray, Float64Array, TimestampMicrosecondArray};
use arrow::datatypes::{DataType, TimeUnit};
use arrow::record_batch::RecordBatch;
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::SessionContext;
use serde_json::Value;
use zarr_datafusion::datasource::factory::ZarrTableFactory;
use zarr_datafusion::datasource::zarr::ZarrTable;
use zarr_datafusion::optimizer::{
    CountStatisticsRule, MinMaxStatisticsRule, ZarrLimitPushdownRule,
};
use zarrs::storage::{AsyncReadableStorageTraits, StoreKey};

use crate::axis::{NumericAxis, TimeAxis};
use crate::config::CollectionConfig;
use crate::error::{EdrError, EdrResult};

/// One queryable parameter — a Zarr data variable.
#[derive(Debug, Clone)]
pub struct Parameter {
    /// Zarr array name; also the EDR parameter name and the SQL column.
    pub name: String,
    pub label: String,
    pub description: Option<String>,
    /// CF `units`. `None` means the store did not record any, in which case the
    /// EDR response says so rather than inventing a unit.
    pub unit: Option<String>,
    pub standard_name: Option<String>,
    /// Whether this variable spans the vertical coordinate. Surface and
    /// pressure-level variables cannot be read in one scan, so this is what
    /// splits a request into separate queries.
    pub uses_z: bool,
    /// CoverageJSON range data type: "float" or "integer".
    pub cov_data_type: &'static str,
}

/// A collection: one Zarr store, its axes, and its parameters.
#[derive(Debug)]
pub struct Collection {
    pub id: String,
    pub title: String,
    pub description: String,
    /// Name the store is registered under in DataFusion.
    pub table: String,
    pub keywords: Vec<String>,

    pub x_name: String,
    pub y_name: String,
    pub z_name: Option<String>,
    pub t_name: String,

    pub x: NumericAxis,
    pub y: NumericAxis,
    pub z: Option<NumericAxis>,
    pub t: TimeAxis,

    /// True when the longitude axis runs 0…360 (ERA5) rather than -180…180, so
    /// CRS84 input has to be folded before it can be matched against the axis.
    pub lon_0_360: bool,
    /// True when the longitude axis wraps the globe, so its two ends are
    /// neighbours rather than the extremes of a bounded range.
    pub lon_global: bool,
    /// Vertical axis units, e.g. "Hectopascal(hPa)".
    pub z_units: Option<String>,

    /// Time range the provider declares as actually populated, from the store's
    /// root attributes. ERA5's time axis is pre-allocated out to 2050, so the
    /// axis alone would advertise decades of empty grids.
    pub valid_time: Option<(i64, i64)>,

    pub parameters: BTreeMap<String, Parameter>,
}

impl Collection {
    pub fn parameter(&self, name: &str) -> Option<&Parameter> {
        self.parameters.get(name)
    }

    /// Fold a CRS84 longitude (-180…180) onto the store's own convention.
    pub fn to_native_lon(&self, lon: f64) -> f64 {
        if self.lon_0_360 {
            lon.rem_euclid(360.0)
        } else {
            lon
        }
    }

    /// Present a stored longitude as CRS84.
    pub fn to_crs84_lon(&self, native: f64) -> f64 {
        if self.lon_0_360 && native > 180.0 {
            native - 360.0
        } else {
            native
        }
    }

    /// Index of the longitude cell nearest a CRS84 longitude.
    ///
    /// On a wrapping axis the distance is measured around the circle: a point
    /// just west of the prime meridian is nearer to the first cell of a 0…360
    /// axis than to its last, which a straight search on the stored values
    /// would get backwards.
    pub fn nearest_x(&self, lon: f64) -> usize {
        let native = self.to_native_lon(lon);
        let linear = self.x.nearest_index(native);
        if !self.lon_global {
            return linear;
        }
        let circular_distance = |i: usize| {
            let d = (self.x.values[i] - native).abs();
            d.min(360.0 - d)
        };
        [linear, 0, self.x.len() - 1]
            .into_iter()
            .min_by(|a, b| circular_distance(*a).total_cmp(&circular_distance(*b)))
            .expect("three candidates")
    }

    /// Spatial extent in CRS84 order: `[west, south, east, north]`.
    pub fn bbox(&self) -> [f64; 4] {
        // A wrapping axis covers the whole world; report it as such rather than
        // as 0…359.75, which would read as a partial extent.
        if self.lon_global {
            return [-180.0, self.y.min(), 180.0, self.y.max()];
        }
        [
            self.to_crs84_lon(self.x.min()),
            self.y.min(),
            self.to_crs84_lon(self.x.max()),
            self.y.max(),
        ]
    }

    /// Temporal extent in epoch microseconds: the declared validity window,
    /// clipped to the axis, falling back to the axis itself.
    pub fn time_extent(&self) -> (i64, i64) {
        let axis = (
            *self.t.values.first().unwrap_or(&0),
            *self.t.values.last().unwrap_or(&0),
        );
        match self.valid_time {
            Some((lo, hi)) => (lo.max(axis.0), hi.min(axis.1)),
            None => axis,
        }
    }

    /// Parameters split into the two groups that must be queried separately:
    /// those spanning the vertical axis and those that do not.
    pub fn split_by_vertical<'a>(&self, names: &'a [String]) -> (Vec<&'a String>, Vec<&'a String>) {
        names
            .iter()
            .partition(|n| self.parameter(n).is_some_and(|p| p.uses_z))
    }
}

/// The registered collections and the session that serves them.
pub struct Catalog {
    pub ctx: SessionContext,
    pub collections: BTreeMap<String, Arc<Collection>>,
}

impl Catalog {
    pub fn collection(&self, id: &str) -> EdrResult<&Arc<Collection>> {
        self.collections
            .get(id)
            .ok_or_else(|| EdrError::NotFound(format!("No collection with id '{id}'")))
    }

    /// Build a session with zarr-datafusion's table factory and optimizer
    /// rules, then load every configured collection.
    pub async fn open(configs: &[CollectionConfig]) -> EdrResult<Self> {
        let state = SessionStateBuilder::new()
            .with_default_features()
            .with_table_factory("ZARR".to_string(), Arc::new(ZarrTableFactory) as _)
            .with_optimizer_rule(Arc::new(CountStatisticsRule::new()))
            .with_optimizer_rule(Arc::new(MinMaxStatisticsRule::new()))
            .with_physical_optimizer_rule(Arc::new(ZarrLimitPushdownRule::new()))
            .build();
        let ctx = SessionContext::new_with_state(state);

        let mut collections = BTreeMap::new();
        for cfg in configs {
            let collection = load_collection(&ctx, cfg).await?;
            collections.insert(collection.id.clone(), Arc::new(collection));
        }
        Ok(Self { ctx, collections })
    }
}

/// Register one store and learn everything the API needs about it.
async fn load_collection(ctx: &SessionContext, cfg: &CollectionConfig) -> EdrResult<Collection> {
    let table = cfg.id.replace('-', "_");
    tracing::info!(id = %cfg.id, location = %cfg.location, "registering collection");

    ctx.sql(&format!(
        "CREATE EXTERNAL TABLE {} STORED AS ZARR LOCATION '{}'",
        quote_ident(&table),
        cfg.location.replace('\'', "''")
    ))
    .await
    .map_err(|e| EdrError::Internal(format!("Could not register '{}': {e}", cfg.location)))?
    .collect()
    .await
    .map_err(|e| EdrError::Internal(format!("Could not register '{}': {e}", cfg.location)))?;

    let provider = ctx
        .table_provider(table.as_str())
        .await
        .map_err(|e| EdrError::Internal(e.to_string()))?;
    let schema = provider.schema();
    let zarr_table = provider.downcast_ref::<ZarrTable>().ok_or_else(|| {
        EdrError::Internal(format!("'{}' is not backed by a Zarr store", cfg.location))
    })?;
    let store_meta = zarr_table
        .store_meta()
        .ok_or_else(|| EdrError::Internal(format!("No Zarr metadata for '{}'", cfg.location)))?;

    let attrs = StoreAttrs::load(
        &cfg.location,
        store_meta.coords.iter().map(|c| c.name.as_str()),
    )
    .await
    .unwrap_or_else(|e| {
        // Attributes are enrichment, not correctness: without them the API
        // still answers, it just cannot name units.
        tracing::warn!(location = %cfg.location, error = %e, "no store attributes; \
                 parameter units will be reported as unknown");
        StoreAttrs::default()
    });

    // Classify coordinates into the EDR axes.
    let mut x_name = None;
    let mut y_name = None;
    let mut z_name = None;
    let mut t_name = None;
    for coord in &store_meta.coords {
        let name = coord.name.as_str();
        let units = attrs.units(name).unwrap_or_default().to_ascii_lowercase();
        let is_time = coord.cf_time_attrs.is_some()
            || matches!(
                schema
                    .field_with_name(name)
                    .map(|f| unwrap_dict(f.data_type())),
                Ok(DataType::Timestamp(_, _))
            );
        if is_time && t_name.is_none() {
            t_name = Some(name.to_string());
        } else if (units.starts_with("degrees_north") || matches!(name, "latitude" | "lat" | "y"))
            && y_name.is_none()
        {
            y_name = Some(name.to_string());
        } else if (units.starts_with("degrees_east") || matches!(name, "longitude" | "lon" | "x"))
            && x_name.is_none()
        {
            x_name = Some(name.to_string());
        } else if z_name.is_none() {
            z_name = Some(name.to_string());
        }
    }
    let (x_name, y_name, t_name) = match (x_name, y_name, t_name) {
        (Some(x), Some(y), Some(t)) => (x, y, t),
        _ => {
            return Err(EdrError::Internal(format!(
                "'{}' has no recognisable x/y/time coordinates (found: {})",
                cfg.location,
                store_meta
                    .coords
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    };

    // Read the axis values. Each is a separate scan, so run them together.
    let sizes: HashMap<&str, usize> = store_meta
        .coords
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.shape.first().copied().unwrap_or(0) as usize,
            )
        })
        .collect();
    let axis_names: Vec<String> = [Some(&x_name), Some(&y_name), z_name.as_ref()]
        .into_iter()
        .flatten()
        .cloned()
        .collect();

    let numeric = futures::future::try_join_all(axis_names.iter().map(|name| {
        let limit = sizes.get(name.as_str()).copied().unwrap_or(0);
        read_numeric_axis(ctx, &table, name, limit)
    }));
    let temporal = read_time_axis(
        ctx,
        &table,
        &t_name,
        sizes.get(t_name.as_str()).copied().unwrap_or(0),
    );
    let (numeric, t) = futures::try_join!(numeric, temporal)?;

    let mut numeric = numeric.into_iter();
    let x = numeric.next().expect("x axis requested");
    let y = numeric.next().expect("y axis requested");
    let z = numeric.next();

    let lon_0_360 = x.max() > 180.0;
    // The axis wraps when its span plus one cell closes the circle.
    let lon_global = x
        .step()
        .map(|step| (x.max() - x.min() + step.abs() - 360.0).abs() < 1e-6)
        .unwrap_or(false);
    tracing::info!(
        id = %cfg.id, x = x.len(), y = y.len(),
        z = z.as_ref().map(|a| a.len()).unwrap_or(0), t = t.len(),
        lon_0_360, lon_global, "axes loaded"
    );

    // Parameters: every data variable, enriched from the store attributes.
    let mut parameters = BTreeMap::new();
    for var in &store_meta.data_vars {
        if cfg
            .parameters
            .as_ref()
            .is_some_and(|only| !only.iter().any(|p| p == &var.name))
        {
            continue;
        }
        let dims = var.dimensions.as_deref().unwrap_or_default();
        // Prefer the recorded dimension names; fall back to rank for a store
        // that does not carry `_ARRAY_DIMENSIONS`.
        let uses_z = z_name.as_ref().is_some_and(|zn| {
            if dims.is_empty() {
                var.shape.len() == store_meta.coords.len()
            } else {
                dims.iter().any(|d| d == zn)
            }
        });
        let cov_data_type = match schema.field_with_name(&var.name).map(|f| f.data_type()) {
            Ok(DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64) => "integer",
            _ => "float",
        };
        parameters.insert(
            var.name.clone(),
            Parameter {
                label: attrs
                    .string(&var.name, "long_name")
                    .unwrap_or(&var.name)
                    .to_string(),
                description: attrs.string(&var.name, "comment").map(str::to_string),
                unit: attrs.units(&var.name).map(str::to_string),
                standard_name: attrs.string(&var.name, "standard_name").map(str::to_string),
                name: var.name.clone(),
                uses_z,
                cov_data_type,
            },
        );
    }
    if parameters.is_empty() {
        return Err(EdrError::Internal(format!(
            "'{}' exposes no data variables",
            cfg.location
        )));
    }

    let valid_time = attrs.validity_window();
    tracing::info!(id = %cfg.id, parameters = parameters.len(), "collection ready");

    Ok(Collection {
        title: cfg.title.clone().unwrap_or_else(|| cfg.id.to_uppercase()),
        description: cfg
            .description
            .clone()
            .unwrap_or_else(|| format!("Zarr store at {}", cfg.location)),
        id: cfg.id.clone(),
        table,
        keywords: cfg.keywords.clone(),
        z_units: z_name
            .as_deref()
            .and_then(|n| attrs.units(n))
            .map(str::to_string),
        x_name,
        y_name,
        z_name,
        t_name,
        x,
        y,
        z,
        t,
        lon_0_360,
        lon_global,
        valid_time,
        parameters,
    })
}

/// Read one numeric coordinate axis.
///
/// `SELECT <coord> FROM <table> LIMIT <axis length>` is the projection
/// zarr-datafusion special-cases: a coordinate-only projection with a limit is
/// answered from the coordinate array without materialising the Cartesian
/// product of every coordinate.
async fn read_numeric_axis(
    ctx: &SessionContext,
    table: &str,
    name: &str,
    limit: usize,
) -> EdrResult<NumericAxis> {
    let batches = run(
        ctx,
        &format!(
            "SELECT {} FROM {} LIMIT {}",
            quote_ident(name),
            quote_ident(table),
            limit.max(1)
        ),
    )
    .await?;
    let values = batches_to_f64(&batches, name)?;
    if values.is_empty() {
        return Err(EdrError::Internal(format!("Axis '{name}' read back empty")));
    }
    Ok(NumericAxis::new(values))
}

async fn read_time_axis(
    ctx: &SessionContext,
    table: &str,
    name: &str,
    limit: usize,
) -> EdrResult<TimeAxis> {
    let batches = run(
        ctx,
        &format!(
            "SELECT {} FROM {} LIMIT {}",
            quote_ident(name),
            quote_ident(table),
            limit.max(1)
        ),
    )
    .await?;
    let values = batches_to_time_micros(&batches, name)?;
    if values.is_empty() {
        return Err(EdrError::Internal(format!(
            "Time axis '{name}' read back empty"
        )));
    }
    Ok(TimeAxis::new(values))
}

pub async fn run(ctx: &SessionContext, sql: &str) -> EdrResult<Vec<RecordBatch>> {
    tracing::debug!(%sql, "executing");
    ctx.sql(sql)
        .await
        .map_err(|e| EdrError::Internal(format!("Query planning failed: {e}")))?
        .collect()
        .await
        .map_err(|e| EdrError::Internal(format!("Query execution failed: {e}")))
}

/// Coordinate columns arrive dictionary-encoded; casting normalises both the
/// dictionary and the underlying width (ERA5 latitudes are `f32`).
pub fn batches_to_f64(batches: &[RecordBatch], column: &str) -> EdrResult<Vec<f64>> {
    let mut out = Vec::new();
    for batch in batches {
        let col = batch
            .column_by_name(column)
            .ok_or_else(|| EdrError::Internal(format!("Column '{column}' missing from result")))?;
        let cast = arrow::compute::cast(col, &DataType::Float64)
            .map_err(|e| EdrError::Internal(format!("Cannot read '{column}' as a number: {e}")))?;
        let arr = cast
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("cast to Float64");
        out.extend((0..arr.len()).map(|i| arr.value(i)));
    }
    Ok(out)
}

pub fn batches_to_time_micros(batches: &[RecordBatch], column: &str) -> EdrResult<Vec<i64>> {
    let target = DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()));
    let mut out = Vec::new();
    for batch in batches {
        let col = batch
            .column_by_name(column)
            .ok_or_else(|| EdrError::Internal(format!("Column '{column}' missing from result")))?;
        let cast = arrow::compute::cast(col, &target)
            .map_err(|e| EdrError::Internal(format!("Cannot read '{column}' as a time: {e}")))?;
        let arr = cast
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .expect("cast to timestamp");
        out.extend((0..arr.len()).map(|i| arr.value(i)));
    }
    Ok(out)
}

fn unwrap_dict(dt: &DataType) -> DataType {
    match dt {
        DataType::Dictionary(_, value) => (**value).clone(),
        other => other.clone(),
    }
}

/// Quote a SQL identifier. ERA5 variable names start with digits
/// (`2m_temperature`), so nothing may go unquoted.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// CF attributes read directly from the Zarr store.
#[derive(Debug, Default)]
struct StoreAttrs {
    root: serde_json::Map<String, Value>,
    arrays: HashMap<String, Value>,
}

impl StoreAttrs {
    fn string(&self, array: &str, key: &str) -> Option<&str> {
        self.arrays.get(array)?.get(key)?.as_str()
    }

    fn units(&self, array: &str) -> Option<&str> {
        self.string(array, "units")
    }

    /// ARCO-ERA5 records the populated span in the root attributes; the time
    /// axis itself is pre-allocated well past it.
    ///
    /// Both ends are dates. The start is the first instant of its day and the
    /// stop is the last: a store valid through 2026-04-30 holds that day's
    /// 23:00 step, so reading the stop as midnight would hide 23 hours of data.
    fn validity_window(&self) -> Option<(i64, i64)> {
        let date = |key: &str| -> Option<chrono::NaiveDate> {
            chrono::NaiveDate::parse_from_str(self.root.get(key)?.as_str()?, "%Y-%m-%d").ok()
        };
        let parse = |key: &str| -> Option<i64> {
            Some(
                date(key)?
                    .and_hms_opt(0, 0, 0)?
                    .and_utc()
                    .timestamp_micros(),
            )
        };
        let parse_end = |key: &str| -> Option<i64> {
            Some(
                date(key)?
                    .and_hms_micro_opt(23, 59, 59, 999_999)?
                    .and_utc()
                    .timestamp_micros(),
            )
        };
        let start = parse("valid_time_start")?;
        // The plain stop is the fully-validated end; `_era5t` extends into the
        // preliminary ERA5T window. Take the later of the two that parse.
        let stop = parse_end("valid_time_stop")
            .into_iter()
            .chain(parse_end("valid_time_stop_era5t"))
            .max()?;
        Some((start, stop))
    }

    /// Load attributes, preferring Zarr v2 consolidated metadata and falling
    /// back to per-array metadata documents.
    async fn load<'a>(
        location: &str,
        coords: impl Iterator<Item = &'a str>,
    ) -> Result<Self, String> {
        let store = RawStore::open(location).await?;
        let coords: Vec<String> = coords.map(str::to_string).collect();

        if let Some(bytes) = store.get(".zmetadata").await {
            let text = sanitize_json(&String::from_utf8_lossy(&bytes));
            let doc: Value =
                serde_json::from_str(&text).map_err(|e| format!("Malformed .zmetadata: {e}"))?;
            let meta = doc
                .get("metadata")
                .and_then(Value::as_object)
                .ok_or("'.zmetadata' has no 'metadata' object")?;
            let mut arrays = HashMap::new();
            for (key, value) in meta {
                if let Some(name) = key.strip_suffix("/.zattrs") {
                    arrays.insert(name.to_string(), value.clone());
                }
            }
            return Ok(Self {
                root: meta
                    .get(".zattrs")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default(),
                arrays,
            });
        }

        // Unconsolidated store: fetch the root document plus each coordinate's.
        // Data-variable attributes are skipped here — a store can hold hundreds
        // of variables and the units are enrichment, not correctness.
        let mut arrays = HashMap::new();
        for name in &coords {
            if let Some(v) = store.attributes(name).await {
                arrays.insert(name.clone(), v);
            }
        }
        let root = store
            .attributes("")
            .await
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        Ok(Self { root, arrays })
    }
}

/// Byte-level access to a Zarr store, local or remote.
enum RawStore {
    Local(std::path::PathBuf),
    Remote {
        store: zarrs::storage::AsyncReadableListableStorage,
        prefix: String,
    },
}

impl RawStore {
    async fn open(location: &str) -> Result<Self, String> {
        if zarr_datafusion::reader::storage::is_remote_url(location) {
            let (store, prefix) = zarr_datafusion::reader::storage::create_async_store(location)
                .await
                .map_err(|e| format!("Cannot open '{location}': {e}"))?;
            Ok(RawStore::Remote {
                store,
                prefix: prefix.as_ref().trim_matches('/').to_string(),
            })
        } else {
            Ok(RawStore::Local(std::path::PathBuf::from(location)))
        }
    }

    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        match self {
            RawStore::Local(root) => std::fs::read(root.join(key)).ok(),
            RawStore::Remote { store, prefix } => {
                let full = if prefix.is_empty() {
                    key.to_string()
                } else {
                    format!("{prefix}/{key}")
                };
                let store_key = StoreKey::new(&full).ok()?;
                store.get(&store_key).await.ok()?.map(|b| b.to_vec())
            }
        }
    }

    /// Attributes for one array (or the group root when `name` is empty),
    /// handling both the v2 `.zattrs` file and the v3 `zarr.json` envelope.
    async fn attributes(&self, name: &str) -> Option<Value> {
        let join = |file: &str| {
            if name.is_empty() {
                file.to_string()
            } else {
                format!("{name}/{file}")
            }
        };
        if let Some(bytes) = self.get(&join(".zattrs")).await {
            return serde_json::from_str(&sanitize_json(&String::from_utf8_lossy(&bytes))).ok();
        }
        let bytes = self.get(&join("zarr.json")).await?;
        let doc: Value =
            serde_json::from_str(&sanitize_json(&String::from_utf8_lossy(&bytes))).ok()?;
        doc.get("attributes").cloned()
    }
}

/// Zarr metadata legitimately contains `NaN` and `Infinity` (ERA5 records a
/// `GRIB_missingValue` of `Infinity`), which `serde_json` rejects.
fn sanitize_json(text: &str) -> String {
    text.replace("NaN", "null")
        .replace("-Infinity", "null")
        .replace("Infinity", "null")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_quoted_for_names_that_start_with_a_digit() {
        assert_eq!(quote_ident("2m_temperature"), "\"2m_temperature\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn json_sanitiser_replaces_non_standard_literals() {
        let raw = r#"{"a": NaN, "b": Infinity, "c": -Infinity, "d": 1}"#;
        let v: Value = serde_json::from_str(&sanitize_json(raw)).unwrap();
        assert!(v["a"].is_null() && v["b"].is_null() && v["c"].is_null());
        assert_eq!(v["d"], 1);
    }

    #[test]
    fn validity_window_prefers_the_later_stop() {
        let root: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{"valid_time_start":"1940-01-01","valid_time_stop":"2026-04-30",
                "valid_time_stop_era5t":"2026-08-26"}"#,
        )
        .unwrap();
        let attrs = StoreAttrs {
            root,
            arrays: HashMap::new(),
        };
        let (start, stop) = attrs.validity_window().unwrap();
        assert_eq!(
            chrono::DateTime::from_timestamp_micros(start)
                .unwrap()
                .to_rfc3339(),
            "1940-01-01T00:00:00+00:00"
        );
        // The stop date is inclusive: its last instant, not its midnight.
        assert_eq!(
            chrono::DateTime::from_timestamp_micros(stop)
                .unwrap()
                .to_rfc3339(),
            "2026-08-26T23:59:59.999999+00:00"
        );
    }
}
