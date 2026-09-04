//! CoverageJSON encoding — the default EDR response format.

use serde_json::{Map, Value, json};

use crate::catalog::{Collection, Parameter};
use crate::edr::query::{DataRequest, ParamValues, QueryKind, ResultSet, format_time};

/// EDR `parameter_names` / CoverageJSON `parameters` entry for one variable.
pub fn parameter_json(param: &Parameter) -> Value {
    let mut unit = Map::new();
    match &param.unit {
        Some(symbol) => {
            unit.insert("label".into(), json!({ "en": symbol }));
            unit.insert(
                "symbol".into(),
                json!({
                    "value": symbol,
                    "type": "http://www.opengis.net/def/uom/UCUM/",
                }),
            );
        }
        // The store recorded no units; say so rather than implying one.
        None => {
            unit.insert("label".into(), json!({ "en": "unknown" }));
        }
    }

    let observed_id = param
        .standard_name
        .as_ref()
        .map(|sn| format!("http://vocab.nerc.ac.uk/standard_name/{sn}/"));
    let mut observed = Map::new();
    if let Some(id) = observed_id {
        observed.insert("id".into(), json!(id));
    }
    observed.insert("label".into(), json!({ "en": param.label }));
    if let Some(description) = &param.description {
        observed.insert("description".into(), json!({ "en": description }));
    }

    json!({
        "type": "Parameter",
        "id": param.name,
        "label": { "en": param.label },
        "description": { "en": param.description.clone().unwrap_or_else(|| param.label.clone()) },
        "unit": Value::Object(unit),
        "observedProperty": Value::Object(observed),
    })
}

/// Encode a result as a Coverage, or a CoverageCollection when a position query
/// asked for several separate points.
pub fn encode(request: &DataRequest, result: &ResultSet) -> Value {
    let collection = &request.collection;

    // Scattered points stay separate coverages; an area or cube is one grid
    // whose unselected cells are null.
    let scattered = matches!(request.kind, QueryKind::Position)
        && result.cells.as_ref().is_some_and(|c| c.len() > 1);

    if scattered {
        let cells = result.cells.as_ref().expect("scattered implies cells");
        let coverages: Vec<Value> = cells
            .iter()
            // Inside a CoverageCollection the parameters are declared once, at
            // the collection level, so the members omit them.
            .map(|&(y, x)| single_cell_coverage(collection, result, y, x, false))
            .collect();
        return json!({
            "type": "CoverageCollection",
            "domainType": domain_type(1, 1, result.z.len(), result.t.len()),
            "parameters": parameters_json(collection, result),
            "referencing": referencing(collection, !result.z.is_empty()),
            "coverages": coverages,
        });
    }

    if matches!(request.kind, QueryKind::Position) && result.x.len() == 1 && result.y.len() == 1 {
        return single_cell_coverage(collection, result, 0, 0, true);
    }

    let ranges: Map<String, Value> = result
        .params
        .iter()
        .map(|(name, values)| (name.clone(), ndarray(collection, name, values)))
        .collect();

    json!({
        "type": "Coverage",
        "domain": domain(collection, result, &result.x, &result.y),
        "parameters": parameters_json(collection, result),
        "ranges": Value::Object(ranges),
    })
}

/// A coverage over exactly one grid cell, taken from the result at output
/// indices `(y, x)`.
fn single_cell_coverage(
    collection: &Collection,
    result: &ResultSet,
    y: usize,
    x: usize,
    include_parameters: bool,
) -> Value {
    let xs = vec![result.x[x]];
    let ys = vec![result.y[y]];
    let ranges: Map<String, Value> = result
        .params
        .iter()
        .map(|(name, values)| {
            (
                name.clone(),
                ndarray(collection, name, &slice_cell(values, y, x)),
            )
        })
        .collect();

    let mut coverage = Map::new();
    coverage.insert("type".into(), json!("Coverage"));
    coverage.insert("domain".into(), domain(collection, result, &xs, &ys));
    if include_parameters {
        coverage.insert("parameters".into(), parameters_json(collection, result));
    }
    coverage.insert("ranges".into(), Value::Object(ranges));
    Value::Object(coverage)
}

/// Extract the series at one `(y, x)` cell, keeping the t (and z) axes.
fn slice_cell(values: &ParamValues, y: usize, x: usize) -> ParamValues {
    let (nt, nz, ny, nx) = dims(values);
    let mut out = Vec::with_capacity(nt * nz);
    for ti in 0..nt {
        for zi in 0..nz {
            let flat = if values.axis_names.contains(&"z") {
                ((ti * nz + zi) * ny + y) * nx + x
            } else {
                (ti * ny + y) * nx + x
            };
            out.push(values.values.get(flat).copied().flatten());
        }
    }
    let mut shape = vec![nt];
    let mut axis_names = vec!["t"];
    if values.axis_names.contains(&"z") {
        shape.push(nz);
        axis_names.push("z");
    }
    shape.push(1);
    shape.push(1);
    axis_names.push("y");
    axis_names.push("x");
    ParamValues {
        axis_names,
        shape,
        values: out,
    }
}

fn dims(values: &ParamValues) -> (usize, usize, usize, usize) {
    match values.shape.as_slice() {
        [t, z, y, x] => (*t, *z, *y, *x),
        [t, y, x] => (*t, 1, *y, *x),
        _ => (1, 1, 1, 1),
    }
}

fn ndarray(collection: &Collection, name: &str, values: &ParamValues) -> Value {
    let data_type = collection
        .parameter(name)
        .map_or("float", |p| p.cov_data_type);
    json!({
        "type": "NdArray",
        "dataType": data_type,
        "axisNames": values.axis_names,
        "shape": values.shape,
        "values": encode_values(values),
    })
}

/// CoverageJSON represents a missing value as null.
fn encode_values(values: &ParamValues) -> Vec<Value> {
    values
        .values
        .iter()
        .map(|v| match v {
            Some(v) if v.is_finite() => json!(v),
            _ => Value::Null,
        })
        .collect()
}

fn parameters_json(collection: &Collection, result: &ResultSet) -> Value {
    let map: Map<String, Value> = result
        .params
        .keys()
        .filter_map(|name| {
            collection
                .parameter(name)
                .map(|p| (name.clone(), parameter_json(p)))
        })
        .collect();
    Value::Object(map)
}

fn domain(collection: &Collection, result: &ResultSet, xs: &[f64], ys: &[f64]) -> Value {
    let mut axes = Map::new();
    axes.insert("x".into(), json!({ "values": xs }));
    axes.insert("y".into(), json!({ "values": ys }));
    if !result.z.is_empty() {
        axes.insert("z".into(), json!({ "values": result.z }));
    }
    let times: Vec<String> = result.t.iter().map(|t| format_time(*t)).collect();
    axes.insert("t".into(), json!({ "values": times }));

    json!({
        "type": "Domain",
        "domainType": domain_type(xs.len(), ys.len(), result.z.len(), result.t.len()),
        "axes": Value::Object(axes),
        "referencing": referencing(collection, !result.z.is_empty()),
    })
}

/// Pick the CoverageJSON domain type that matches the shape of the result.
fn domain_type(nx: usize, ny: usize, nz: usize, nt: usize) -> &'static str {
    let single_cell = nx <= 1 && ny <= 1;
    match (single_cell, nz > 1, nt > 1) {
        (true, false, false) => "Point",
        (true, false, true) => "PointSeries",
        (true, true, false) => "VerticalProfile",
        _ => "Grid",
    }
}

fn referencing(collection: &Collection, has_z: bool) -> Value {
    let mut systems = vec![json!({
        "coordinates": ["x", "y"],
        "system": {
            "type": "GeographicCRS",
            "id": "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        }
    })];
    if has_z {
        systems.push(json!({
            "coordinates": ["z"],
            "system": {
                "type": "VerticalCRS",
                "cs": {
                    "csAxes": [{
                        "name": { "en": collection.z_name.clone().unwrap_or_else(|| "z".into()) },
                        "direction": "down",
                        "unit": { "symbol": collection.z_units.clone().unwrap_or_else(|| "unknown".into()) },
                    }]
                }
            }
        }));
    }
    systems.push(json!({
        "coordinates": ["t"],
        "system": { "type": "TemporalRS", "calendar": "Gregorian" }
    }));
    Value::Array(systems)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_type_follows_the_result_shape() {
        assert_eq!(domain_type(1, 1, 0, 1), "Point");
        assert_eq!(domain_type(1, 1, 0, 24), "PointSeries");
        assert_eq!(domain_type(1, 1, 37, 1), "VerticalProfile");
        assert_eq!(domain_type(1, 1, 37, 24), "Grid");
        assert_eq!(domain_type(10, 10, 0, 1), "Grid");
    }

    #[test]
    fn slicing_a_cell_keeps_the_time_series() {
        // 2 times x 2 lats x 2 lons, surface variable.
        let values = ParamValues {
            axis_names: vec!["t", "y", "x"],
            shape: vec![2, 2, 2],
            values: (0..8).map(|i| Some(i as f64)).collect(),
        };
        let cell = slice_cell(&values, 1, 0);
        assert_eq!(cell.shape, vec![2, 1, 1]);
        assert_eq!(cell.axis_names, vec!["t", "y", "x"]);
        // (t=0,y=1,x=0) is index 2; (t=1,y=1,x=0) is index 6.
        assert_eq!(cell.values, vec![Some(2.0), Some(6.0)]);
    }

    #[test]
    fn slicing_keeps_the_vertical_axis() {
        // 1 time x 3 levels x 1 lat x 2 lons.
        let values = ParamValues {
            axis_names: vec!["t", "z", "y", "x"],
            shape: vec![1, 3, 1, 2],
            values: (0..6).map(|i| Some(i as f64)).collect(),
        };
        let cell = slice_cell(&values, 0, 1);
        assert_eq!(cell.shape, vec![1, 3, 1, 1]);
        assert_eq!(cell.values, vec![Some(1.0), Some(3.0), Some(5.0)]);
    }

    #[test]
    fn missing_values_encode_as_null() {
        let values = ParamValues {
            axis_names: vec!["t", "y", "x"],
            shape: vec![1, 1, 2],
            values: vec![Some(1.5), None],
        };
        let encoded = encode_values(&values);
        assert_eq!(encoded[0], json!(1.5));
        assert_eq!(encoded[1], Value::Null);
    }
}
