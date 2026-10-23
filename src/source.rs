//! WMS sources (`source/wms.py`, `client/wms.py`): resolution range, coverage, coverage
//! sub-requests and WMS 1.1.1 GetMap, with `on_error` → transparent and uncacheable.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tracing::{debug, warn};

use crate::config::SourceConf;
use crate::image::{MergeMode, Rgba, decode_png};
use crate::pyfloat::int;
use crate::srs::{BBox, Srs};

/// What a source contributes to a merge. Responses stay encoded until merged, so a
/// metatile holds at most one decoded source image at a time.
pub enum SourceImage {
    /// `BlankImageError`: outside resolution range or coverage — not merged at all.
    Skip,
    /// `on_error: transparent`: a fully transparent layer, not cacheable.
    Transparent,
    /// A PNG of `size`; with `place`, a coverage sub-image to paste at that offset into a
    /// transparent canvas of the metatile size (`sub_image_result`).
    Png { data: Vec<u8>, size: (u32, u32), place: Option<(i64, i64)> },
}

impl SourceImage {
    /// Decode for merging: the image (metatile-sized) and how Pillow would merge it.
    pub fn decode(data: &[u8], size: (u32, u32), place: Option<(i64, i64)>, canvas: (u32, u32)) -> Result<(Rgba, MergeMode)> {
        let (img, mode) = decode_png(data)?;
        if (img.w, img.h) != size {
            bail!("got {}x{}, asked {}x{}", img.w, img.h, size.0, size.1);
        }
        Ok(match place {
            None => (img, mode),
            Some((x, y)) => {
                let mut c = Rgba::blank(canvas.0, canvas.1);
                c.paste(&img, x, y);
                (c, MergeMode::Composite)
            }
        })
    }
}

pub struct WmsSource {
    pub conf: SourceConf,
    client: reqwest::Client,
}

impl WmsSource {
    pub fn new(conf: SourceConf, client: reqwest::Client) -> Self {
        WmsSource { conf, client }
    }

    /// `ResolutionRange.contains(bbox, size, srs)`.
    fn res_in_range(&self, bbox: BBox, size: (u32, u32), srs: &Srs) -> bool {
        let (mut w, mut h) = (bbox[2] - bbox[0], bbox[3] - bbox[1]);
        if srs.geographic {
            // deg_to_m with the WGS84 semi-major axis.
            let m = 2.0 * std::f64::consts::PI * 6378137.0 / 360.0;
            w *= m;
            h *= m;
        }
        let (xr, yr) = (w / size.0 as f64, h / size.1 as f64);
        if let Some(min) = self.conf.min_res {
            let min = min + 1e-6;
            if min <= xr || min <= yr {
                return false;
            }
        }
        if let Some(max) = self.conf.max_res {
            if max > xr || max > yr {
                return false;
            }
        }
        true
    }

    pub async fn get_map(&self, bbox: BBox, size: (u32, u32), srs: &Srs) -> Result<SourceImage> {
        if (self.conf.min_res.is_some() || self.conf.max_res.is_some()) && !self.res_in_range(bbox, size, srs) {
            return Ok(SourceImage::Skip);
        }
        if let Some(cov) = &self.conf.coverage {
            if !cov.intersects(bbox, srs)? {
                return Ok(SourceImage::Skip);
            }
        }
        match self.fetch(bbox, size, srs).await {
            Ok(r) => Ok(r),
            Err(e) if self.conf.on_error_transparent => {
                warn!("source {}: {e:#}; transparent, not cached", self.conf.name);
                Ok(SourceImage::Transparent)
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch(&self, bbox: BBox, size: (u32, u32), srs: &Srs) -> Result<SourceImage> {
        if !self.conf.supported_srs.is_empty() && !self.conf.supported_srs.contains(srs) {
            bail!("SRS {} not supported by source (reprojection is out of scope)", srs.code);
        }
        if let Some(cov) = &self.conf.coverage {
            if !cov.extent_contains(bbox, srs)? {
                // `_get_sub_query`: request only the part inside the coverage extent.
                let (sub_size, off, sub_bbox) = bbox_position_in_image(bbox, size, cov.extent_bbox_for(srs)?);
                if sub_size.0 == 0 || sub_size.1 == 0 {
                    return Ok(SourceImage::Skip);
                }
                // sub_image_result: pasted into a transparent canvas, composited when merged.
                let data = self.request(sub_bbox, sub_size, srs).await?;
                return Ok(SourceImage::Png { data, size: sub_size, place: Some(off) });
            }
        }
        let data = self.request(bbox, size, srs).await?;
        Ok(SourceImage::Png { data, size, place: None })
    }

    pub fn getmap_url(&self, bbox: BBox, size: (u32, u32), srs: &Srs) -> String {
        let mut q: Vec<(String, String)> = vec![
            ("service".into(), "WMS".into()),
            ("request".into(), "GetMap".into()),
            ("version".into(), "1.1.1".into()),
            ("styles".into(), String::new()),
        ];
        let mut has_format = false;
        for (k, v) in &self.conf.params {
            has_format |= k == "format";
            q.push((k.clone(), v.clone()));
        }
        if !has_format {
            q.push(("format".into(), "image/png".into()));
        }
        q.push(("srs".into(), srs.code.clone()));
        q.push(("bbox".into(), bbox.iter().map(|v| pyfloat_repr(*v)).collect::<Vec<_>>().join(",")));
        q.push(("width".into(), size.0.to_string()));
        q.push(("height".into(), size.1.to_string()));
        let qs: Vec<String> = q
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencode(v)))
            .collect();
        let sep = if self.conf.url.contains('?') { '&' } else { '?' };
        format!("{}{}{}", self.conf.url, sep, qs.join("&"))
    }

    async fn request(&self, bbox: BBox, size: (u32, u32), srs: &Srs) -> Result<Vec<u8>> {
        let url = self.getmap_url(bbox, size, srs);
        debug!("source {}: {url}", self.conf.name);
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs_f64(self.conf.client_timeout))
            .send()
            .await
            .map_err(|e| anyhow!("{}: {e}", self.conf.name))?;
        let status = resp.status();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.bytes().await?;
        if !status.is_success() {
            bail!("HTTP {status} from {url}: {}", String::from_utf8_lossy(&body[..body.len().min(200)]));
        }
        if !ctype.starts_with("image/") {
            bail!("no image from {url} ({ctype}): {}", String::from_utf8_lossy(&body[..body.len().min(200)]));
        }
        Ok(body.to_vec())
    }
}

/// `image.bbox_position_in_image(bbox, size, src_bbox)`: sub-image size, top-left offset
/// and sub-bbox of the part of `bbox` covered by `src_bbox`.
pub fn bbox_position_in_image(bbox: BBox, size: (u32, u32), src: BBox) -> ((u32, u32), (i64, i64), BBox) {
    let (w, h) = (size.0 as f64, size.1 as f64);
    // make_lin_transf(bbox, (0, 0, w, h))
    let px = |x: f64| (x - bbox[0]) * w / (bbox[2] - bbox[0]);
    let py = |y: f64| (bbox[3] - y) * h / (bbox[3] - bbox[1]);
    let mut off = [0i64, size.1 as i64, size.0 as i64, 0];
    let mut sub = bbox;
    if src[0] > bbox[0] {
        sub[0] = src[0];
        off[0] = int(px(src[0]));
    }
    if src[1] > bbox[1] {
        sub[1] = src[1];
        off[1] = int(py(src[1]));
    }
    if src[2] < bbox[2] {
        sub[2] = src[2];
        off[2] = int(px(src[2]));
    }
    if src[3] < bbox[3] {
        sub[3] = src[3];
        off[3] = int(py(src[3]));
    }
    let s = ((off[2] - off[0]).unsigned_abs() as u32, (off[1] - off[3]).unsigned_abs() as u32);
    (s, (off[0], off[3]), sub)
}

/// Python `str(float)`: shortest round-trip repr, always with a fraction or exponent.
fn pyfloat_repr(v: f64) -> String {
    let s = format!("{v:?}");
    if s.contains(['.', 'e', 'E', 'n', 'i']) { s } else { format!("{s}.0") }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_matches_mapproxy_doctests() {
        assert_eq!(
            bbox_position_in_image([-180.0, -90.0, 180.0, 90.0], (600, 300), [-180.0, -90.0, 180.0, 90.0]),
            ((600, 300), (0, 0), [-180.0, -90.0, 180.0, 90.0])
        );
        assert_eq!(
            bbox_position_in_image([-200.0, -100.0, 200.0, 100.0], (600, 300), [-180.0, -90.0, 180.0, 90.0]),
            ((540, 270), (30, 15), [-180.0, -90.0, 180.0, 90.0])
        );
        assert_eq!(
            bbox_position_in_image([-200.0, -50.0, 200.0, 100.0], (600, 300), [-180.0, -90.0, 180.0, 90.0]),
            ((540, 280), (30, 20), [-180.0, -50.0, 180.0, 90.0])
        );
        assert_eq!(
            bbox_position_in_image(
                [586400.0, 196400.0, 752800.0, 362800.0],
                (256, 256),
                [586400.0, 196400.0, 752800.0, 350000.0]
            ),
            ((256, 237), (0, 19), [586400.0, 196400.0, 752800.0, 350000.0])
        );
    }

    #[test]
    fn float_repr_like_python() {
        assert_eq!(pyfloat_repr(-2500000.0), "-2500000.0");
        assert_eq!(pyfloat_repr(0.1), "0.1");
    }
}
