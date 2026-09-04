//! The slice of WKT that OGC API - EDR's `coords` parameter uses.
//!
//! EDR passes geometry as WKT in the query string, so only the 2-D geometry
//! types the data queries accept are parsed here: POINT, MULTIPOINT,
//! LINESTRING and POLYGON.

use crate::error::{EdrError, EdrResult};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coord {
    /// Longitude, degrees east, CRS84.
    pub x: f64,
    /// Latitude, degrees north.
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    Point(Coord),
    MultiPoint(Vec<Coord>),
    LineString(Vec<Coord>),
    /// Exterior ring followed by any interior rings (holes).
    Polygon(Vec<Vec<Coord>>),
}

impl Geometry {
    /// Every vertex, for extent computation.
    pub fn vertices(&self) -> Vec<Coord> {
        match self {
            Geometry::Point(c) => vec![*c],
            Geometry::MultiPoint(cs) | Geometry::LineString(cs) => cs.clone(),
            Geometry::Polygon(rings) => rings.concat(),
        }
    }

    /// `[west, south, east, north]` over all vertices.
    pub fn bbox(&self) -> [f64; 4] {
        let vertices = self.vertices();
        let mut bbox = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        for c in vertices {
            bbox[0] = bbox[0].min(c.x);
            bbox[1] = bbox[1].min(c.y);
            bbox[2] = bbox[2].max(c.x);
            bbox[3] = bbox[3].max(c.y);
        }
        bbox
    }

    /// Ray-casting test against the exterior ring, minus any hole that contains
    /// the point. Non-polygon geometries are treated as covering nothing.
    ///
    /// A point exactly on a boundary counts as inside, so an area query over a
    /// rectangle returns the same cells as the equivalent cube: on a 0.25° grid
    /// a great many cell centres land precisely on a whole-degree edge, and
    /// dropping them would punch holes along the border.
    pub fn contains(&self, p: Coord) -> bool {
        let Geometry::Polygon(rings) = self else {
            return false;
        };
        let Some(exterior) = rings.first() else {
            return false;
        };
        if rings.iter().any(|ring| on_boundary(ring, p)) {
            return true;
        }
        if !ring_contains(exterior, p) {
            return false;
        }
        !rings[1..].iter().any(|hole| ring_contains(hole, p))
    }
}

/// Whether `p` lies on any edge of `ring`, within a tolerance far below the
/// grid spacing of the collections served here.
fn on_boundary(ring: &[Coord], p: Coord) -> bool {
    const EPSILON: f64 = 1e-9;
    ring.windows(2).any(|edge| {
        let (a, b) = (edge[0], edge[1]);
        // Outside the edge's bounding box: cannot be on it.
        if p.x < a.x.min(b.x) - EPSILON
            || p.x > a.x.max(b.x) + EPSILON
            || p.y < a.y.min(b.y) - EPSILON
            || p.y > a.y.max(b.y) + EPSILON
        {
            return false;
        }
        // Collinear when the cross product of (b-a) and (p-a) vanishes.
        let cross = (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x);
        let scale = (b.x - a.x).hypot(b.y - a.y).max(EPSILON);
        (cross / scale).abs() <= EPSILON
    })
}

fn ring_contains(ring: &[Coord], p: Coord) -> bool {
    let mut inside = false;
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (ring[i], ring[j]);
        // Half-open crossing test: counts each edge once, so a point on a
        // shared vertex is not double-counted.
        if (a.y > p.y) != (b.y > p.y) {
            let x_at_p = a.x + (p.y - a.y) / (b.y - a.y) * (b.x - a.x);
            if p.x < x_at_p {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// Parse an EDR `coords` value.
pub fn parse(input: &str) -> EdrResult<Geometry> {
    let text = input.trim();
    let (keyword, rest) = split_keyword(text)?;
    // EDR allows the Z/M qualifiers; the extra ordinates are simply ignored.
    let keyword = keyword
        .trim_end_matches(" ZM")
        .trim_end_matches(" Z")
        .trim_end_matches(" M")
        .trim();

    match keyword {
        "POINT" => {
            let coords = parse_coord_list(strip_parens(rest)?)?;
            match coords.as_slice() {
                [c] => Ok(Geometry::Point(*c)),
                _ => Err(bad("POINT takes exactly one position")),
            }
        }
        "MULTIPOINT" => {
            let body = strip_parens(rest)?;
            // Both MULTIPOINT(1 2, 3 4) and MULTIPOINT((1 2),(3 4)) are legal.
            let coords = if body.contains('(') {
                split_groups(body)?
                    .into_iter()
                    .map(|g| {
                        let cs = parse_coord_list(&g)?;
                        match cs.as_slice() {
                            [c] => Ok(*c),
                            _ => Err(bad("each MULTIPOINT member is one position")),
                        }
                    })
                    .collect::<EdrResult<Vec<_>>>()?
            } else {
                parse_coord_list(body)?
            };
            if coords.is_empty() {
                return Err(bad("MULTIPOINT is empty"));
            }
            Ok(Geometry::MultiPoint(coords))
        }
        "LINESTRING" => {
            let coords = parse_coord_list(strip_parens(rest)?)?;
            if coords.len() < 2 {
                return Err(bad("LINESTRING needs at least two positions"));
            }
            Ok(Geometry::LineString(coords))
        }
        "POLYGON" => {
            let rings = split_groups(strip_parens(rest)?)?
                .into_iter()
                .map(|g| parse_coord_list(&g))
                .collect::<EdrResult<Vec<Vec<Coord>>>>()?;
            if rings.is_empty() || rings[0].len() < 4 {
                return Err(bad(
                    "POLYGON needs a closed exterior ring of at least four positions",
                ));
            }
            if rings[0].first() != rings[0].last() {
                return Err(bad("POLYGON exterior ring is not closed"));
            }
            Ok(Geometry::Polygon(rings))
        }
        other => Err(EdrError::NotSupported(format!(
            "Geometry type '{other}' is not supported; use POINT, MULTIPOINT, LINESTRING or POLYGON"
        ))),
    }
}

fn bad(msg: &str) -> EdrError {
    EdrError::BadRequest(format!("Invalid coords: {msg}"))
}

fn split_keyword(text: &str) -> EdrResult<(&str, &str)> {
    let open = text
        .find('(')
        .ok_or_else(|| bad("expected a WKT geometry such as POINT(0 51.5)"))?;
    let (keyword, rest) = text.split_at(open);
    Ok((keyword.trim(), rest))
}

fn strip_parens(text: &str) -> EdrResult<&str> {
    let text = text.trim();
    text.strip_prefix('(')
        .and_then(|t| t.strip_suffix(')'))
        .map(str::trim)
        .ok_or_else(|| bad("unbalanced parentheses"))
}

/// Split `(a),(b)` into its parenthesised groups.
fn split_groups(text: &str) -> EdrResult<Vec<String>> {
    let mut groups = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for ch in text.chars() {
        match ch {
            '(' => {
                depth += 1;
                if depth == 1 {
                    current.clear();
                    continue;
                }
            }
            ')' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| bad("unbalanced parentheses"))?;
                if depth == 0 {
                    groups.push(std::mem::take(&mut current));
                    continue;
                }
            }
            _ => {}
        }
        if depth >= 1 {
            current.push(ch);
        }
    }
    if depth != 0 {
        return Err(bad("unbalanced parentheses"));
    }
    if groups.is_empty() {
        return Err(bad("expected a parenthesised coordinate list"));
    }
    Ok(groups)
}

fn parse_coord_list(text: &str) -> EdrResult<Vec<Coord>> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let mut nums = pair.split_whitespace();
            let x = nums.next().and_then(|v| v.parse::<f64>().ok());
            let y = nums.next().and_then(|v| v.parse::<f64>().ok());
            match (x, y) {
                // Trailing ordinates (z, m) are accepted and dropped.
                (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Ok(Coord { x, y }),
                _ => Err(bad(&format!("'{pair}' is not an 'x y' position"))),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_point() {
        assert_eq!(
            parse("POINT(-0.12 51.5)").unwrap(),
            Geometry::Point(Coord { x: -0.12, y: 51.5 })
        );
    }

    #[test]
    fn parses_both_multipoint_spellings() {
        let flat = parse("MULTIPOINT(0 51, 1 52)").unwrap();
        let nested = parse("MULTIPOINT((0 51),(1 52))").unwrap();
        assert_eq!(flat, nested);
        assert_eq!(flat.vertices().len(), 2);
    }

    #[test]
    fn parses_a_polygon_with_a_hole() {
        let g = parse("POLYGON((0 0, 10 0, 10 10, 0 10, 0 0),(4 4, 6 4, 6 6, 4 6, 4 4))").unwrap();
        let Geometry::Polygon(rings) = &g else {
            panic!("expected a polygon");
        };
        assert_eq!(rings.len(), 2);
        assert!(g.contains(Coord { x: 1.0, y: 1.0 }));
        assert!(!g.contains(Coord { x: 5.0, y: 5.0 }), "hole is excluded");
        assert!(!g.contains(Coord { x: 20.0, y: 5.0 }));
        assert_eq!(g.bbox(), [0.0, 0.0, 10.0, 10.0]);
        // The hole's own edge stays inside the polygon.
        assert!(g.contains(Coord { x: 4.0, y: 5.0 }));
    }

    #[test]
    fn every_corner_and_edge_of_a_rectangle_counts_as_inside() {
        let g = parse("POLYGON((-1 51, 1 51, 1 52, -1 52, -1 51))").unwrap();
        for (x, y) in [
            (-1.0, 51.0),
            (0.0, 51.0),
            (1.0, 51.0),
            (-1.0, 51.5),
            (0.0, 51.5),
            (1.0, 51.5),
            (-1.0, 52.0),
            (0.0, 52.0),
            (1.0, 52.0),
        ] {
            assert!(g.contains(Coord { x, y }), "({x}, {y}) should be inside");
        }
        assert!(!g.contains(Coord { x: 1.25, y: 51.5 }));
        assert!(!g.contains(Coord { x: 0.0, y: 52.25 }));
    }

    #[test]
    fn accepts_z_qualified_geometries_and_drops_the_ordinate() {
        assert_eq!(
            parse("POINT Z(3 4 500)").unwrap(),
            Geometry::Point(Coord { x: 3.0, y: 4.0 })
        );
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "POINT(0)",
            "POINT 0 51",
            "POLYGON((0 0, 1 0, 1 1, 0 0.5))",
            "CIRCLE(0 0, 5)",
            "",
        ] {
            assert!(parse(bad).is_err(), "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn unclosed_polygon_ring_is_rejected() {
        assert!(parse("POLYGON((0 0, 10 0, 10 10, 0 10))").is_err());
    }
}
