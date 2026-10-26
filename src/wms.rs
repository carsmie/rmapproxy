//! WMS 1.1.1 / 1.3.0 GetMap and GetCapabilities from the caches (`service/wms.py`,
//! `layer/cache_map_layer.py`, `layer/srs_conditional.py`, `image/tile.py`, `image/merge.py`).
//! Pixels follow MapProxy: grid per request SRS, closest level, merged tiles resampled with
//! Pillow's bilinear EXTENT / MESH, layers composited onto the request background.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::config::WmsConf;
use crate::coverage::bbox_contains;
use crate::image::{Rgba, decode_png, encode_png_rgba};
use crate::pyfloat::round_int;
use crate::source::{bbox_position_in_image, best_srs};
use crate::srs::{BBox, Srs};
use crate::tile::{CacheGrid, TileManager};
use crate::transform::{extent_warp, mesh_warp, transform_meshes};

/// `TileGrid` defaults (`stretch_factor`, `max_shrink_factor`), `cache.max_tile_limit`.
const STRETCH_FACTOR: f64 = 1.15;
const MAX_SHRINK_FACTOR: f64 = 4.0;
const MAX_TILE_LIMIT: usize = 500;

pub struct Wms {
    conf: WmsConf,
    layers: Vec<WmsLayer>,
    mgr: Arc<TileManager>,
}

struct WmsLayer {
    name: String,
    title: String,
    abstract_: String,
    /// One `SRSConditional` per cache source, bottom first.
    maps: Vec<CacheLayer>,
}

/// A cache as WMS map layer: its grids in config order (`SRSConditional`) and the merged
/// source resolution range (`merge_layer_res_ranges`; None = unbounded).
struct CacheLayer {
    /// Grid and its `CacheMapLayer.extent`.
    grids: Vec<(Arc<CacheGrid>, Extent)>,
    res_range: Option<(Option<f64>, Option<f64>)>,
}

/// `MapExtent` (bbox in its SRS).
#[derive(Clone)]
struct Extent {
    bbox: BBox,
    srs: Srs,
}

impl Extent {
    fn bbox_for(&self, srs: &Srs) -> Result<BBox> {
        self.srs.transform_bbox(srs, self.bbox)
    }

    fn llbbox(&self) -> Result<BBox> {
        self.bbox_for(&Srs::new("EPSG:4326")?)
    }
}

/// `merge_layer_extents`: None = DefaultMapExtent; two real extents merge as the union of
/// their lon/lat bboxes (`MapExtent.__add__`). The last one is the starting point.
fn merge_extents(mut exts: Vec<Option<Extent>>) -> Result<Option<Extent>> {
    let Some(mut acc) = exts.pop() else { return Ok(None) };
    for e in exts {
        acc = match (acc, e) {
            (a, None) => a,
            (None, b) => b,
            (Some(a), Some(b)) => {
                let (x, y) = (a.llbbox()?, b.llbbox()?);
                Some(Extent { bbox: [x[0].min(y[0]), x[1].min(y[1]), x[2].max(y[2]), x[3].max(y[3])], srs: Srs::new("EPSG:4326")? })
            }
        };
    }
    Ok(acc)
}

/// A layer or merge result: an unmodified cache tile (served as-is, like MapProxy's file
/// buffer) or decoded RGBA pixels.
enum Img {
    Raw(Vec<u8>),
    Pix(Rgba),
}

impl Img {
    fn pixels(self) -> Result<Rgba> {
        match self {
            Img::Raw(d) => Ok(decode_png(&d)?.0),
            Img::Pix(p) => Ok(p),
        }
    }
}

struct Query {
    bbox: BBox,
    size: (u32, u32),
    srs: Srs,
}

pub struct WmsError {
    code: &'static str,
    text: String,
    version: Version,
}

#[derive(Clone, Copy, PartialEq)]
enum Version {
    V111,
    V130,
}

impl Wms {
    pub fn new(mgr: Arc<TileManager>) -> Option<Wms> {
        let conf = mgr.cfg.wms.clone()?;
        let mut layers = vec![];
        for l in &mgr.cfg.layers {
            let mut maps = vec![];
            for src in &l.sources {
                let Some(c) = mgr.cfg.caches.get(src) else { continue };
                let cgs: Vec<_> = c.grids.iter().filter_map(|g| mgr.cache_grid(&c.name, g)).collect();
                let Some(first) = cgs.first().cloned() else { continue };
                // Source extents (coverage or default), else the grid's own bbox.
                let src_ext: Vec<Option<Extent>> = first
                    .sources
                    .iter()
                    .map(|s| s.conf.coverage.as_ref().map(|c| Extent { bbox: c.bbox, srs: c.srs.clone() }))
                    .collect();
                let merged = match merge_extents(src_ext) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("WMS layer {}: {e:#}", l.name);
                        continue;
                    }
                };
                let grids: Vec<_> = cgs
                    .into_iter()
                    .map(|g| {
                        let e = merged.clone().unwrap_or_else(|| Extent { bbox: g.meta.grid.bbox, srs: g.meta.grid.srs.clone() });
                        (g, e)
                    })
                    .collect();
                let res_range = first.sources.iter().map(|s| (s.conf.min_res, s.conf.max_res)).try_fold(None, |acc: Option<(Option<f64>, Option<f64>)>, r| {
                    // resolution_range(None, None) is None: one unbounded source unbounds all.
                    if r == (None, None) {
                        return None;
                    }
                    Some(Some(match acc {
                        None => r,
                        Some(a) => (a.0.zip(r.0).map(|(x, y)| x.max(y)), a.1.zip(r.1).map(|(x, y)| x.min(y))),
                    }))
                });
                maps.push(CacheLayer { grids, res_range: res_range.flatten().filter(|r| *r != (None, None)) });
            }
            if !maps.is_empty() {
                layers.push(WmsLayer { name: l.name.clone(), title: l.title.clone(), abstract_: l.abstract_.clone(), maps });
            }
        }
        Some(Wms { conf, layers, mgr })
    }

    /// `base_url`: the request URL (MapProxy `service.url`, used for the DCP links).
    pub async fn handle(&self, q: &std::collections::HashMap<String, String>, base_url: &str) -> Response {
        let p = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
        // wms_request: no version -> the highest; 1.3.0 or 1.1.1 handling otherwise.
        let version = if p("version").is_empty() || p("version") == "1.3.0" { Version::V130 } else { Version::V111 };
        let r = match p("request").to_ascii_lowercase().as_str() {
            "getmap" | "map" => self.map(q, version).await,
            "getcapabilities" | "capabilities" => Ok(self.capabilities(version, base_url)),
            r => Err(WmsError { code: "OperationNotSupported", text: format!("unknown WMS request type '{r}'"), version }),
        };
        r.unwrap_or_else(|e| e.into_response())
    }

    async fn map(&self, q: &std::collections::HashMap<String, String>, version: Version) -> Result<Response, WmsError> {
        let err = |code: &'static str, text: String| WmsError { code, text, version };
        let p = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
        let srs_code = if version == Version::V130 { p("crs") } else { p("srs") };
        let (w, h) = (p("width").parse::<u32>(), p("height").parse::<u32>());
        let (Ok(w), Ok(h)) = (w, h) else { return Err(err("MissingDimensionValue", "invalid width/height".into())) };
        if (w as u64) * (h as u64) > self.conf.max_output_pixels {
            return Err(err("", "image size too large".into()));
        }
        let names: Vec<&str> = p("layers").split(',').filter(|s| !s.is_empty()).collect();
        let mut req_layers = vec![];
        for n in &names {
            match self.layers.iter().find(|l| l.name == *n) {
                Some(l) => req_layers.push(l),
                None => return Err(err("LayerNotDefined", format!("unknown layer: {n}"))),
            }
        }
        let format = p("format");
        if !self.conf.image_formats.iter().any(|f| f == format) {
            return Err(err("InvalidFormat", format!("unsupported image format: {format}")));
        }
        if !self.conf.srs.iter().any(|s| s.eq_ignore_ascii_case(srs_code)) {
            let code = if version == Version::V130 { "InvalidCRS" } else { "InvalidSRS" };
            return Err(err(code, format!("unsupported {}: {srs_code}", if version == Version::V130 { "crs" } else { "srs" })));
        }
        let (srs, ne) = parse_srs(srs_code).map_err(|e| err("InvalidSRS", format!("{e:#}")))?;
        let b: Vec<f64> = p("bbox").split(',').filter_map(|v| v.trim().parse().ok()).collect();
        if b.len() != 4 {
            return Err(err("", "invalid bbox".into()));
        }
        // WMS 1.3.0 switches lat/lon for north/east CRS.
        let bbox = if version == Version::V130 && ne { [b[1], b[0], b[3], b[2]] } else { [b[0], b[1], b[2], b[3]] };
        if bbox[0] >= bbox[2] || bbox[1] >= bbox[3] {
            return Err(err("", format!("invalid bbox {}", p("bbox"))));
        }
        let transparent = p("transparent").eq_ignore_ascii_case("true");
        let bgcolor = parse_bgcolor(p("bgcolor"));
        let query = Query { bbox, size: (w, h), srs };

        // LayerRenderer + LayerMerger: layers that render nothing (BlankImageError) or fail
        // (on_source_errors: notify) are left out.
        let mut imgs = vec![];
        let mut cacheable = true;
        for l in &req_layers {
            for m in &l.maps {
                match self.cache_layer_map(m, &query).await {
                    Ok(Some(i)) => imgs.push(i),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("WMS {}: {e:#}", l.name);
                        cacheable = false;
                    }
                }
            }
        }
        let result = merge(imgs, query.size, transparent, bgcolor).map_err(|e| err("", format!("{e:#}")))?;
        let (ctype, body) = encode(result, format, transparent).map_err(|e| err("", format!("{e:#}")))?;
        let mut r = Response::new(Body::from(body));
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
        if !cacheable {
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-store"));
        }
        Ok(r)
    }

    /// `SRSConditional.get_map` -> `CacheMapLayer.get_map`. None = BlankImageError.
    async fn cache_layer_map(&self, m: &CacheLayer, q: &Query) -> Result<Option<Img>> {
        if let Some(r) = m.res_range {
            if !res_range_contains(r, q) {
                return Ok(None);
            }
        }
        let srs_list: Vec<Srs> = m.grids.iter().map(|g| g.0.meta.grid.srs.clone()).collect();
        let pick = best_srs(&q.srs, &srs_list);
        // srs_map keeps the last grid per SRS.
        let (cg, extent) = m.grids.iter().rev().find(|g| g.0.meta.grid.srs == pick).unwrap().clone();
        let in_ext = q.srs.transform_bbox(&extent.srs, q.bbox)?;
        if bbox_contains(extent.bbox, in_ext) {
            return self.image(&cg, q).await;
        }
        if !bbox_intersects(extent.bbox, in_ext) {
            return Ok(None);
        }
        let ext = extent.bbox_for(&q.srs)?;
        let (size, off, bbox) = bbox_position_in_image(q.bbox, q.size, ext);
        if size.0 == 0 || size.1 == 0 {
            return Ok(None);
        }
        let sub = Query { bbox, size, srs: q.srs.clone() };
        let Some(img) = self.image(&cg, &sub).await? else { return Ok(None) };
        // sub_image_result: pasted onto a transparent canvas.
        let mut c = Rgba::blank(q.size.0, q.size.1);
        c.paste(&img.pixels()?, off.0, off.1);
        Ok(Some(Img::Pix(c)))
    }

    /// `CacheMapLayer._image`: affected tiles at the closest level, merged and transformed.
    async fn image(&self, cg: &Arc<CacheGrid>, q: &Query) -> Result<Option<Img>> {
        let grid = &cg.meta.grid;
        let src_bbox = if q.srs != grid.srs { q.srs.transform_bbox(&grid.srs, q.bbox)? } else { q.bbox };
        if !bbox_intersects(grid.bbox, src_bbox) {
            return Ok(None);
        }
        let res = ((src_bbox[0] - src_bbox[2]).abs() / q.size.0 as f64).min((src_bbox[1] - src_bbox[3]).abs() / q.size.1 as f64);
        let level = closest_level(&grid.res, res);
        if res > grid.res[0] * MAX_SHRINK_FACTOR {
            return Ok(None);
        }
        // get_affected_level_tiles: shrink by res/10 so touched-only tiles are left out.
        let delta = grid.res[level] / 10.0;
        let (x0, y0, _) = grid.tile_coord_for_point(src_bbox[0] + delta, src_bbox[1] + delta, level);
        let (x1, y1, _) = grid.tile_coord_for_point(src_bbox[2] - delta, src_bbox[3] - delta, level);
        let xs: Vec<i64> = (x0..=x1).collect();
        let ys: Vec<i64> = if grid.flipped { (y1..=y0).collect() } else { (y0..=y1).rev().collect() };
        if xs.is_empty() || ys.is_empty() {
            return Err(anyhow!("Invalid BBOX"));
        }
        let tiles_bbox = grid.tiles_bbox((xs[0], *ys.last().unwrap(), level), (*xs.last().unwrap(), ys[0], level));
        let n = xs.len() * ys.len();
        if n >= MAX_TILE_LIMIT {
            return Err(anyhow!("too many tiles, max_tile_limit: {MAX_TILE_LIMIT}, num_tiles: {n}"));
        }
        let gs = grid.grid_sizes[level];
        let coords: Vec<Option<(i64, i64, usize)>> = ys
            .iter()
            .flat_map(|&y| xs.iter().map(move |&x| (x >= 0 && y >= 0 && x < gs.0 && y < gs.1).then_some((x, y, level))))
            .collect();
        if coords.iter().all(Option::is_none) {
            return Ok(None);
        }
        let mut tiles = Vec::with_capacity(coords.len());
        for c in &coords {
            tiles.push(match c {
                Some(c) => Some(self.mgr.tile(cg, *c).await?.data),
                None => None,
            });
        }
        // TileMerger: a single tile is used as-is; otherwise pasted onto a transparent canvas.
        let (tw, th) = grid.tile_size;
        let merged = if xs.len() == 1 && ys.len() == 1 && tiles[0].is_some() {
            Img::Raw(tiles.pop().unwrap().unwrap())
        } else {
            let mut c = Rgba::blank(xs.len() as u32 * tw, ys.len() as u32 * th);
            for (i, t) in tiles.iter().enumerate() {
                if let Some(t) = t {
                    let (x, y) = ((i % xs.len()) as u32 * tw, (i / xs.len()) as u32 * th);
                    c.paste(&decode_png(t)?.0, x as i64, y as i64);
                }
            }
            Img::Pix(c)
        };
        let src_size = (xs.len() as u32 * tw, ys.len() as u32 * th);
        tokio::task::block_in_place(|| transform(merged, src_size, tiles_bbox, &grid.srs, q)).map(Some)
    }

    fn capabilities(&self, version: Version, base_url: &str) -> Response {
        let v130 = version == Version::V130;
        let md = |k: &str| self.conf.md.get(k).map(|s| xml_escape(s)).unwrap_or_default();
        let svc = xml_escape(base_url);
        let or_url = self.conf.md.get("online_resource").map(|u| xml_escape(u)).unwrap_or_else(|| svc.clone());
        let mut x = String::new();
        if v130 {
            x.push_str("<?xml version=\"1.0\" ?>\n<WMS_Capabilities xmlns=\"http://www.opengis.net/wms\" xmlns:sld=\"http://www.opengis.net/sld\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" version=\"1.3.0\" xsi:schemaLocation=\"http://www.opengis.net/wms http://schemas.opengis.net/wms/1.3.0/capabilities_1_3_0.xsd http://www.opengis.net/sld http://schemas.opengis.net/sld/1.1.0/sld_capabilities.xsd\">\n");
        } else {
            x.push_str("<?xml version=\"1.0\"?>\n<!DOCTYPE WMT_MS_Capabilities SYSTEM \"http://schemas.opengis.net/wms/1.1.1/WMS_MS_Capabilities.dtd\"\n [\n <!ELEMENT VendorSpecificCapabilities EMPTY>\n ]>  <!-- end of DOCTYPE declaration -->\n<WMT_MS_Capabilities version=\"1.1.1\">\n");
        }
        x.push_str(&format!("<Service>\n  <Name>{}</Name>\n  <Title>{}</Title>\n  <Abstract>{}</Abstract>\n", if v130 { "WMS" } else { "OGC:WMS" }, md("title"), md("abstract")));
        x.push_str(&format!("  <OnlineResource xmlns:xlink=\"http://www.w3.org/1999/xlink\" xlink:href=\"{or_url}\"/>\n"));
        if self.conf.md.keys().any(|k| k.starts_with("contact.")) {
            x.push_str(&format!(
                "  <ContactInformation>\n    <ContactPersonPrimary>\n      <ContactPerson>{}</ContactPerson>\n      <ContactOrganization>{}</ContactOrganization>\n    </ContactPersonPrimary>\n    <ContactPosition>{}</ContactPosition>\n    <ContactAddress>\n      <AddressType>{}</AddressType>\n      <Address>{}</Address>\n      <City>{}</City>\n      <StateOrProvince>{}</StateOrProvince>\n      <PostCode>{}</PostCode>\n      <Country>{}</Country>\n    </ContactAddress>\n    <ContactVoiceTelephone>{}</ContactVoiceTelephone>\n    <ContactFacsimileTelephone>{}</ContactFacsimileTelephone>\n    <ContactElectronicMailAddress>{}</ContactElectronicMailAddress>\n  </ContactInformation>\n",
                md("contact.person"), md("contact.organization"), md("contact.position"),
                self.conf.md.get("contact.address_type").map(|s| xml_escape(s)).unwrap_or_else(|| "postal".into()),
                md("contact.address"), md("contact.city"), md("contact.state"), md("contact.postcode"), md("contact.country"),
                md("contact.phone"), md("contact.fax"), md("contact.email"),
            ));
        }
        let or = |k: &str, d: &str| self.conf.md.get(k).map(|s| xml_escape(s)).unwrap_or_else(|| d.into());
        x.push_str(&format!("  <Fees>{}</Fees>\n  <AccessConstraints>{}</AccessConstraints>\n", or("fees", "none"), or("access_constraints", "none")));
        if v130 {
            let side = (self.conf.max_output_pixels as f64).sqrt() as u64;
            x.push_str(&format!("  <MaxWidth>{side}</MaxWidth>\n  <MaxHeight>{side}</MaxHeight>\n"));
        }
        x.push_str("</Service>\n<Capability>\n  <Request>\n");
        let xl = if v130 { "" } else { " xmlns:xlink=\"http://www.w3.org/1999/xlink\"" };
        let get = format!("      <DCPType>\n        <HTTP>\n          <Get><OnlineResource{xl} xlink:href=\"{svc}?\"/></Get>\n        </HTTP>\n      </DCPType>\n");
        let caps_fmt = if v130 { "text/xml" } else { "application/vnd.ogc.wms_xml" };
        x.push_str(&format!("    <GetCapabilities>\n      <Format>{caps_fmt}</Format>\n{get}    </GetCapabilities>\n    <GetMap>\n"));
        for f in &self.conf.image_formats {
            x.push_str(&format!("      <Format>{}</Format>\n", xml_escape(f)));
        }
        x.push_str(&format!("{get}    </GetMap>\n  </Request>\n  <Exception>\n"));
        if v130 {
            x.push_str("    <Format>XML</Format>\n    <Format>INIMAGE</Format>\n    <Format>BLANK</Format>\n");
        } else {
            x.push_str("    <Format>application/vnd.ogc.se_xml</Format>\n    <Format>application/vnd.ogc.se_inimage</Format>\n    <Format>application/vnd.ogc.se_blank</Format>\n");
        }
        x.push_str("  </Exception>\n");
        // Root group (unnamed, md title, all CRS) over the named layers; extents as
        // layer_llbbox / layer_srs_bbox with the DEFAULT_EXTENTS (3857, 4326) + native SRS.
        let mut children = String::new();
        let mut exts = vec![];
        for l in &self.layers {
            // SRSConditional.extent: the first grid's; several caches merge.
            let e = merge_extents(l.maps.iter().map(|m| Some(m.grids[0].1.clone())).collect()).ok().flatten();
            children.push_str(&format!("    <Layer>\n      <Name>{}</Name>\n      <Title>{}</Title>\n", xml_escape(&l.name), xml_escape(&l.title)));
            if !l.abstract_.is_empty() {
                children.push_str(&format!("      <Abstract>{}</Abstract>\n", xml_escape(&l.abstract_)));
            }
            if let Some(e) = &e {
                children.push_str(&self.extent_xml(e, v130, "      "));
            }
            children.push_str("    </Layer>\n");
            exts.push(e);
        }
        x.push_str(&format!("  <Layer>\n    <Title>{}</Title>\n", md("title")));
        for s in &self.conf.srs {
            x.push_str(&format!("    <{t}>{}</{t}>\n", xml_escape(s), t = if v130 { "CRS" } else { "SRS" }));
        }
        if let Ok(Some(root)) = merge_extents(exts) {
            x.push_str(&self.extent_xml(&root, v130, "    "));
        }
        x.push_str(&children);
        x.push_str("  </Layer>\n</Capability>\n");
        x.push_str(if v130 { "</WMS_Capabilities>\n" } else { "</WMT_MS_Capabilities>\n" });
        let ct = if v130 { "text/xml" } else { "application/vnd.ogc.wms_xml" };
        ([(header::CONTENT_TYPE, ct)], x).into_response()
    }
}

impl Wms {
    fn extent_xml(&self, e: &Extent, v130: bool, ind: &str) -> String {
        let mut x = ll_xml(limit_llbbox(e.llbbox().unwrap_or([-180.0, -90.0, 180.0, 90.0])), v130, ind);
        let defaults = ["EPSG:3857", "EPSG:4326"];
        for code in defaults {
            if self.conf.srs.iter().any(|s| s == code) {
                if let Ok(b) = Srs::new(code).and_then(|s| e.bbox_for(&s)) {
                    x.push_str(&bbox_xml(code, b, v130, ind));
                }
            }
        }
        if !defaults.contains(&e.srs.code.as_str()) {
            x.push_str(&bbox_xml(&e.srs.code, e.bbox, v130, ind));
        }
        x
    }
}

impl IntoResponse for WmsError {
    /// WMS service exception document (`wms111exception.xml` / `wms130exception.xml`).
    fn into_response(self) -> Response {
        let code = if self.code.is_empty() { String::new() } else { format!(" code=\"{}\"", self.code) };
        let (ct, body) = if self.version == Version::V130 {
            (
                "text/xml",
                format!(
                    "<?xml version=\"1.0\"?>\n<ServiceExceptionReport version=\"1.3.0\" xmlns=\"http://www.opengis.net/ogc\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:schemaLocation=\"http://www.opengis.net/ogc http://schemas.opengis.net/wms/1.3.0/exceptions_1_3_0.xsd\">\n    <ServiceException{code}>{}</ServiceException>\n</ServiceExceptionReport>\n",
                    xml_escape(&self.text)
                ),
            )
        } else {
            (
                "application/vnd.ogc.se_xml",
                format!(
                    "<?xml version=\"1.0\"?>\n<!DOCTYPE ServiceExceptionReport SYSTEM \"http://schemas.opengis.net/wms/1.1.1/exception_1_1_1.dtd\">\n<ServiceExceptionReport version=\"1.1.1\">\n    <ServiceException{code}>{}</ServiceException>\n</ServiceExceptionReport>\n",
                    xml_escape(&self.text)
                ),
            )
        };
        (StatusCode::OK, [(header::CONTENT_TYPE, ct)], body).into_response()
    }
}

/// `ImageTransformer.transform` of the merged tiles into the query bbox/size/SRS.
fn transform(img: Img, src_size: (u32, u32), src_bbox: BBox, src_srs: &Srs, q: &Query) -> Result<Img> {
    let (dw, dh) = (q.size.0 as f64, q.size.1 as f64);
    let d = q.bbox;
    if *src_srs == q.srs {
        // _no_transformation_needed: bbox_equals with x/10 for indexes 0-1, y/10 for 2-3.
        let (xd, yd) = ((d[2] - d[0]) / dw / 10.0, (d[3] - d[1]) / dh / 10.0);
        let eq = (src_bbox[0] - d[0]).abs() < xd && (src_bbox[1] - d[1]).abs() < xd && (src_bbox[2] - d[2]).abs() < yd && (src_bbox[3] - d[3]).abs() < yd;
        if src_size == q.size && eq {
            return Ok(img);
        }
        // _transform_simple.
        let src = img.pixels()?;
        let (sw, sh) = (src_size.0 as f64, src_size.1 as f64);
        let px = |x: f64| (x - src_bbox[0]) * sw / (src_bbox[2] - src_bbox[0]);
        let py = |y: f64| (src_bbox[3] - y) * sh / (src_bbox[3] - src_bbox[1]);
        let (minx, miny, maxx, maxy) = (px(d[0]), py(d[3]), px(d[2]), py(d[1]));
        let src_res = ((src_bbox[0] - src_bbox[2]) / sw, (src_bbox[1] - src_bbox[3]) / sh);
        let dst_res = ((d[0] - d[2]) / dw, (d[1] - d[3]) / dh);
        let tenth = ((dst_res.0 / (dw * 10.0)).abs(), (dst_res.1 / (dh * 10.0)).abs());
        if (src_res.0 - dst_res.0).abs() < tenth.0 && (src_res.1 - dst_res.1).abs() < tenth.1 {
            let (x, y) = (round_int(minx), round_int(miny));
            return Ok(Img::Pix(crop(&src, x, y, q.size)));
        }
        return Ok(Img::Pix(extent_warp(&src, true, q.size, [minx, miny, maxx, maxy])));
    }
    let src = img.pixels()?;
    let meshes = transform_meshes(src_size, src_bbox, src_srs, q.size, q.bbox, &q.srs)?;
    Ok(Img::Pix(mesh_warp(&src, true, q.size, &meshes)))
}

/// Pillow `crop` with any (also negative) offset: outside is (0, 0, 0, 0).
fn crop(src: &Rgba, x: i64, y: i64, size: (u32, u32)) -> Rgba {
    let mut out = Rgba::filled(size.0, size.1, [0, 0, 0, 0]);
    out.paste(src, -x, -y);
    out
}

/// `LayerMerger.merge` for the WMS request (`img_opts.transparent`, `bgcolor`).
fn merge(mut imgs: Vec<Img>, size: (u32, u32), transparent: bool, bg: [u8; 3]) -> Result<Img> {
    // A single (transparent cache) layer is returned as-is for TRANSPARENT=true.
    if imgs.len() == 1 && transparent {
        return Ok(imgs.pop().unwrap());
    }
    let canvas_px = if transparent { [bg[0], bg[1], bg[2], 0] } else { [bg[0], bg[1], bg[2], 255] };
    let mut c = Rgba::filled(size.0, size.1, canvas_px);
    for i in imgs {
        let img = i.pixels()?;
        if transparent {
            c.alpha_composite(&img);
        } else {
            // RGB canvas: paste(img, (0, 0), img) — Paste.c BLEND with the alpha as mask.
            for (o, p) in c.px.chunks_exact_mut(4).zip(img.px.chunks_exact(4)) {
                let a = p[3] as u32;
                for k in 0..3 {
                    let t = o[k] as u32 * (255 - a) + p[k] as u32 * a + 128;
                    o[k] = (((t >> 8) + t) >> 8) as u8;
                }
            }
        }
    }
    Ok(Img::Pix(c))
}

/// `img_to_buf`: PNG keeps RGBA/RGB, JPEG drops alpha (`convert('RGB')`).
fn encode(img: Img, format: &str, transparent: bool) -> Result<(&'static str, Vec<u8>)> {
    let png = format.contains("png");
    if let (Img::Raw(d), true) = (&img, png) {
        return Ok(("image/png", d.clone()));
    }
    let img = img.pixels()?;
    if png {
        if transparent {
            return Ok(("image/png", encode_png_rgba(&img, png::Compression::Fast)?));
        }
        let rgb: Vec<u8> = img.px.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        let mut out = vec![];
        let mut e = png::Encoder::new(&mut out, img.w, img.h);
        e.set_color(png::ColorType::Rgb);
        e.set_depth(png::BitDepth::Eight);
        e.set_compression(png::Compression::Fast);
        e.write_header()?.write_image_data(&rgb)?;
        return Ok(("image/png", out));
    }
    let rgb: Vec<u8> = img.px.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
    let mut out = vec![];
    // base_config().image.jpeg_quality default 90.
    jpeg_encoder::Encoder::new(&mut out, 90).encode(&rgb, img.w as u16, img.h as u16, jpeg_encoder::ColorType::Rgb)?;
    Ok(("image/jpeg", out))
}

/// `TileGrid.closest_level` without threshold_res.
fn closest_level(res: &[f64], want: f64) -> usize {
    let mut threshold_result = None;
    for (level, &l_res) in res.iter().enumerate() {
        if let Some(t) = threshold_result {
            if l_res < want {
                return t;
            }
        }
        if l_res <= want * STRETCH_FACTOR {
            threshold_result = Some(level);
        }
    }
    res.len() - 1
}

/// `ResolutionRange.contains(bbox, size, srs)`.
fn res_range_contains((min, max): (Option<f64>, Option<f64>), q: &Query) -> bool {
    let (mut w, mut h) = ((q.bbox[2] - q.bbox[0]).abs(), (q.bbox[3] - q.bbox[1]).abs());
    if q.srs.geographic {
        let m = 2.0 * std::f64::consts::PI * 6378137.0 / 360.0;
        w *= m;
        h *= m;
    }
    let (xr, yr) = (w / q.size.0 as f64, h / q.size.1 as f64);
    if let Some(min) = min {
        let min = min + 1e-6;
        if min <= xr || min <= yr {
            return false;
        }
    }
    if let Some(max) = max {
        if max > xr || max > yr {
            return false;
        }
    }
    true
}

fn bbox_intersects(a: BBox, b: BBox) -> bool {
    a[0] < b[2] && a[2] > b[0] && a[1] < b[3] && a[3] > b[1]
}

/// The SRS and whether WMS 1.3.0 gives it north/east (geographic EPSG codes; CRS:84 is
/// lon/lat).
fn parse_srs(code: &str) -> Result<(Srs, bool)> {
    if code.eq_ignore_ascii_case("CRS:84") {
        return Ok((Srs::new("EPSG:4326")?, false));
    }
    let s = Srs::new(code)?;
    let ne = s.geographic;
    Ok((s, ne))
}

/// `bgcolor` (0xRRGGBB), default white.
fn parse_bgcolor(s: &str) -> [u8; 3] {
    let hex = s.trim_start_matches("0x").trim_start_matches("0X").trim_start_matches('#');
    match u32::from_str_radix(hex, 16) {
        Ok(v) if hex.len() == 6 => [(v >> 16) as u8, (v >> 8) as u8, v as u8],
        _ => [255, 255, 255],
    }
}

/// `limit_llbbox`: clients choke on +-90.
fn limit_llbbox(b: BBox) -> BBox {
    [b[0].max(-180.0), b[1].max(-89.999999), b[2].min(180.0), b[3].min(89.999999)]
}

/// Python `repr(float)`.
fn pf(v: f64) -> String {
    let s = format!("{v:?}");
    if s.contains(['.', 'e', 'E', 'n', 'i']) { s } else { format!("{s}.0") }
}

fn ll_xml(b: BBox, v130: bool, ind: &str) -> String {
    let b = b.map(pf);
    if v130 {
        format!(
            "{ind}<EX_GeographicBoundingBox>\n{ind}  <westBoundLongitude>{}</westBoundLongitude>\n{ind}  <eastBoundLongitude>{}</eastBoundLongitude>\n{ind}  <southBoundLatitude>{}</southBoundLatitude>\n{ind}  <northBoundLatitude>{}</northBoundLatitude>\n{ind}</EX_GeographicBoundingBox>\n",
            b[0], b[2], b[1], b[3]
        )
    } else {
        format!("{ind}<LatLonBoundingBox minx=\"{}\" miny=\"{}\" maxx=\"{}\" maxy=\"{}\" />\n", b[0], b[1], b[2], b[3])
    }
}

fn bbox_xml(code: &str, b: BBox, v130: bool, ind: &str) -> String {
    let ne = v130 && Srs::new(code).map(|s| s.geographic).unwrap_or(false);
    let b = if ne { [b[1], b[0], b[3], b[2]] } else { b }.map(pf);
    format!("{ind}<BoundingBox {}=\"{code}\" minx=\"{}\" miny=\"{}\" maxx=\"{}\" maxy=\"{}\" />\n", if v130 { "CRS" } else { "SRS" }, b[0], b[1], b[2], b[3])
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
