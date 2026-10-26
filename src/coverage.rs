//! Source/seed coverages (`util/coverage.py`, `config/coverage.py`): a bbox or GeoJSON
//! polygons in their own SRS. MapProxy simplifies datasource polygons with a tolerance of
//! 1e-6 of their extent; tnt-mapproxy's coverage files are rectangles or only used for
//! seeding, so the raw polygons are used here.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_yaml::Value;

use crate::srs::{BBox, Srs};

type Ring = Vec<(f64, f64)>;

#[derive(Debug)]
pub struct Coverage {
    pub srs: Srs,
    pub bbox: BBox,
    /// Polygons as rings (first = exterior); empty for a bbox coverage.
    polygons: Vec<Vec<Ring>>,
}

impl Coverage {
    pub fn from_conf(c: &Value, abspath: &dyn Fn(&str) -> PathBuf) -> Result<Coverage> {
        if c.get("clip").and_then(|v| v.as_bool()) == Some(true) {
            bail!("coverage clip: true is not supported");
        }
        let srs_of = |k: &str| c.get(k).and_then(|s| s.as_str());
        if let Some(b) = c.get("bbox") {
            let srs = Srs::new(srs_of("bbox_srs").or(srs_of("srs")).ok_or_else(|| anyhow!("coverage without srs"))?)?;
            return Ok(Coverage { srs, bbox: parse_bbox(b)?, polygons: vec![] });
        }
        let ds = c.get("datasource").ok_or_else(|| anyhow!("coverage needs bbox or datasource"))?;
        let srs = Srs::new(srs_of("srs").ok_or_else(|| anyhow!("coverage without srs"))?)?;
        if ds.is_sequence() {
            return Ok(Coverage { srs, bbox: parse_bbox(ds)?, polygons: vec![] });
        }
        let ds = ds.as_str().ok_or_else(|| anyhow!("bad datasource"))?;
        if let Ok(b) = parse_bbox(&Value::String(ds.into())) {
            return Ok(Coverage { srs, bbox: b, polygons: vec![] });
        }
        Coverage::from_geojson(&abspath(ds), srs)
    }

    pub fn from_geojson(path: &Path, srs: Srs) -> Result<Coverage> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let j: serde_json::Value = serde_json::from_str(&text)?;
        let mut geoms = vec![];
        match j["type"].as_str() {
            Some("FeatureCollection") => {
                for f in j["features"].as_array().into_iter().flatten() {
                    geoms.push(f["geometry"].clone());
                }
            }
            Some("Feature") => geoms.push(j["geometry"].clone()),
            Some("Polygon" | "MultiPolygon") => geoms.push(j.clone()),
            t => bail!("{}: unsupported GeoJSON type {t:?}", path.display()),
        }
        let ring = |r: &serde_json::Value| -> Ring {
            r.as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| Some((p[0].as_f64()?, p[1].as_f64()?)))
                .collect()
        };
        let poly = |p: &serde_json::Value| -> Vec<Ring> { p.as_array().into_iter().flatten().map(ring).collect() };
        let mut polygons = vec![];
        for g in geoms {
            match g["type"].as_str() {
                Some("Polygon") => polygons.push(poly(&g["coordinates"])),
                Some("MultiPolygon") => {
                    for p in g["coordinates"].as_array().into_iter().flatten() {
                        polygons.push(poly(p));
                    }
                }
                _ => {}
            }
        }
        let mut bbox = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for &(x, y) in polygons.iter().filter_map(|p| p.first()).flatten() {
            bbox = [bbox[0].min(x), bbox[1].min(y), bbox[2].max(x), bbox[3].max(y)];
        }
        if polygons.is_empty() {
            bail!("{}: no polygons", path.display());
        }
        Ok(Coverage { srs, bbox, polygons })
    }

    /// `Coverage.transform_to(srs)`: a bbox coverage by its 16-point envelope, polygons
    /// vertex by vertex (`transform_geometry`, no densification).
    pub fn transform_to(&self, srs: &Srs) -> Result<Coverage> {
        if *srs == self.srs {
            return Ok(Coverage { srs: self.srs.clone(), bbox: self.bbox, polygons: self.polygons.clone() });
        }
        if self.polygons.is_empty() {
            return Ok(Coverage { srs: srs.clone(), bbox: self.srs.transform_bbox(srs, self.bbox)?, polygons: vec![] });
        }
        let mut polygons = Vec::with_capacity(self.polygons.len());
        for p in &self.polygons {
            let rings = p
                .iter()
                .map(|r| r.iter().map(|&(x, y)| self.srs.transform_point(srs, x, y)).collect::<Result<Ring>>())
                .collect::<Result<Vec<Ring>>>()?;
            polygons.push(rings);
        }
        let mut bbox = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for &(x, y) in polygons.iter().filter_map(|p| p.first()).flatten() {
            bbox = [bbox[0].min(x), bbox[1].min(y), bbox[2].max(x), bbox[3].max(y)];
        }
        Ok(Coverage { srs: srs.clone(), bbox, polygons })
    }

    fn bbox_in_coverage_srs(&self, bbox: BBox, srs: &Srs) -> Result<BBox> {
        srs.transform_bbox(&self.srs, bbox)
    }

    /// `Coverage.intersects(bbox, srs)` (shapely semantics: touching counts).
    pub fn intersects(&self, bbox: BBox, srs: &Srs) -> Result<bool> {
        let b = self.bbox_in_coverage_srs(bbox, srs)?;
        if self.polygons.is_empty() {
            // BBOXCoverage: strict, touching does not count.
            return Ok(bbox_intersects(self.bbox, b));
        }
        // GeomCoverage (shapely intersects, touching counts); the bbox test only prefilters.
        let touching = !(self.bbox[0] > b[2] || self.bbox[2] < b[0] || self.bbox[1] > b[3] || self.bbox[3] < b[1]);
        Ok(touching && self.polygons.iter().any(|p| polygon_intersects_rect(p, b)))
    }

    /// `Coverage.contains(bbox, srs)`: the bbox lies completely inside the coverage.
    pub fn contains(&self, bbox: BBox, srs: &Srs) -> Result<bool> {
        let b = self.bbox_in_coverage_srs(bbox, srs)?;
        if self.polygons.is_empty() {
            return Ok(bbox_contains(self.bbox, b));
        }
        let corners = [(b[0], b[1]), (b[2], b[1]), (b[2], b[3]), (b[0], b[3])];
        Ok(self.polygons.iter().any(|p| {
            corners.iter().all(|&(x, y)| point_in_polygon(p, x, y))
                // no polygon boundary (outer or hole) passes through the rect interior
                && !p.iter().flatten().any(|&(x, y)| x > b[0] && x < b[2] && y > b[1] && y < b[3])
                && !p.iter().any(|ring| {
                    (0..ring.len()).any(|i| {
                        let (a, c) = (ring[i], ring[(i + 1) % ring.len()]);
                        (0..4).any(|k| proper_cross(a, c, corners[k], corners[(k + 1) % 4]))
                    })
                })
        }))
    }

    /// `MapExtent.contains(MapExtent(bbox, srs))`: the query bbox, transformed into the
    /// coverage SRS, lies within the coverage bbox (with MapProxy's tiny tolerance).
    pub fn extent_contains(&self, bbox: BBox, srs: &Srs) -> Result<bool> {
        Ok(bbox_contains(self.bbox, self.bbox_in_coverage_srs(bbox, srs)?))
    }

    /// `MapExtent.bbox_for(srs)`.
    pub fn extent_bbox_for(&self, srs: &Srs) -> Result<BBox> {
        self.srs.transform_bbox(srs, self.bbox)
    }
}

fn parse_bbox(v: &Value) -> Result<BBox> {
    let vals: Vec<f64> = match v {
        Value::Sequence(s) => s
            .iter()
            .map(|x| x.as_f64().or_else(|| x.as_i64().map(|i| i as f64)))
            .collect::<Option<_>>()
            .ok_or_else(|| anyhow!("bad bbox"))?,
        Value::String(s) => s.split(',').map(|x| x.trim().parse::<f64>()).collect::<Result<_, _>>()?,
        _ => bail!("bad bbox"),
    };
    if vals.len() != 4 {
        bail!("bbox needs 4 values");
    }
    Ok([vals[0], vals[1], vals[2], vals[3]])
}

/// `util/bbox.py::bbox_intersects` (strict).
pub fn bbox_intersects(a: BBox, b: BBox) -> bool {
    a[0] < b[2] && a[2] > b[0] && a[1] < b[3] && a[3] > b[1]
}

/// `util/bbox.py::bbox_contains`: `one` contains `two`, tolerance extent/1e13.
pub fn bbox_contains(one: BBox, two: BBox) -> bool {
    let xd = (one[2] - one[0]).abs() / 10e12;
    let yd = (one[3] - one[1]).abs() / 10e12;
    one[0] <= two[0] + xd && one[2] >= two[2] - xd && one[1] <= two[1] + yd && one[3] >= two[3] - yd
}

fn point_in_polygon(p: &[Ring], x: f64, y: f64) -> bool {
    let inside = |r: &Ring| {
        let mut c = false;
        let n = r.len();
        for i in 0..n {
            let (xi, yi) = r[i];
            let (xj, yj) = r[(i + n - 1) % n];
            if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                c = !c;
            }
        }
        c
    };
    match p.split_first() {
        Some((outer, holes)) => inside(outer) && !holes.iter().any(inside),
        None => false,
    }
}

fn segments_intersect(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let orient = |p: (f64, f64), q: (f64, f64), r: (f64, f64)| (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0);
    let on = |p: (f64, f64), q: (f64, f64), r: (f64, f64)| {
        r.0 >= p.0.min(q.0) && r.0 <= p.0.max(q.0) && r.1 >= p.1.min(q.1) && r.1 <= p.1.max(q.1)
    };
    let (o1, o2, o3, o4) = (orient(a, b, c), orient(a, b, d), orient(c, d, a), orient(c, d, b));
    if ((o1 > 0.0 && o2 < 0.0) || (o1 < 0.0 && o2 > 0.0)) && ((o3 > 0.0 && o4 < 0.0) || (o3 < 0.0 && o4 > 0.0)) {
        return true;
    }
    (o1 == 0.0 && on(a, b, c)) || (o2 == 0.0 && on(a, b, d)) || (o3 == 0.0 && on(c, d, a)) || (o4 == 0.0 && on(c, d, b))
}

/// Segments cross at a single interior point of both (not touching).
fn proper_cross(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let orient = |p: (f64, f64), q: (f64, f64), r: (f64, f64)| (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0);
    let (o1, o2, o3, o4) = (orient(a, b, c), orient(a, b, d), orient(c, d, a), orient(c, d, b));
    o1 * o2 < 0.0 && o3 * o4 < 0.0
}

fn polygon_intersects_rect(p: &[Ring], r: BBox) -> bool {
    let in_rect = |&(x, y): &(f64, f64)| x >= r[0] && x <= r[2] && y >= r[1] && y <= r[3];
    if p.first().is_some_and(|outer| outer.iter().any(in_rect)) {
        return true;
    }
    let corners = [(r[0], r[1]), (r[2], r[1]), (r[2], r[3]), (r[0], r[3])];
    if corners.iter().any(|&(x, y)| point_in_polygon(p, x, y)) {
        return true;
    }
    let edges: Vec<_> = (0..4).map(|i| (corners[i], corners[(i + 1) % 4])).collect();
    p.iter().any(|ring| {
        (0..ring.len()).any(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
            edges.iter().any(|&(c, d)| segments_intersect(a, b, c, d))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_polygon_intersection() {
        let tri = vec![vec![(0.0, 0.0), (10.0, 0.0), (0.0, 10.0), (0.0, 0.0)]];
        assert!(polygon_intersects_rect(&tri, [1.0, 1.0, 2.0, 2.0])); // inside
        assert!(polygon_intersects_rect(&tri, [-5.0, -5.0, 20.0, 20.0])); // contains
        assert!(polygon_intersects_rect(&tri, [4.0, 4.0, 8.0, 8.0])); // crosses hypotenuse
        assert!(!polygon_intersects_rect(&tri, [6.0, 6.0, 8.0, 8.0])); // beyond hypotenuse
        assert!(polygon_intersects_rect(&tri, [10.0, -1.0, 12.0, 0.0])); // touches corner
    }

    #[test]
    fn contains_tolerance_matches_doctest() {
        assert!(!bbox_contains([0.0, 0.0, 10.0, 10.0], [0.000001, 0.0000001, 10.000001, 10.000001]));
        assert!(bbox_contains([0.0, 0.0, 10.0, 10.0], [1e-13, 1e-13, 10.0000000000001, 10.0000000000001]));
    }
}
