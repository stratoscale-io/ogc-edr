//! GeoJSON encoding: one point feature per grid cell, time and level.
//!
//! GeoJSON has no notion of a coverage, so a result is flattened into features.
//! Cells with no data at all are dropped rather than emitted as null features.

use serde_json::{Map, Value, json};

use crate::edr::query::{DataRequest, ResultSet, format_time};

pub fn encode(request: &DataRequest, result: &ResultSet) -> Value {
    let collection = &request.collection;
    let names: Vec<&String> = result.parameter_names().collect();

    let features: Vec<Value> = result
        .rows()
        .filter_map(|row| {
            // A cell where nothing was measured is left out rather than
            // emitted as a feature of nulls.
            if row.values.iter().all(Option::is_none) {
                return None;
            }

            let mut properties = Map::new();
            properties.insert("datetime".into(), json!(format_time(row.t)));
            if let Some(level) = row.z {
                properties.insert(
                    collection.z_name.clone().unwrap_or_else(|| "z".into()),
                    json!(level),
                );
            }
            for (name, value) in names.iter().zip(&row.values) {
                properties.insert(
                    (*name).clone(),
                    match value {
                        Some(v) if v.is_finite() => json!(v),
                        _ => Value::Null,
                    },
                );
            }

            Some(json!({
                "type": "Feature",
                "geometry": { "type": "Point", "coordinates": [row.x, row.y] },
                "properties": Value::Object(properties),
            }))
        })
        .collect();

    json!({
        "type": "FeatureCollection",
        "features": features,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edr::query::ParamValues;
    use std::collections::BTreeMap;

    fn result() -> ResultSet {
        ResultSet {
            x: vec![0.0, 0.25],
            y: vec![51.5],
            z: Vec::new(),
            t: vec![0, 3_600_000_000],
            cells: None,
            params: BTreeMap::from([(
                "2m_temperature".to_string(),
                ParamValues {
                    axis_names: vec!["t", "y", "x"],
                    shape: vec![2, 1, 2],
                    values: vec![Some(280.0), None, Some(281.0), Some(282.0)],
                },
            )]),
        }
    }

    #[test]
    fn rows_are_walked_in_time_then_space_order() {
        let result = result();
        let rows: Vec<_> = result.rows().collect();
        assert_eq!(rows.len(), 4, "2 times x 1 latitude x 2 longitudes");
        assert_eq!((rows[0].t, rows[0].x), (0, 0.0));
        assert_eq!((rows[1].t, rows[1].x), (0, 0.25));
        assert_eq!((rows[2].t, rows[2].x), (3_600_000_000, 0.0));
        assert_eq!(rows[0].values, vec![Some(280.0)]);
        assert_eq!(rows[1].values, vec![None]);
    }

    #[test]
    fn a_surface_parameter_repeats_down_the_vertical_axis() {
        let mut result = result();
        result.z = vec![500.0, 850.0];
        let rows: Vec<_> = result.rows().collect();
        // 2 times x 2 levels x 2 cells.
        assert_eq!(rows.len(), 8);
        assert_eq!(rows[0].z, Some(500.0));
        assert_eq!(rows[2].z, Some(850.0));
        // The same reading stands at both levels, rather than going missing.
        assert_eq!(rows[0].values, rows[2].values);
    }
}
