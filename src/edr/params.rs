//! Parsing and validation of the EDR query-string parameters.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

use crate::error::{EdrError, EdrResult};

/// Response encodings this server produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    CoverageJson,
    GeoJson,
}

impl Format {
    pub fn media_type(self) -> &'static str {
        match self {
            Format::CoverageJson => "application/prs.coverage+json",
            Format::GeoJson => "application/geo+json",
        }
    }

    pub fn parse(raw: &str) -> EdrResult<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" => Ok(Format::CoverageJson),
            "covjson" | "coveragejson" | "application/prs.coverage+json" => {
                Ok(Format::CoverageJson)
            }
            "geojson" | "application/geo+json" => Ok(Format::GeoJson),
            other => Err(EdrError::NotSupported(format!(
                "Output format '{other}' is not supported; use CoverageJSON or GeoJSON"
            ))),
        }
    }
}

/// A `bbox` value: the horizontal box, plus the vertical bounds that the
/// six-element form carries.
pub type BoundingBox = ([f64; 4], Option<(f64, f64)>);

/// A `datetime` selection: a single instant or a (possibly half-open) interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TemporalSelection {
    Instant(i64),
    Interval(Option<i64>, Option<i64>),
}

/// A `z` selection, in the vertical coordinate's own units.
#[derive(Debug, Clone, PartialEq)]
pub enum VerticalSelection {
    All,
    Values(Vec<f64>),
    Interval(f64, f64),
}

/// The query string, normalised to lowercase keys.
///
/// A key may appear more than once. EDR spells a list of parameters
/// `parameter-name=a,b`, but an HTML `<select multiple>` submits one
/// `parameter-name=` per selected option, and both have to mean the same
/// thing — so every value is kept rather than the last overwriting the rest.
#[derive(Debug, Default, Clone)]
pub struct Query(HashMap<String, Vec<String>>);

impl Query {
    pub fn new(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut values: HashMap<String, Vec<String>> = HashMap::new();
        for (key, value) in pairs {
            values
                .entry(key.to_ascii_lowercase())
                .or_default()
                .push(value);
        }
        Query(values)
    }

    /// Set a parameter that was implied by another one, such as the vertical
    /// bounds carried in a six-element `bbox`.
    pub fn insert(&mut self, key: &str, value: String) {
        self.0.insert(key.to_ascii_lowercase(), vec![value]);
    }

    /// The first non-blank value carried for a key.
    ///
    /// A submitted form sends every field, so an untouched one arrives as an
    /// empty value; that is the same as not having been given.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.all(key).next()
    }

    /// Every non-blank value for a key, in the order they were sent.
    fn all(&self, key: &str) -> impl Iterator<Item = &str> {
        self.0
            .get(key)
            .into_iter()
            .flatten()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
    }

    pub fn required(&self, key: &str) -> EdrResult<&str> {
        self.get(key)
            .ok_or_else(|| EdrError::BadRequest(format!("Required parameter '{key}' is missing")))
    }

    pub fn format(&self) -> EdrResult<Format> {
        Format::parse(self.get("f").unwrap_or_default())
    }

    /// EDR permits `crs`, but this server only serves CRS84.
    pub fn crs(&self) -> EdrResult<()> {
        match self.get("crs") {
            None => Ok(()),
            Some(crs) => {
                let normalised = crs.trim().to_ascii_uppercase();
                let ok = normalised.contains("CRS84")
                    || normalised.contains("CRS:84")
                    || normalised.ends_with("4326")
                    || normalised == "WGS84";
                ok.then_some(()).ok_or_else(|| {
                    EdrError::NotSupported(format!(
                        "CRS '{crs}' is not supported; this collection is served in CRS84"
                    ))
                })
            }
        }
    }

    pub fn limit(&self, default: usize) -> EdrResult<usize> {
        match self.get("limit") {
            None => Ok(default),
            Some(raw) => raw.parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(|| {
                EdrError::BadRequest(format!("'limit' must be a positive integer, got '{raw}'"))
            }),
        }
    }

    /// The requested parameters; `None` when none were named.
    ///
    /// Accepts both spellings of the list — comma-separated in one value, or
    /// one value per parameter — and both spellings of the key, since
    /// `parameter_name` is common in the wild and costs nothing to allow.
    pub fn parameter_names(&self) -> Option<Vec<String>> {
        let names: Vec<String> = self
            .all("parameter-name")
            .chain(self.all("parameter_name"))
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect();
        (!names.is_empty()).then_some(names)
    }

    /// `datetime`, or the pair of fields the HTML form submits in its place.
    ///
    /// A date-and-time picker holds one instant, so the form carries two of
    /// them and they are composed here into the interval EDR spells with a
    /// slash. `datetime` itself always wins, leaving the API contract alone;
    /// an empty picker means that end is open.
    pub fn datetime(&self) -> EdrResult<Option<TemporalSelection>> {
        if let Some(raw) = self.get("datetime") {
            return parse_datetime(raw).map(Some);
        }
        match (self.get("datetime-from"), self.get("datetime-to")) {
            (None, None) => Ok(None),
            (from, to) => {
                parse_datetime(&format!("{}/{}", from.unwrap_or(".."), to.unwrap_or("..")))
                    .map(Some)
            }
        }
    }

    pub fn vertical(&self) -> EdrResult<Option<VerticalSelection>> {
        self.get("z").map(parse_z).transpose()
    }

    /// `bbox=west,south,east,north` (a 6-element form carries min/max z, whose
    /// vertical part is returned separately).
    pub fn bbox(&self) -> EdrResult<Option<BoundingBox>> {
        let Some(raw) = self.get("bbox") else {
            return Ok(None);
        };
        let nums: Vec<f64> = raw
            .split(',')
            .map(|v| {
                v.trim().parse::<f64>().map_err(|_| {
                    EdrError::BadRequest(format!("'bbox' contains a non-numeric value: '{v}'"))
                })
            })
            .collect::<EdrResult<_>>()?;
        match nums.as_slice() {
            [w, s, e, n] => Ok(Some(([*w, *s, *e, *n], None))),
            [w, s, zmin, e, n, zmax] => Ok(Some(([*w, *s, *e, *n], Some((*zmin, *zmax))))),
            _ => Err(EdrError::BadRequest(
                "'bbox' must have four values (west,south,east,north) or six with min/max z".into(),
            )),
        }
    }

    /// `within` plus `within-units`, converted to metres.
    pub fn radius_metres(&self) -> EdrResult<f64> {
        let raw = self.required("within")?;
        let value: f64 = raw
            .parse()
            .map_err(|_| EdrError::BadRequest(format!("'within' must be a number, got '{raw}'")))?;
        if !(value.is_finite() && value > 0.0) {
            return Err(EdrError::BadRequest(
                "'within' must be a positive distance".into(),
            ));
        }
        let units = self.get("within-units").unwrap_or("km");
        let per_unit = match units.trim().to_ascii_lowercase().as_str() {
            "m" | "metre" | "metres" | "meter" | "meters" => 1.0,
            "km" | "kilometre" | "kilometres" | "kilometer" | "kilometers" => 1_000.0,
            "mi" | "mile" | "miles" => 1_609.344,
            "nmi" | "nm" => 1_852.0,
            "ft" | "feet" => 0.3048,
            other => {
                return Err(EdrError::NotSupported(format!(
                    "'within-units' of '{other}' is not supported; use m, km, mi, nmi or ft"
                )));
            }
        };
        Ok(value * per_unit)
    }

    /// `resolution-x` / `resolution-y` / `resolution-z`: how many points to
    /// return along an axis, thinning the grid to fit.
    pub fn resolution(&self, axis: &str) -> EdrResult<Option<usize>> {
        let key = format!("resolution-{axis}");
        match self.get(&key) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .map(Some)
                .ok_or_else(|| {
                    EdrError::BadRequest(format!(
                        "'{key}' must be a positive number of points, got '{raw}'"
                    ))
                }),
        }
    }
}

/// RFC 3339 instants, or an interval `start/end` where either side may be open
/// (`..` or empty).
pub fn parse_datetime(raw: &str) -> EdrResult<TemporalSelection> {
    let raw = raw.trim();
    if let Some((start, end)) = raw.split_once('/') {
        let start = parse_open_instant(start)?;
        let end = parse_open_instant(end)?;
        if matches!((start, end), (Some(s), Some(e)) if s > e) {
            return Err(EdrError::BadRequest(
                "'datetime' interval ends before it starts".into(),
            ));
        }
        if start.is_none() && end.is_none() {
            return Err(EdrError::BadRequest(
                "'datetime' interval must bound at least one end".into(),
            ));
        }
        Ok(TemporalSelection::Interval(start, end))
    } else {
        Ok(TemporalSelection::Instant(parse_instant(raw)?))
    }
}

fn parse_open_instant(raw: &str) -> EdrResult<Option<i64>> {
    let raw = raw.trim();
    if raw.is_empty() || raw == ".." {
        Ok(None)
    } else {
        parse_instant(raw).map(Some)
    }
}

/// Parse one instant to epoch microseconds. Accepts a full RFC 3339 timestamp,
/// a timezone-less timestamp (read as UTC), or a bare date.
pub fn parse_instant(raw: &str) -> EdrResult<i64> {
    let raw = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.with_timezone(&Utc).timestamp_micros());
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(raw, format) {
            return Ok(naive.and_utc().timestamp_micros());
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Ok(date
            .and_hms_opt(0, 0, 0)
            .expect("midnight is a valid time")
            .and_utc()
            .timestamp_micros());
    }
    Err(EdrError::BadRequest(format!(
        "'{raw}' is not an RFC 3339 date-time"
    )))
}

/// `z=500`, `z=850,700,500`, `z=500/850`, `z=R5/1000/-100`, or `z=all`.
pub fn parse_z(raw: &str) -> EdrResult<VerticalSelection> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("all") {
        return Ok(VerticalSelection::All);
    }

    let number = |v: &str| -> EdrResult<f64> {
        v.trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .ok_or_else(|| EdrError::BadRequest(format!("'z' contains a non-numeric value: '{v}'")))
    };

    // Repeating interval: R{count}/{start}/{step}
    if let Some(rest) = raw.strip_prefix('R').or_else(|| raw.strip_prefix('r')) {
        let parts: Vec<&str> = rest.split('/').collect();
        let [count, start, step] = parts.as_slice() else {
            return Err(EdrError::BadRequest(
                "A repeating 'z' must be written R{count}/{start}/{step}".into(),
            ));
        };
        let count: usize = count.trim().parse().map_err(|_| {
            EdrError::BadRequest(format!("'z' repeat count '{count}' is not a number"))
        })?;
        if count == 0 || count > 10_000 {
            return Err(EdrError::BadRequest(
                "'z' repeat count must be between 1 and 10000".into(),
            ));
        }
        let (start, step) = (number(start)?, number(step)?);
        return Ok(VerticalSelection::Values(
            (0..count).map(|i| start + step * i as f64).collect(),
        ));
    }

    if let Some((lo, hi)) = raw.split_once('/') {
        let (lo, hi) = (number(lo)?, number(hi)?);
        return Ok(VerticalSelection::Interval(lo.min(hi), lo.max(hi)));
    }

    let values = raw
        .split(',')
        .map(number)
        .collect::<EdrResult<Vec<f64>>>()?;
    if values.is_empty() {
        return Err(EdrError::BadRequest("'z' is empty".into()));
    }
    Ok(VerticalSelection::Values(values))
}

/// Great-circle distance in metres on a spherical Earth — accurate enough to
/// pick grid cells inside a radius.
pub fn haversine_metres(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    const EARTH_RADIUS_M: f64 = 6_371_008.8;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dlat = p2 - p1;
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn micros(rfc3339: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_micros()
    }

    #[test]
    fn parses_instants_intervals_and_open_ends() {
        assert_eq!(
            parse_datetime("2024-01-01T00:00:00Z").unwrap(),
            TemporalSelection::Instant(micros("2024-01-01T00:00:00Z"))
        );
        assert_eq!(
            parse_datetime("2024-01-01/2024-01-02").unwrap(),
            TemporalSelection::Interval(
                Some(micros("2024-01-01T00:00:00Z")),
                Some(micros("2024-01-02T00:00:00Z"))
            )
        );
        assert_eq!(
            parse_datetime("../2024-01-02T00:00:00Z").unwrap(),
            TemporalSelection::Interval(None, Some(micros("2024-01-02T00:00:00Z")))
        );
        assert_eq!(
            parse_datetime("2024-01-01T00:00:00Z/..").unwrap(),
            TemporalSelection::Interval(Some(micros("2024-01-01T00:00:00Z")), None)
        );
    }

    #[test]
    fn rejects_backwards_and_fully_open_intervals() {
        assert!(parse_datetime("2024-01-02/2024-01-01").is_err());
        assert!(parse_datetime("../..").is_err());
        assert!(parse_datetime("yesterday").is_err());
    }

    #[test]
    fn offsets_are_normalised_to_utc() {
        assert_eq!(
            parse_datetime("2024-01-01T01:00:00+01:00").unwrap(),
            TemporalSelection::Instant(micros("2024-01-01T00:00:00Z"))
        );
    }

    #[test]
    fn parses_every_z_form() {
        assert_eq!(parse_z("all").unwrap(), VerticalSelection::All);
        assert_eq!(
            parse_z("500").unwrap(),
            VerticalSelection::Values(vec![500.0])
        );
        assert_eq!(
            parse_z("850,700,500").unwrap(),
            VerticalSelection::Values(vec![850.0, 700.0, 500.0])
        );
        assert_eq!(
            parse_z("500/850").unwrap(),
            VerticalSelection::Interval(500.0, 850.0)
        );
        assert_eq!(
            parse_z("R3/1000/-100").unwrap(),
            VerticalSelection::Values(vec![1000.0, 900.0, 800.0])
        );
        assert!(parse_z("R0/1000/-100").is_err());
        assert!(parse_z("high").is_err());
    }

    #[test]
    fn radius_units_convert_to_metres() {
        let q = |within: &str, units: Option<&str>| {
            let mut map = HashMap::from([("within".to_string(), within.to_string())]);
            if let Some(u) = units {
                map.insert("within-units".to_string(), u.to_string());
            }
            Query::new(map).radius_metres()
        };
        assert_eq!(q("10", None).unwrap(), 10_000.0);
        assert_eq!(q("10", Some("m")).unwrap(), 10.0);
        assert_eq!(q("1", Some("nmi")).unwrap(), 1_852.0);
        assert!(q("-1", Some("km")).is_err());
        assert!(q("10", Some("parsecs")).is_err());
    }

    #[test]
    fn bbox_accepts_four_and_six_element_forms() {
        let q = |v: &str| Query::new(HashMap::from([("bbox".into(), v.to_string())])).bbox();
        assert_eq!(q("-1,50,1,52").unwrap().unwrap().0, [-1.0, 50.0, 1.0, 52.0]);
        let (bbox, z) = q("-1,50,500,1,52,850").unwrap().unwrap();
        assert_eq!(bbox, [-1.0, 50.0, 1.0, 52.0]);
        assert_eq!(z, Some((500.0, 850.0)));
        assert!(q("-1,50,1").is_err());
    }

    #[test]
    fn only_crs84_equivalents_are_accepted() {
        let crs = |v: &str| Query::new(HashMap::from([("crs".into(), v.to_string())])).crs();
        assert!(crs("CRS84").is_ok());
        assert!(crs("http://www.opengis.net/def/crs/OGC/1.3/CRS84").is_ok());
        assert!(crs("EPSG:4326").is_ok());
        assert!(crs("EPSG:3857").is_err());
    }

    #[test]
    fn haversine_matches_a_known_distance() {
        // London to Paris, ~343 km.
        let d = haversine_metres(-0.1276, 51.5072, 2.3522, 48.8566);
        assert!((d - 343_500.0).abs() < 2_000.0, "got {d} m");
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;

    fn selection(pairs: &[(&str, &str)]) -> EdrResult<Option<TemporalSelection>> {
        Query::new(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>(),
        )
        .datetime()
    }

    #[test]
    fn the_two_pickers_compose_into_an_interval() {
        let from = "2024-01-01T00:00";
        let to = "2024-01-02T00:00";
        assert_eq!(
            selection(&[("datetime-from", from), ("datetime-to", to)]).unwrap(),
            Some(parse_datetime(&format!("{from}/{to}")).unwrap())
        );
    }

    #[test]
    fn one_empty_picker_leaves_that_end_open() {
        let from = "2024-01-01T00:00";
        assert_eq!(
            selection(&[("datetime-from", from), ("datetime-to", "")]).unwrap(),
            Some(parse_datetime(&format!("{from}/..")).unwrap())
        );
        assert_eq!(
            selection(&[("datetime-from", ""), ("datetime-to", from)]).unwrap(),
            Some(parse_datetime(&format!("../{from}")).unwrap())
        );
    }

    #[test]
    fn both_empty_means_no_selection_at_all() {
        // A submitted form sends every field, so this is the untouched state:
        // it must read as absent, not as an unbounded interval.
        assert_eq!(
            selection(&[("datetime-from", ""), ("datetime-to", "")]).unwrap(),
            None
        );
        assert_eq!(selection(&[]).unwrap(), None);
    }

    #[test]
    fn the_api_spelling_always_wins() {
        // An EDR client's `datetime` is never second-guessed by form fields.
        let explicit = "2024-03-01T00:00:00Z/2024-03-02T00:00:00Z";
        assert_eq!(
            selection(&[
                ("datetime", explicit),
                ("datetime-from", "1999-01-01T00:00"),
                ("datetime-to", "1999-01-02T00:00"),
            ])
            .unwrap(),
            Some(parse_datetime(explicit).unwrap())
        );
    }

    #[test]
    fn a_reversed_pair_is_rejected_like_any_interval() {
        assert!(
            selection(&[
                ("datetime-from", "2024-01-02T00:00"),
                ("datetime-to", "2024-01-01T00:00"),
            ])
            .is_err()
        );
    }
}
