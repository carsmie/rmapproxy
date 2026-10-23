//! Spatial reference systems and the bbox transform MapProxy uses (`srs.py`), on the
//! pure-Rust proj4rs (as rGIS). Axis order is always x/east first (pyproj always_xy).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use anyhow::{Result, anyhow, bail};
use parking_lot::Mutex;
use proj4rs::Proj;

pub type BBox = [f64; 4];

#[derive(Clone)]
pub struct Srs {
    /// Normalised `EPSG:nnnn` code.
    pub code: String,
    pub geographic: bool,
    proj: Arc<Proj>,
}

impl std::fmt::Debug for Srs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.code)
    }
}

impl PartialEq for Srs {
    fn eq(&self, o: &Self) -> bool {
        self.code == o.code
    }
}

static CACHE: LazyLock<Mutex<HashMap<String, Srs>>> = LazyLock::new(Default::default);

/// MapProxy treats these as one SRS (`WEBMERCATOR_EPSG`).
const WEBMERCATOR: [&str; 3] = ["EPSG:900913", "EPSG:3857", "EPSG:102100"];

impl Srs {
    pub fn new(code: &str) -> Result<Srs> {
        let mut code = code.trim().to_ascii_uppercase();
        if WEBMERCATOR.contains(&code.as_str()) {
            code = "EPSG:3857".into();
        }
        if let Some(s) = CACHE.lock().get(&code) {
            return Ok(s.clone());
        }
        let num: u16 = code
            .strip_prefix("EPSG:")
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| anyhow!("unsupported SRS '{code}'"))?;
        let def = crs_definitions::from_code(num).ok_or_else(|| anyhow!("unknown SRS '{code}'"))?;
        let proj = Proj::from_proj_string(def.proj4)
            .map_err(|e| anyhow!("invalid proj string for {code}: {e:?}"))?;
        let srs = Srs {
            geographic: proj.is_latlong(),
            code: code.clone(),
            proj: Arc::new(proj),
        };
        CACHE.lock().insert(code, srs.clone());
        Ok(srs)
    }

    pub fn transform_point(&self, to: &Srs, x: f64, y: f64) -> Result<(f64, f64)> {
        if self == to {
            return Ok((x, y));
        }
        let mut p = if self.geographic {
            (x.to_radians(), y.clamp(-90.0, 90.0).to_radians())
        } else {
            (x, y)
        };
        proj4rs::transform::transform(&self.proj, &to.proj, &mut p)
            .map_err(|e| anyhow!("transform {} -> {} failed: {e:?}", self.code, to.code))?;
        if to.geographic {
            p = (p.0.to_degrees(), p.1.to_degrees());
        }
        if !p.0.is_finite() || !p.1.is_finite() {
            bail!("non-finite transform result {} -> {}", self.code, to.code);
        }
        Ok(p)
    }

    /// `_SRS.transform_bbox_to(other, bbox, with_points=16)`: envelope of 16 transformed
    /// outline points.
    pub fn transform_bbox(&self, to: &Srs, bbox: BBox) -> Result<BBox> {
        if self == to {
            return Ok(bbox);
        }
        let mut out = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for (x, y) in envelope_points(bbox, 16) {
            let (tx, ty) = self.transform_point(to, x, y)?;
            out[0] = out[0].min(tx);
            out[1] = out[1].min(ty);
            out[2] = out[2].max(tx);
            out[3] = out[3].max(ty);
        }
        if self.code == "EPSG:4326" && to.code == "EPSG:3857" {
            // srs.py keeps the old clamped world extent for 4326 -> 3857.
            const M: f64 = 20037508.342789244;
            if bbox[0] <= -180.0 {
                out[0] = -M;
            }
            if bbox[1] <= -85.06 {
                out[1] = -M;
            }
            if bbox[2] >= 180.0 {
                out[2] = M;
            }
            if bbox[3] >= 85.06 {
                out[3] = M;
            }
        }
        Ok(out)
    }
}

/// `generate_envelope_points(bbox, n)`: a ring of points around the bbox.
pub fn envelope_points(bbox: BBox, n: usize) -> Vec<(f64, f64)> {
    let (mut minx, mut miny, mut maxx, mut maxy) = (bbox[0], bbox[1], bbox[2], bbox[3]);
    let steps = if n <= 4 { 0 } else { (n - 4).div_ceil(4) } + 1;
    let (width, height) = (maxx - minx, maxy - miny);
    (minx, maxx) = (minx.min(maxx), minx.max(maxx));
    (miny, maxy) = (miny.min(maxy), miny.max(maxy));
    let (xstep, ystep) = (width / steps as f64, height / steps as f64);
    let mut r = Vec::with_capacity(4 * steps);
    for i in 0..=steps {
        r.push((minx + i as f64 * xstep, miny));
    }
    for i in 1..steps {
        r.push((maxx, miny + i as f64 * ystep));
    }
    for i in (0..=steps).rev() {
        r.push((minx + i as f64 * xstep, maxy));
    }
    for i in (1..steps).rev() {
        r.push((minx, miny + i as f64 * ystep));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_matches_mapproxy_doctest() {
        assert_eq!(
            envelope_points([10.0, 5.0, 20.0, 15.0], 4),
            vec![(10.0, 5.0), (20.0, 5.0), (20.0, 15.0), (10.0, 15.0)]
        );
        assert_eq!(envelope_points([10.0, 5.0, 20.0, 15.0], 8).len(), 8);
    }

    #[test]
    fn utm33_roundtrip() {
        let (g, u) = (Srs::new("EPSG:4326").unwrap(), Srs::new("EPSG:25833").unwrap());
        let (x, y) = g.transform_point(&u, 15.0, 60.0).unwrap();
        assert!((x - 500000.0).abs() < 1e-6, "{x}");
        let (lon, lat) = u.transform_point(&g, x, y).unwrap();
        assert!((lon - 15.0).abs() < 1e-9 && (lat - 60.0).abs() < 1e-9);
    }
}
