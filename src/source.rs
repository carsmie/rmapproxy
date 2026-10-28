//! WMS sources (`source/wms.py`, `client/wms.py`): resolution range, coverage, coverage
//! sub-requests and WMS 1.1.1 GetMap, with `on_error` → transparent and uncacheable.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tracing::{debug, warn};

use crate::config::SourceConf;
use crate::image::{MergeMode, Rgba, decode_png, decode_png_typed};
use crate::pyfloat::int;
use crate::srs::{BBox, Srs};
use crate::transform::{mesh_warp, transform_meshes};

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
    /// A reprojected image, metatile-sized, ready to merge.
    Decoded(Rgba, MergeMode),
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
    /// The source's `image_opts.transparent` (from `req.transparent`).
    fn transparent(&self) -> bool {
        self.conf.params.iter().any(|(k, v)| k == "transparent" && v.eq_ignore_ascii_case("true"))
    }

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
        if let Some(max) = self.conf.max_res && (max > xr || max > yr) {
            return false;
        }
        true
    }

    pub async fn get_map(&self, bbox: BBox, size: (u32, u32), srs: &Srs) -> Result<SourceImage> {
        if (self.conf.min_res.is_some() || self.conf.max_res.is_some()) && !self.res_in_range(bbox, size, srs) {
            return Ok(SourceImage::Skip);
        }
        if let Some(cov) = &self.conf.coverage && !cov.intersects(bbox, srs)? {
            return Ok(SourceImage::Skip);
        }
        let needs_transform = !self.conf.supported_srs.is_empty() && !self.conf.supported_srs.contains(srs);
        let r = if needs_transform { self.fetch_transformed(bbox, size, srs).await } else { self.fetch(bbox, size, srs).await };
        match r {
            Ok(r) => Ok(r),
            Err(e) if self.conf.on_error_transparent => {
                warn!("source {}: {e:#}; transparent, not cached", self.conf.name);
                Ok(SourceImage::Transparent)
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch(&self, bbox: BBox, size: (u32, u32), srs: &Srs) -> Result<SourceImage> {
        if let Some(cov) = &self.conf.coverage && !cov.extent_contains(bbox, srs)? {
            // `_get_sub_query`: request only the part inside the coverage extent.
            let (sub_size, off, sub_bbox) = bbox_position_in_image(bbox, size, cov.extent_bbox_for(srs)?);
            if sub_size.0 == 0 || sub_size.1 == 0 {
                return Ok(SourceImage::Skip);
            }
            // sub_image_result: pasted into a transparent canvas, composited when merged.
            let data = self.request(sub_bbox, sub_size, srs).await?;
            return Ok(SourceImage::Png { data, size: sub_size, place: Some(off) });
        }
        let data = self.request(bbox, size, srs).await?;
        Ok(SourceImage::Png { data, size, place: None })
    }

    /// `_get_transformed`: request the best supported SRS over the transformed bbox, then
    /// mesh-warp into the query SRS.
    async fn fetch_transformed(&self, bbox: BBox, size: (u32, u32), dst: &Srs) -> Result<SourceImage> {
        let src_srs = best_srs(dst, &self.conf.supported_srs);
        let src_bbox = dst.transform_bbox(&src_srs, bbox)?;
        let (w, h) = (src_bbox[2] - src_bbox[0], src_bbox[3] - src_bbox[1]);
        let ratio = w / h;
        let (xres, yres) = (w / size.0 as f64, h / size.1 as f64);
        let src_size = if xres < yres {
            (size.0, (size.0 as f64 / ratio + 0.5) as u32)
        } else {
            ((size.1 as f64 * ratio + 0.5) as u32, size.1)
        };
        // Polygon `contains` (not the extent test of the untransformed path) picks the
        // sub-query, whose result is always RGBA (pasted onto a transparent canvas).
        let (img, alpha, mode) = match &self.conf.coverage {
            Some(cov) if !cov.contains(src_bbox, &src_srs)? => {
                let (sub_size, off, sub_bbox) = bbox_position_in_image(src_bbox, src_size, cov.extent_bbox_for(&src_srs)?);
                if sub_size.0 == 0 || sub_size.1 == 0 {
                    return Ok(SourceImage::Skip);
                }
                let data = self.request(sub_bbox, sub_size, &src_srs).await?;
                let (img, _) = tokio::task::block_in_place(|| SourceImage::decode(&data, sub_size, Some(off), src_size))?;
                (img, true, MergeMode::Composite)
            }
            _ => {
                let data = self.request(src_bbox, src_size, &src_srs).await?;
                let (mut img, mode, ctype) = tokio::task::block_in_place(|| decode_png_typed(&data))?;
                if (img.w, img.h) != src_size {
                    bail!("got {}x{}, asked {}x{}", img.w, img.h, src_size.0, src_size.1);
                }
                match ctype {
                    png::ColorType::Rgba => (img, true, mode),
                    // LA: warped premultiplied (La), pasted when merged.
                    png::ColorType::GrayscaleAlpha => (img, true, mode),
                    // ImageResult.as_image: P -> RGBA for transparent sources; otherwise
                    // img_for_resampling converts P to its palette mode, RGB: tRNS is lost.
                    png::ColorType::Indexed if self.transparent() => (img, true, mode),
                    png::ColorType::Indexed => {
                        img.px.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[3] = 255);
                        (img, false, MergeMode::Paste)
                    }
                    // RGB/L; a tRNS colour key would be re-applied to warped pixels by
                    // MapProxy — not replicated (rGIS/QGIS serve RGBA).
                    _ => (img, false, MergeMode::Paste),
                }
            }
        };
        let out = tokio::task::block_in_place(|| -> Result<Rgba> {
            let meshes = transform_meshes(src_size, src_bbox, &src_srs, size, bbox, dst)?;
            Ok(mesh_warp(&img, alpha, size, &meshes))
        })?;
        Ok(SourceImage::Decoded(out, mode))
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

/// `SupportedSRS.best_srs` without `preferred_src_proj`: the target itself, else the first
/// with the same latlong-ness, else the first.
pub fn best_srs(target: &Srs, supported: &[Srs]) -> Srs {
    if supported.contains(target) {
        return target.clone();
    }
    supported.iter().find(|s| s.geographic == target.geographic).unwrap_or(&supported[0]).clone()
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
