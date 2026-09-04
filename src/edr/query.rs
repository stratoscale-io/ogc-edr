//! Turning an EDR data query into Zarr reads.
//!
//! Every data query resolves to a set of indices on each coordinate axis. Those
//! indices become IN-lists and BETWEEN predicates that zarr-datafusion pushes
//! down to chunk selection, and the rows that come back are scattered into a
//! dense array indexed by the same axes.
//!
//! Two ERA5 facts shape this module:
//!
//! * Longitudes are stored 0…360 while EDR speaks CRS84, so a request that
//!   straddles the prime meridian selects indices from both ends of the axis.
//! * Surface variables are `(time, latitude, longitude)` and pressure-level
//!   variables are `(time, level, latitude, longitude)`. A single scan cannot
//!   mix the two, so a request spanning both runs one query per group and the
//!   results are merged.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::axis::{NumericAxis, TimeAxis};
use crate::catalog::{Collection, batches_to_f64, batches_to_time_micros, quote_ident, run};
use crate::edr::params::{Query, TemporalSelection, VerticalSelection, haversine_metres};
use crate::edr::wkt::{Coord, Geometry};
use crate::error::{EdrError, EdrResult};
use datafusion::prelude::SessionContext;

/// Longitude indices, latitude indices, and the cell mask for a selection that
/// is not a full rectangle.
type SpatialSelection = (Vec<usize>, Vec<usize>, Option<HashSet<(usize, usize)>>);

/// Which EDR data query produced this request; it decides the shape of the
/// response as much as the selection does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    Position,
    Radius,
    Area,
    Cube,
}

impl QueryKind {
    pub fn path(self) -> &'static str {
        match self {
            QueryKind::Position => "position",
            QueryKind::Radius => "radius",
            QueryKind::Area => "area",
            QueryKind::Cube => "cube",
        }
    }
}

/// The grid cells, levels and times a request resolves to.
#[derive(Debug, Clone)]
pub struct Selection {
    /// Indices into the collection's longitude axis, in output order.
    pub x_idx: Vec<usize>,
    pub y_idx: Vec<usize>,
    /// Indices into the vertical axis; empty when the collection has none.
    pub z_idx: Vec<usize>,
    pub t_idx: Vec<usize>,
    /// `(y, x)` index pairs to keep when the selection is not a full rectangle
    /// — a polygon, a radius, or scattered points. The SQL still selects the
    /// bounding rectangle, because a chunk covering one cell of an ERA5 field
    /// covers the whole globe at that timestep anyway.
    pub mask: Option<HashSet<(usize, usize)>>,
}

impl Selection {
    /// Number of data values one parameter would occupy.
    pub fn value_count(&self) -> usize {
        let cells = match &self.mask {
            Some(mask) => mask.len(),
            None => self.x_idx.len() * self.y_idx.len(),
        };
        cells * self.t_idx.len() * self.z_idx.len().max(1)
    }

    /// Rows the widened scan will materialise, which for a scattered selection
    /// exceeds [`Self::value_count`].
    pub fn scan_row_count(&self) -> usize {
        hull_len(&self.x_idx)
            .saturating_mul(hull_len(&self.y_idx))
            .saturating_mul(hull_len(&self.z_idx).max(1))
            .saturating_mul(self.t_idx.len())
    }

    fn is_empty(&self) -> bool {
        self.x_idx.is_empty()
            || self.y_idx.is_empty()
            || self.t_idx.is_empty()
            || self.mask.as_ref().is_some_and(HashSet::is_empty)
    }
}

/// A fully validated request, ready to run.
#[derive(Debug)]
pub struct DataRequest {
    pub kind: QueryKind,
    pub collection: std::sync::Arc<Collection>,
    pub parameters: Vec<String>,
    pub selection: Selection,
}

/// The two ceilings a request is held to. They answer different questions and
/// must not be conflated: one protects the server from an expensive read, the
/// other caps how much the caller gets back.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Most grid rows the scan may materialise. A server-side resource guard,
    /// never lowered by the caller.
    pub max_scan_rows: usize,
    /// `limit` to apply when the request does not carry one.
    pub default_limit: usize,
}

/// Resolve the geometry, `datetime`, `z` and `parameter-name` of a request
/// against a collection's axes.
pub fn resolve(
    collection: &std::sync::Arc<Collection>,
    kind: QueryKind,
    geometry: &Geometry,
    query: &Query,
    limits: Limits,
) -> EdrResult<DataRequest> {
    query.crs()?;
    let parameters = resolve_parameters(collection, query)?;
    let t_idx = resolve_time(collection, query.datetime()?)?;
    let z_idx = resolve_vertical(collection, query.vertical()?, &parameters)?;
    let (x_idx, y_idx, mask) = resolve_spatial(collection, kind, geometry, query)?;

    let mut selection = Selection {
        x_idx,
        y_idx,
        z_idx,
        t_idx,
        mask,
    };
    // Present the axes in ascending coordinate order. Longitude matters most:
    // a box across the prime meridian selects indices from both ends of a
    // 0…360 axis, which in storage order would hand a client a Grid whose x
    // axis jumps from 0.5 to -0.5.
    selection.x_idx.sort_by(|a, b| {
        let key = |i: &usize| collection.to_crs84_lon(collection.x.values[*i]);
        key(a).total_cmp(&key(b))
    });
    selection
        .y_idx
        .sort_by(|a, b| collection.y.values[*a].total_cmp(&collection.y.values[*b]));
    selection.z_idx.sort_by(|a, b| {
        let axis = collection.z.as_ref().expect("z indices imply a z axis");
        axis.values[*a].total_cmp(&axis.values[*b])
    });

    thin_to_resolution(collection, &mut selection, query)?;

    if selection.is_empty() {
        return Err(EdrError::NotFound(
            "The query selects no data: nothing in this collection matches that \
             location, time and vertical level"
                .into(),
        ));
    }

    // Widening a scattered selection to a contiguous range (see
    // `spatial_predicates`) means the scan can be much larger than the
    // response. Two points on opposite sides of the world select two cells but
    // span the grid between them, so the read is bounded as well as the result.
    // This ceiling is the server's own: lowering `limit` asks for a smaller
    // answer, which is no reason to refuse a read that was already affordable.
    let scanned = selection.scan_row_count();
    if scanned > limits.max_scan_rows {
        return Err(EdrError::TooLarge(format!(
            "The query is too spread out: reading it needs {scanned} grid rows, above this \
             server's ceiling of {}. Positions far apart are read as the range spanning \
             them, so request them separately, or narrow 'datetime'.",
            limits.max_scan_rows
        )));
    }

    let limit = query.limit(limits.default_limit)?.min(limits.max_scan_rows);
    let total = selection.value_count() * parameters.len();
    if total > limit {
        return Err(EdrError::TooLarge(format!(
            "The query selects {total} values ({} cells x {} levels x {} times x {} parameters), \
             above the limit of {limit}. Narrow 'datetime', the area, 'z', or 'parameter-name', \
             or thin the grid with 'resolution-x' and 'resolution-y'.",
            selection
                .mask
                .as_ref()
                .map_or(selection.x_idx.len() * selection.y_idx.len(), HashSet::len),
            selection.z_idx.len().max(1),
            selection.t_idx.len(),
            parameters.len()
        )));
    }

    Ok(DataRequest {
        kind,
        collection: collection.clone(),
        parameters,
        selection,
    })
}

fn resolve_parameters(collection: &Collection, query: &Query) -> EdrResult<Vec<String>> {
    match query.parameter_names() {
        None => {
            // Without `parameter-name` there is no sensible default across 273
            // ERA5 variables: returning all of them would read the whole store.
            Err(EdrError::BadRequest(format!(
                "'parameter-name' is required for this collection; it has {} parameters. \
                 See /collections/{}/ for the list.",
                collection.parameters.len(),
                collection.id
            )))
        }
        Some(names) => {
            let mut resolved = Vec::new();
            for name in names {
                if collection.parameter(&name).is_none() {
                    return Err(EdrError::BadRequest(format!(
                        "'{name}' is not a parameter of collection '{}'",
                        collection.id
                    )));
                }
                if !resolved.contains(&name) {
                    resolved.push(name);
                }
            }
            if resolved.is_empty() {
                return Err(EdrError::BadRequest("'parameter-name' is empty".into()));
            }
            Ok(resolved)
        }
    }
}

/// Map a `datetime` selection onto time-axis indices, clipped to the span the
/// store declares as populated.
fn resolve_time(
    collection: &Collection,
    datetime: Option<TemporalSelection>,
) -> EdrResult<Vec<usize>> {
    let (valid_lo, valid_hi) = collection.time_extent();
    let axis = &collection.t;

    let indices: Vec<usize> = match datetime {
        // No `datetime`: EDR returns the latest available time step.
        None => {
            let idx = axis.nearest_index(valid_hi);
            vec![idx]
        }
        Some(TemporalSelection::Instant(t)) => {
            if t < valid_lo || t > valid_hi {
                return Err(EdrError::NotFound(format!(
                    "{} is outside the collection's temporal extent ({} to {})",
                    format_time(t),
                    format_time(valid_lo),
                    format_time(valid_hi)
                )));
            }
            vec![axis.nearest_index(t)]
        }
        Some(TemporalSelection::Interval(lo, hi)) => {
            let lo = lo.unwrap_or(valid_lo).max(valid_lo);
            let hi = hi.unwrap_or(valid_hi).min(valid_hi);
            if lo > hi {
                return Err(EdrError::NotFound(format!(
                    "The requested interval does not overlap the collection's temporal \
                     extent ({} to {})",
                    format_time(valid_lo),
                    format_time(valid_hi)
                )));
            }
            axis.indices_in_range(lo, hi).collect()
        }
    };
    Ok(indices)
}

/// Map a `z` selection onto vertical-axis indices.
fn resolve_vertical(
    collection: &Collection,
    vertical: Option<VerticalSelection>,
    parameters: &[String],
) -> EdrResult<Vec<usize>> {
    let Some(axis) = collection.z.as_ref() else {
        if vertical.is_some() {
            return Err(EdrError::BadRequest(
                "This collection has no vertical axis, so 'z' cannot be used".into(),
            ));
        }
        return Ok(Vec::new());
    };

    // Only parameters that actually span the vertical axis need levels.
    let any_vertical = parameters
        .iter()
        .any(|p| collection.parameter(p).is_some_and(|p| p.uses_z));
    if !any_vertical {
        if vertical.is_some() {
            return Err(EdrError::BadRequest(format!(
                "None of the requested parameters vary with '{}', so 'z' does not apply",
                collection.z_name.as_deref().unwrap_or("z")
            )));
        }
        return Ok(Vec::new());
    }

    let indices = match vertical {
        // Omitted `z` means every level. For ERA5 that is free: one chunk holds
        // all 37 levels of a timestep, so a narrower selection reads no less.
        None | Some(VerticalSelection::All) => (0..axis.len()).collect(),
        Some(VerticalSelection::Values(values)) => {
            let mut indices = Vec::new();
            for v in values {
                let idx = axis.nearest_index(v);
                if (axis.values[idx] - v).abs() > 1e-6 {
                    return Err(EdrError::BadRequest(format!(
                        "z={v} is not a level of this collection; available levels are {}",
                        summarise(&axis.values)
                    )));
                }
                if !indices.contains(&idx) {
                    indices.push(idx);
                }
            }
            indices
        }
        Some(VerticalSelection::Interval(lo, hi)) => {
            let indices = axis.indices_in_range(lo, hi);
            if indices.is_empty() {
                return Err(EdrError::NotFound(format!(
                    "No level lies between {lo} and {hi}; available levels are {}",
                    summarise(&axis.values)
                )));
            }
            indices
        }
    };
    Ok(indices)
}

/// Resolve the geometry to longitude/latitude indices plus an optional cell
/// mask for the non-rectangular query types.
fn resolve_spatial(
    collection: &Collection,
    kind: QueryKind,
    geometry: &Geometry,
    query: &Query,
) -> EdrResult<SpatialSelection> {
    match kind {
        QueryKind::Position => {
            // Snap each requested position to its nearest grid cell.
            let points = match geometry {
                Geometry::Point(c) => vec![*c],
                Geometry::MultiPoint(cs) => cs.clone(),
                // A LINESTRING at `position` is read as its vertices.
                Geometry::LineString(cs) => cs.clone(),
                Geometry::Polygon(_) => {
                    return Err(EdrError::BadRequest(
                        "The position query takes POINT or MULTIPOINT coords; use /area for a \
                         POLYGON"
                            .into(),
                    ));
                }
            };
            let cells: Vec<(usize, usize)> = points
                .iter()
                .map(|p| nearest_cell(collection, *p))
                .collect();
            Ok(from_cells(cells))
        }
        QueryKind::Radius => {
            let Geometry::Point(centre) = geometry else {
                return Err(EdrError::BadRequest(
                    "The radius query takes POINT coords".into(),
                ));
            };
            let radius_m = query.radius_metres()?;
            Ok(cells_within_radius(collection, *centre, radius_m))
        }
        QueryKind::Area => {
            let Geometry::Polygon(_) = geometry else {
                return Err(EdrError::BadRequest(
                    "The area query takes POLYGON coords".into(),
                ));
            };
            let [w, s, e, n] = geometry.bbox();
            let (x_idx, y_idx) = rectangle(collection, w, s, e, n)?;
            // Keep only the cells whose centre falls inside the polygon.
            let mask: HashSet<(usize, usize)> = y_idx
                .iter()
                .flat_map(|&y| x_idx.iter().map(move |&x| (y, x)))
                .filter(|&(y, x)| {
                    geometry.contains(Coord {
                        x: collection.to_crs84_lon(collection.x.values[x]),
                        y: collection.y.values[y],
                    })
                })
                .collect();
            Ok((x_idx, y_idx, Some(mask)))
        }
        QueryKind::Cube => {
            let [w, s, e, n] = geometry.bbox();
            let (x_idx, y_idx) = rectangle(collection, w, s, e, n)?;
            Ok((x_idx, y_idx, None))
        }
    }
}

fn nearest_cell(collection: &Collection, p: Coord) -> (usize, usize) {
    (collection.y.nearest_index(p.y), collection.nearest_x(p.x))
}

/// Turn scattered cells into axis index lists plus a mask, so the SQL reads one
/// rectangle and the response keeps only the requested cells.
fn from_cells(cells: Vec<(usize, usize)>) -> SpatialSelection {
    let mut y_idx: Vec<usize> = cells.iter().map(|c| c.0).collect();
    let mut x_idx: Vec<usize> = cells.iter().map(|c| c.1).collect();
    y_idx.sort_unstable();
    y_idx.dedup();
    x_idx.sort_unstable();
    x_idx.dedup();
    let mask = (y_idx.len() * x_idx.len() != cells.len())
        .then(|| cells.into_iter().collect::<HashSet<_>>());
    (x_idx, y_idx, mask)
}

fn cells_within_radius(collection: &Collection, centre: Coord, radius_m: f64) -> SpatialSelection {
    // Bound the search by a lat/lon box, then keep cells inside the true
    // great-circle radius.
    let deg_lat = radius_m / 111_320.0;
    let cos_lat = centre.y.to_radians().cos().abs().max(1e-6);
    let deg_lon = (deg_lat / cos_lat).min(180.0);

    let y_idx = collection
        .y
        .indices_in_range(centre.y - deg_lat, centre.y + deg_lat);
    let x_idx = longitude_indices(collection, centre.x - deg_lon, centre.x + deg_lon);

    let mask: HashSet<(usize, usize)> = y_idx
        .iter()
        .flat_map(|&y| x_idx.iter().map(move |&x| (y, x)))
        .filter(|&(y, x)| {
            haversine_metres(
                centre.x,
                centre.y,
                collection.to_crs84_lon(collection.x.values[x]),
                collection.y.values[y],
            ) <= radius_m
        })
        .collect();

    // A radius smaller than the grid spacing would select nothing; fall back to
    // the single nearest cell so the query answers rather than 404s.
    if mask.is_empty() {
        let cell = nearest_cell(collection, centre);
        return from_cells(vec![cell]);
    }
    (x_idx, y_idx, Some(mask))
}

fn rectangle(
    collection: &Collection,
    west: f64,
    south: f64,
    east: f64,
    north: f64,
) -> EdrResult<(Vec<usize>, Vec<usize>)> {
    if south > north {
        return Err(EdrError::BadRequest(
            "The bounding box's south edge is north of its north edge".into(),
        ));
    }
    let y_idx = collection.y.indices_in_range(south, north);
    let x_idx = longitude_indices(collection, west, east);
    if y_idx.is_empty() || x_idx.is_empty() {
        return Err(EdrError::NotFound(
            "The requested area does not overlap the collection's grid".into(),
        ));
    }
    Ok((x_idx, y_idx))
}

/// Longitude index selection, handling both the 0…360 storage convention and
/// boxes that cross the antimeridian (`west > east`).
fn longitude_indices(collection: &Collection, west: f64, east: f64) -> Vec<usize> {
    let axis = &collection.x;
    if east - west >= 360.0 {
        return (0..axis.len()).collect();
    }
    let mut indices: Vec<usize> = (0..axis.len())
        .filter(|&i| {
            let lon = collection.to_crs84_lon(axis.values[i]);
            // Compare on the circle: how far east of `west` does this lie?
            let span = (east - west).rem_euclid(360.0);
            let offset = (lon - west).rem_euclid(360.0);
            offset <= span + 1e-9 || (span == 0.0 && offset.min(360.0 - offset) < 1e-9)
        })
        .collect();
    indices.sort_unstable();
    indices
}

/// Apply `resolution-x` / `resolution-y` / `resolution-z` by thinning the
/// selected indices to at most the requested number of points.
fn thin_to_resolution(
    collection: &Collection,
    selection: &mut Selection,
    query: &Query,
) -> EdrResult<()> {
    for (axis, indices) in [
        ("x", &mut selection.x_idx),
        ("y", &mut selection.y_idx),
        ("z", &mut selection.z_idx),
    ] {
        if let Some(target) = query.resolution(axis)? {
            *indices = thin(std::mem::take(indices), target);
        }
    }
    if collection.z.is_none() && query.resolution("z")?.is_some() {
        return Err(EdrError::BadRequest(
            "This collection has no vertical axis, so 'resolution-z' does not apply".into(),
        ));
    }
    // Thinning the axes invalidates a mask built from the full rectangle.
    if let Some(mask) = &mut selection.mask {
        let keep: HashSet<usize> = selection.x_idx.iter().copied().collect();
        let keep_y: HashSet<usize> = selection.y_idx.iter().copied().collect();
        mask.retain(|(y, x)| keep_y.contains(y) && keep.contains(x));
    }
    Ok(())
}

/// Pick `target` evenly spaced entries, always keeping both ends.
fn thin(indices: Vec<usize>, target: usize) -> Vec<usize> {
    if target >= indices.len() || indices.is_empty() {
        return indices;
    }
    if target == 1 {
        return vec![indices[0]];
    }
    let last = indices.len() - 1;
    let mut out: Vec<usize> = (0..target)
        .map(|i| indices[(i * last) / (target - 1)])
        .collect();
    out.dedup();
    out
}

/// A dense, axis-indexed result ready to encode.
#[derive(Debug)]
pub struct ResultSet {
    /// Longitudes in CRS84, in output order.
    pub x: Vec<f64>,
    pub y: Vec<f64>,
    /// Vertical levels; empty when no parameter in the response uses them.
    pub z: Vec<f64>,
    /// Times as epoch microseconds.
    pub t: Vec<i64>,
    /// For scattered selections (points, polygon, radius), the `(y, x)` output
    /// index pairs that carry data.
    pub cells: Option<Vec<(usize, usize)>>,
    pub params: BTreeMap<String, ParamValues>,
}

/// One parameter's values, laid out row-major over its own axes.
#[derive(Debug)]
pub struct ParamValues {
    /// Axis names in layout order: `["t", "z", "y", "x"]` or `["t", "y", "x"]`.
    pub axis_names: Vec<&'static str>,
    pub shape: Vec<usize>,
    pub values: Vec<Option<f64>>,
}

/// One cell of a result: a position in time, space and height, with the value
/// of each parameter there.
#[derive(Debug, Clone)]
pub struct ResultRow {
    pub t: i64,
    /// The vertical level, when the result has a vertical axis.
    pub z: Option<f64>,
    pub y: f64,
    /// Longitude in CRS84.
    pub x: f64,
    /// One entry per parameter, in the result's parameter order.
    pub values: Vec<Option<f64>>,
}

impl ResultSet {
    /// Walk the result cell by cell, in time, level, latitude, longitude order.
    ///
    /// A parameter that does not vary with height repeats down the vertical
    /// axis: a 2 m temperature is the same reading whichever pressure level the
    /// row is showing, so repeating it states that, where a gap would suggest
    /// the value is unknown there.
    pub fn rows(&self) -> impl Iterator<Item = ResultRow> + '_ {
        let levels = self.z.len().max(1);
        let cells: Vec<(usize, usize)> = match &self.cells {
            Some(cells) => cells.clone(),
            None => (0..self.y.len())
                .flat_map(|y| (0..self.x.len()).map(move |x| (y, x)))
                .collect(),
        };

        (0..self.t.len()).flat_map(move |ti| {
            let cells = cells.clone();
            (0..levels).flat_map(move |zi| {
                let cells = cells.clone();
                cells.into_iter().map(move |(yi, xi)| ResultRow {
                    t: self.t[ti],
                    z: self.z.get(zi).copied(),
                    y: self.y[yi],
                    x: self.x[xi],
                    values: self
                        .params
                        .values()
                        .map(|values| {
                            let flat = if values.axis_names.contains(&"z") {
                                ((ti * levels + zi) * self.y.len() + yi) * self.x.len() + xi
                            } else {
                                // No vertical axis of its own: the same value
                                // stands at every level.
                                (ti * self.y.len() + yi) * self.x.len() + xi
                            };
                            values.values.get(flat).copied().flatten()
                        })
                        .collect(),
                })
            })
        })
    }

    /// The parameter names, in the order [`Self::rows`] reports their values.
    pub fn parameter_names(&self) -> impl Iterator<Item = &String> {
        self.params.keys()
    }

    /// True when no parameter has a single non-null value.
    pub fn is_empty(&self) -> bool {
        self.params
            .values()
            .all(|p| p.values.iter().all(Option::is_none))
    }
}

/// Run the request and assemble the values.
pub async fn execute(ctx: &SessionContext, request: &DataRequest) -> EdrResult<ResultSet> {
    let collection = &request.collection;
    let selection = &request.selection;

    let x: Vec<f64> = selection
        .x_idx
        .iter()
        .map(|&i| collection.to_crs84_lon(collection.x.values[i]))
        .collect();
    let y: Vec<f64> = selection
        .y_idx
        .iter()
        .map(|&i| collection.y.values[i])
        .collect();
    let z: Vec<f64> = match collection.z.as_ref() {
        Some(axis) => selection.z_idx.iter().map(|&i| axis.values[i]).collect(),
        None => Vec::new(),
    };
    let t: Vec<i64> = selection
        .t_idx
        .iter()
        .map(|&i| collection.t.values[i])
        .collect();

    // Position in the output arrays, keyed by the value as it comes back from
    // Arrow. Both sides go through the same f32 -> f64 cast, so the bit
    // patterns match exactly.
    let x_pos: HashMap<u64, usize> = x
        .iter()
        .enumerate()
        .map(|(i, v)| (collection.to_native_lon(*v).to_bits(), i))
        .collect();
    let y_pos: HashMap<u64, usize> = y
        .iter()
        .enumerate()
        .map(|(i, v)| (v.to_bits(), i))
        .collect();
    let z_pos: HashMap<u64, usize> = z
        .iter()
        .enumerate()
        .map(|(i, v)| (v.to_bits(), i))
        .collect();
    let t_pos: HashMap<i64, usize> = t.iter().enumerate().map(|(i, v)| (*v, i)).collect();

    let mask: Option<HashSet<(usize, usize)>> = selection.mask.as_ref().map(|cells| {
        // Re-express the mask in output-index space.
        let y_out: HashMap<usize, usize> = selection
            .y_idx
            .iter()
            .enumerate()
            .map(|(o, &i)| (i, o))
            .collect();
        let x_out: HashMap<usize, usize> = selection
            .x_idx
            .iter()
            .enumerate()
            .map(|(o, &i)| (i, o))
            .collect();
        cells
            .iter()
            .filter_map(|(cy, cx)| Some((*y_out.get(cy)?, *x_out.get(cx)?)))
            .collect()
    });

    let (vertical_params, flat_params) = collection.split_by_vertical(&request.parameters);
    let mut params: BTreeMap<String, ParamValues> = BTreeMap::new();

    // Surface and pressure-level variables have different dimensionality and
    // cannot share a scan, so each group is its own query.
    for (group, uses_z) in [(flat_params, false), (vertical_params, true)] {
        if group.is_empty() {
            continue;
        }
        let names: Vec<String> = group.into_iter().cloned().collect();
        let axis_names: Vec<&'static str> = if uses_z {
            vec!["t", "z", "y", "x"]
        } else {
            vec!["t", "y", "x"]
        };
        let shape: Vec<usize> = if uses_z {
            vec![t.len(), z.len(), y.len(), x.len()]
        } else {
            vec![t.len(), y.len(), x.len()]
        };
        let cells = shape.iter().product::<usize>();

        let sql = build_sql(collection, selection, &names, uses_z);
        let batches = run(ctx, &sql).await?;

        let mut columns: HashMap<&str, Vec<f64>> = HashMap::new();
        for name in [&collection.x_name, &collection.y_name] {
            columns.insert(name.as_str(), batches_to_f64(&batches, name)?);
        }
        if uses_z {
            let z_name = collection
                .z_name
                .as_ref()
                .expect("vertical group has an axis");
            columns.insert(z_name.as_str(), batches_to_f64(&batches, z_name)?);
        }
        let times = batches_to_time_micros(&batches, &collection.t_name)?;
        let values: HashMap<&String, Vec<f64>> = names
            .iter()
            .map(|n| Ok((n, batches_to_f64(&batches, n)?)))
            .collect::<EdrResult<_>>()?;
        let nulls: HashMap<&String, Vec<bool>> = names
            .iter()
            .map(|n| Ok((n, null_flags(&batches, n)?)))
            .collect::<EdrResult<_>>()?;

        let mut buffers: HashMap<&String, Vec<Option<f64>>> =
            names.iter().map(|n| (n, vec![None; cells])).collect();

        let lon = &columns[collection.x_name.as_str()];
        let lat = &columns[collection.y_name.as_str()];
        let lev = collection
            .z_name
            .as_ref()
            .filter(|_| uses_z)
            .map(|n| &columns[n.as_str()]);

        for row in 0..times.len() {
            let (Some(&ti), Some(&yi), Some(&xi)) = (
                t_pos.get(&times[row]),
                y_pos.get(&lat[row].to_bits()),
                x_pos.get(&lon[row].to_bits()),
            ) else {
                // A row outside the requested axes: the store returned a
                // coordinate the selection did not ask for.
                continue;
            };
            if mask.as_ref().is_some_and(|m| !m.contains(&(yi, xi))) {
                continue;
            }
            let flat = match lev {
                Some(levels) => {
                    let Some(&zi) = z_pos.get(&levels[row].to_bits()) else {
                        continue;
                    };
                    ((ti * z.len() + zi) * y.len() + yi) * x.len() + xi
                }
                None => (ti * y.len() + yi) * x.len() + xi,
            };
            for name in &names {
                if !nulls[name][row] {
                    buffers.get_mut(name).expect("buffer per parameter")[flat] =
                        Some(values[name][row]);
                }
            }
        }

        for name in &names {
            params.insert(
                name.clone(),
                ParamValues {
                    axis_names: axis_names.clone(),
                    shape: shape.clone(),
                    values: buffers.remove(name).expect("buffer per parameter"),
                },
            );
        }
    }

    let cells = mask.map(|m| {
        let mut cells: Vec<(usize, usize)> = m.into_iter().collect();
        cells.sort_unstable();
        cells
    });

    Ok(ResultSet {
        x,
        y,
        z,
        t,
        cells,
        params,
    })
}

/// Build the scan for one dimensionality group.
///
/// The vertical coordinate is named only in the pressure-level group: naming it
/// while projecting a surface variable makes zarr-datafusion declare a column
/// it then elides from the batch.
fn build_sql(
    collection: &Collection,
    selection: &Selection,
    parameters: &[String],
    uses_z: bool,
) -> String {
    let mut projection = vec![
        quote_ident(&collection.t_name),
        quote_ident(&collection.y_name),
        quote_ident(&collection.x_name),
    ];

    // The coordinate axes, paired with the indices selected on each.
    let mut axes: Vec<(&str, &NumericAxis, &[usize])> = vec![
        (collection.y_name.as_str(), &collection.y, &selection.y_idx),
        (collection.x_name.as_str(), &collection.x, &selection.x_idx),
    ];
    if uses_z {
        let z_name = collection
            .z_name
            .as_ref()
            .expect("vertical group has an axis");
        let z_axis = collection.z.as_ref().expect("vertical group has an axis");
        projection.insert(1, quote_ident(z_name));
        axes.push((z_name.as_str(), z_axis, &selection.z_idx));
    }
    projection.extend(parameters.iter().map(|p| quote_ident(p)));

    let mut predicates: Vec<String> = spatial_predicates(&axes);
    predicates.push(time_predicate(
        &collection.t_name,
        &selection.t_idx,
        &collection.t,
    ));

    format!(
        "SELECT {} FROM {} WHERE {}",
        projection.join(", "),
        quote_ident(&collection.table),
        predicates.join(" AND ")
    )
}

/// How one coordinate axis is constrained. Only contiguous forms are produced:
/// see [`spatial_predicates`].
#[derive(Debug, PartialEq)]
enum AxisPredicate {
    Eq(f64),
    /// A contiguous index run, expressed as value bounds.
    Between(f64, f64),
}

impl AxisPredicate {
    fn render(&self, column: &str) -> String {
        let column = quote_ident(column);
        match self {
            AxisPredicate::Eq(v) => format!("{column} = {}", format_f64(*v)),
            AxisPredicate::Between(lo, hi) => format!(
                "{column} BETWEEN {} AND {}",
                format_f64(*lo),
                format_f64(*hi)
            ),
        }
    }
}

/// Constrain each coordinate axis to a contiguous range.
///
/// A scattered selection — a box across the prime meridian, a thinned grid, a
/// list of pressure levels — is widened to the range that spans it rather than
/// written as an `IN` list. zarr-datafusion resolves an `IN` list to a set of
/// scattered indices, and when that axis is not the outermost dimension of the
/// variable it pairs the values it reads with the wrong coordinates, returning
/// data that looks plausible but belongs to other cells.
///
/// Widening is cheap where it matters: an ERA5 chunk spans the whole globe (and
/// every level) at one timestep, so a wider longitude, latitude or level range
/// reads the same chunks. The surplus rows are discarded when values are placed,
/// because only the coordinates actually selected have a slot.
fn spatial_predicates(axes: &[(&str, &NumericAxis, &[usize])]) -> Vec<String> {
    axes.iter()
        .map(|(name, axis, indices)| bounding_predicate(axis, indices).render(name))
        .collect()
}

/// The value range spanning `indices`, regardless of axis direction.
fn bounding_predicate(axis: &NumericAxis, indices: &[usize]) -> AxisPredicate {
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for &i in indices {
        lo = lo.min(axis.values[i]);
        hi = hi.max(axis.values[i]);
    }
    if lo == hi {
        AxisPredicate::Eq(lo)
    } else {
        AxisPredicate::Between(lo, hi)
    }
}

/// Number of indices a bounding range would cover.
fn hull_len(indices: &[usize]) -> usize {
    match (indices.iter().min(), indices.iter().max()) {
        (Some(lo), Some(hi)) => hi - lo + 1,
        _ => 0,
    }
}

/// The time selection is always contiguous by construction, so it too is a
/// range rather than a list of instants.
fn time_predicate(column: &str, indices: &[usize], axis: &TimeAxis) -> String {
    let column = quote_ident(column);
    let (Some(&first), Some(&last)) = (indices.iter().min(), indices.iter().max()) else {
        // Unreachable: an empty selection is rejected before the query is built.
        return format!("{column} IS NULL");
    };
    if first == last {
        format!("{column} = '{}'", sql_time(axis.values[first]))
    } else {
        format!(
            "{column} BETWEEN '{}' AND '{}'",
            sql_time(axis.values[first]),
            sql_time(axis.values[last])
        )
    }
}

/// A timestamp literal DataFusion compares against a UTC timestamp column.
fn sql_time(micros: i64) -> String {
    chrono::DateTime::from_timestamp_micros(micros)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
        .unwrap_or_default()
}

/// Format without an exponent, which DataFusion's SQL parser would read as an
/// identifier.
fn format_f64(v: f64) -> String {
    let s = format!("{v:.10}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

fn null_flags(batches: &[arrow::record_batch::RecordBatch], column: &str) -> EdrResult<Vec<bool>> {
    let mut out = Vec::new();
    for batch in batches {
        let col = batch
            .column_by_name(column)
            .ok_or_else(|| EdrError::Internal(format!("Column '{column}' missing from result")))?;
        out.extend((0..col.len()).map(|i| col.is_null(i)));
    }
    Ok(out)
}

pub fn format_time(micros: i64) -> String {
    chrono::DateTime::from_timestamp_micros(micros)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn summarise(values: &[f64]) -> String {
    let shown: Vec<String> = values.iter().take(8).map(|v| format_f64(*v)).collect();
    if values.len() > shown.len() {
        format!("{}, … ({} in total)", shown.join(", "), values.len())
    } else {
        shown.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinning_keeps_both_ends() {
        assert_eq!(thin((0..10).collect(), 3), vec![0, 4, 9]);
        assert_eq!(thin((0..10).collect(), 20), (0..10).collect::<Vec<_>>());
        assert_eq!(thin((0..10).collect(), 1), vec![0]);
        assert_eq!(thin(vec![], 5), Vec::<usize>::new());
    }

    #[test]
    fn floats_never_use_exponent_notation() {
        assert_eq!(format_f64(0.25), "0.25");
        assert_eq!(format_f64(-0.0000000001), "-0.0000000001");
        assert_eq!(format_f64(1e-12), "0");
        assert_eq!(format_f64(360.0), "360");
    }

    #[test]
    fn times_are_selected_as_an_instant_or_a_range() {
        let axis = TimeAxis::new((0..10).map(|i| i * 3_600_000_000).collect());
        assert_eq!(
            time_predicate("time", &[2], &axis),
            "\"time\" = '1970-01-01T02:00:00'"
        );
        assert_eq!(
            time_predicate("time", &[2, 3, 4], &axis),
            "\"time\" BETWEEN '1970-01-01T02:00:00' AND '1970-01-01T04:00:00'"
        );
    }

    #[test]
    fn scattered_cells_produce_a_mask_but_a_full_rectangle_does_not() {
        let (x, y, mask) = from_cells(vec![(0, 0), (1, 1)]);
        assert_eq!((x, y), (vec![0, 1], vec![0, 1]));
        assert_eq!(mask.unwrap().len(), 2);

        let (_, _, mask) = from_cells(vec![(0, 0), (0, 1), (1, 0), (1, 1)]);
        assert!(mask.is_none(), "a full rectangle needs no mask");
    }
}

#[cfg(test)]
mod predicate_tests {
    use super::*;

    fn lat() -> NumericAxis {
        NumericAxis::new((0..721).map(|i| 90.0 - i as f64 * 0.25).collect())
    }
    fn lon() -> NumericAxis {
        NumericAxis::new((0..1440).map(|i| i as f64 * 0.25).collect())
    }

    #[test]
    fn a_single_index_becomes_an_equality() {
        assert_eq!(bounding_predicate(&lat(), &[152]), AxisPredicate::Eq(52.0));
    }

    #[test]
    fn a_run_becomes_a_range_in_either_index_direction() {
        let axis = lat();
        // Ordering a north-to-south axis by value yields descending indices.
        assert_eq!(
            bounding_predicate(&axis, &[152, 153, 154]),
            AxisPredicate::Between(51.5, 52.0)
        );
        assert_eq!(
            bounding_predicate(&axis, &[154, 153, 152]),
            AxisPredicate::Between(51.5, 52.0)
        );
    }

    #[test]
    fn a_scattered_selection_is_widened_never_listed() {
        // A box across the prime meridian takes cells from both ends of the
        // axis; the predicate spans them rather than listing them.
        let axis = lon();
        let rendered = spatial_predicates(&[("longitude", &axis, &[0, 1, 1438, 1439])]);
        assert_eq!(rendered, vec!["\"longitude\" BETWEEN 0 AND 359.75"]);
        assert!(!rendered[0].contains(" IN ("));
    }

    #[test]
    fn every_axis_is_constrained_contiguously() {
        let (lat_axis, lon_axis) = (lat(), lon());
        let rendered = spatial_predicates(&[
            ("latitude", &lat_axis, &[100, 110, 120]),
            ("longitude", &lon_axis, &[0, 1, 1439]),
        ]);
        assert_eq!(rendered[0], "\"latitude\" BETWEEN 60 AND 65");
        assert_eq!(rendered[1], "\"longitude\" BETWEEN 0 AND 359.75");
    }

    #[test]
    fn the_scan_estimate_counts_the_widened_range() {
        let selection = Selection {
            // Two cells at opposite ends of the axis span everything between.
            x_idx: vec![0, 1439],
            y_idx: vec![100, 101],
            z_idx: Vec::new(),
            t_idx: vec![0],
            mask: None,
        };
        assert_eq!(selection.value_count(), 4);
        assert_eq!(selection.scan_row_count(), 1440 * 2);
    }
}
