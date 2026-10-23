//! The subset of the MapProxy configuration tnt-mapproxy uses (see SCOPE.md), with
//! MapProxy's defaults and `base:` merge semantics (`config/loader.py`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde_yaml::{Mapping, Value};

use crate::coverage::Coverage;
use crate::grid::TileGrid;
use crate::srs::Srs;

#[derive(Debug, Clone)]
pub struct Globals {
    pub base_dir: PathBuf,
    pub tile_lock_dir: PathBuf,
    pub link_single_color: bool,
    pub client_timeout: f64,
    pub expires_hours: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct ImageOpts {
    /// `image.colors` (0/None = no quantising).
    pub colors: Option<u32>,
    /// `encoding_options.quantizer`: fastoctree (default) or mediancut.
    pub quantizer: Option<String>,
    pub transparent: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct CacheConf {
    pub name: String,
    pub grids: Vec<String>,
    pub sources: Vec<String>,
    pub meta_size: (i64, i64),
    pub meta_buffer: i64,
    pub image: ImageOpts,
}

#[derive(Debug, Clone)]
pub struct SourceConf {
    pub name: String,
    pub url: String,
    /// GetMap parameters from `req` besides url (layers, transparent, dpi, format, …),
    /// lower-cased keys, in config order.
    pub params: Vec<(String, String)>,
    pub supported_srs: Vec<Srs>,
    pub min_res: Option<f64>,
    pub max_res: Option<f64>,
    pub coverage: Option<Arc<Coverage>>,
    /// `on_error: {other: {response: transparent, cache: false}}`.
    pub on_error_transparent: bool,
    pub client_timeout: f64,
}

#[derive(Debug, Clone)]
pub struct LayerConf {
    pub name: String,
    pub title: String,
    pub abstract_: String,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct WmtsConf {
    pub title: String,
    pub abstract_: String,
    pub online_resource: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub globals: Globals,
    pub grids: BTreeMap<String, TileGrid>,
    pub caches: BTreeMap<String, CacheConf>,
    pub sources: BTreeMap<String, SourceConf>,
    pub layers: Vec<LayerConf>,
    pub wmts: WmtsConf,
}

/// Read a YAML file with merge keys applied and its `base:` files merged underneath.
pub fn load_yaml_with_bases(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut v: Value = serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    v.apply_merge().with_context(|| format!("merge keys in {}", path.display()))?;
    if v.is_null() {
        v = Value::Mapping(Mapping::new());
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let bases: Vec<String> = match v.get("base") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Sequence(s)) => s.iter().filter_map(|b| b.as_str().map(String::from)).collect(),
        _ => vec![],
    };
    if let Value::Mapping(m) = &mut v {
        m.remove("base");
    }
    let mut merged = Value::Mapping(Mapping::new());
    for b in bases {
        let base = load_yaml_with_bases(&dir.join(b))?;
        merge_into(&mut merged, base);
    }
    merge_into(&mut merged, v);
    Ok(merged)
}

/// `merge_dict(conf, base)`: mappings merge recursively, anything else is replaced.
fn merge_into(dst: &mut Value, src: Value) {
    match (dst, src) {
        (Value::Mapping(d), Value::Mapping(s)) => {
            for (k, v) in s {
                match d.get_mut(&k) {
                    Some(dv) if dv.is_mapping() && v.is_mapping() => merge_into(dv, v),
                    _ => {
                        d.insert(k, v);
                    }
                }
            }
        }
        (d, s) => *d = s,
    }
}

fn get<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, k| v.get(*k))
}

fn f64_of(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_i64().map(|i| i as f64)).or_else(|| v.as_str()?.parse().ok())
}

fn str_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Sequence(s)) => s.iter().filter_map(str_of).collect(),
        Some(v) => str_of(v).into_iter().collect(),
        None => vec![],
    }
}

fn pair(v: Option<&Value>, default: (i64, i64)) -> (i64, i64) {
    match v.and_then(|v| v.as_sequence()) {
        Some(s) if s.len() == 2 => (
            s[0].as_i64().unwrap_or(default.0),
            s[1].as_i64().unwrap_or(default.1),
        ),
        _ => default,
    }
}

fn bbox4(v: &Value) -> Result<[f64; 4]> {
    let s = v.as_sequence().ok_or_else(|| anyhow!("bbox must be a list"))?;
    if s.len() != 4 {
        bail!("bbox needs 4 values");
    }
    let mut b = [0.0; 4];
    for (i, x) in s.iter().enumerate() {
        b[i] = f64_of(x).ok_or_else(|| anyhow!("bad bbox value {x:?}"))?;
    }
    Ok(b)
}

const WEBMERCATOR_EXTENT: f64 = 20037508.342789244;

/// MapProxy's predefined grids usable as `base:`.
fn builtin_grid(name: &str) -> Option<Value> {
    let yaml = match name {
        "GLOBAL_WEBMERCATOR" => format!(
            "{{srs: 'EPSG:3857', origin: nw, bbox: [-{e}, -{e}, {e}, {e}], bbox_srs: 'EPSG:3857'}}",
            e = WEBMERCATOR_EXTENT
        ),
        "GLOBAL_MERCATOR" => format!(
            "{{srs: 'EPSG:900913', origin: sw, bbox: [-{e}, -{e}, {e}, {e}], bbox_srs: 'EPSG:900913'}}",
            e = WEBMERCATOR_EXTENT
        ),
        "GLOBAL_GEODETIC" => "{srs: 'EPSG:4326', origin: sw, bbox: [-180, -90, 180, 90], bbox_srs: 'EPSG:4326'}".into(),
        _ => return None,
    };
    serde_yaml::from_str(&yaml).ok()
}

fn grid_conf(name: &str, grids: &Mapping, depth: usize) -> Result<Value> {
    if depth > 10 {
        bail!("grid base chain too deep at {name}");
    }
    let own = match grids.get(name) {
        Some(v) => v.clone(),
        None => return builtin_grid(name).ok_or_else(|| anyhow!("unknown grid '{name}'")),
    };
    let mut merged = match own.get("base").and_then(|b| b.as_str()) {
        Some(b) => grid_conf(b, grids, depth + 1)?,
        None => Value::Mapping(Mapping::new()),
    };
    merge_into(&mut merged, own);
    Ok(merged)
}

/// `grid/resolutions.py::resolutions` (the min_res / num_levels path) and TileGrid's
/// default pyramid.
fn grid_resolutions(g: &Value, bbox: [f64; 4], tile: (u32, u32)) -> Result<Vec<f64>> {
    if let Some(r) = g.get("res").and_then(|r| r.as_sequence()) {
        return r.iter().map(|x| f64_of(x).ok_or_else(|| anyhow!("bad res"))).collect();
    }
    let factor = match g.get("res_factor") {
        Some(Value::String(s)) if s == "sqrt2" => 2f64.sqrt(),
        Some(v) => f64_of(v).unwrap_or(2.0),
        None => 2.0,
    };
    let num_levels = g.get("num_levels").and_then(|v| v.as_u64()).map(|n| n as usize);
    if g.get("max_res").is_some() {
        bail!("grids with max_res are not supported");
    }
    let min_res = match g.get("min_res").and_then(f64_of) {
        Some(r) => r,
        None => ((bbox[2] - bbox[0]) / tile.0 as f64).max((bbox[3] - bbox[1]) / tile.1 as f64),
    };
    let n = num_levels.unwrap_or(if factor == 2f64.sqrt() { 40 } else { 20 });
    let mut res = vec![min_res];
    while res.len() < n {
        res.push(res[res.len() - 1] / factor);
    }
    Ok(res)
}

fn build_grid(name: &str, g: &Value) -> Result<TileGrid> {
    let srs = Srs::new(g.get("srs").and_then(|s| s.as_str()).ok_or_else(|| anyhow!("grid {name}: no srs"))?)?;
    let tile = pair(g.get("tile_size"), (256, 256));
    let tile = (tile.0 as u32, tile.1 as u32);
    let mut bbox = match g.get("bbox") {
        Some(b) => bbox4(b)?,
        None if srs.geographic => [-180.0, -90.0, 180.0, 90.0],
        None => [-WEBMERCATOR_EXTENT, -WEBMERCATOR_EXTENT, WEBMERCATOR_EXTENT, WEBMERCATOR_EXTENT],
    };
    if let Some(bs) = g.get("bbox_srs").and_then(|s| s.as_str()) {
        let bs = Srs::new(bs)?;
        if bs != srs {
            bbox = bs.transform_bbox(&srs, bbox)?;
        }
    }
    let flipped = match g.get("origin").and_then(|o| o.as_str()).unwrap_or("ll") {
        "nw" | "ul" | "upper-left" => true,
        "sw" | "ll" | "lower-left" => false,
        o => bail!("grid {name}: unsupported origin '{o}'"),
    };
    let res = grid_resolutions(g, bbox, tile)?;
    Ok(TileGrid::new(name, srs, bbox, tile, res, flipped))
}

fn layers_of(v: Option<&Value>, out: &mut Vec<LayerConf>) {
    let Some(Value::Sequence(items)) = v else { return };
    for l in items {
        if let Some(name) = l.get("name").and_then(str_of) {
            let sources = strings(l.get("sources"));
            if !sources.is_empty() {
                out.push(LayerConf {
                    name,
                    title: l.get("title").and_then(str_of).unwrap_or_default(),
                    abstract_: l.get("abstract").and_then(str_of).unwrap_or_default(),
                    sources,
                });
            }
        }
        layers_of(l.get("layers"), out);
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let v = load_yaml_with_bases(path)?;
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let abspath = |p: &str| -> PathBuf {
            let p = PathBuf::from(p);
            if p.is_absolute() { p } else { dir.join(p) }
        };

        let base_dir = get(&v, &["globals", "cache", "base_dir"])
            .and_then(|s| s.as_str())
            .map(abspath)
            .unwrap_or_else(|| dir.join("cache_data"));
        let globals = Globals {
            tile_lock_dir: get(&v, &["globals", "cache", "tile_lock_dir"])
                .and_then(|s| s.as_str())
                .map(abspath)
                .unwrap_or_else(|| base_dir.join("tile_locks")),
            base_dir,
            link_single_color: match get(&v, &["globals", "cache", "link_single_color_images"]) {
                Some(Value::Bool(b)) => *b,
                Some(Value::String(s)) => s == "hardlink" || s == "symlink" || s == "true",
                _ => false,
            },
            client_timeout: get(&v, &["globals", "http", "client_timeout"]).and_then(f64_of).unwrap_or(60.0),
            expires_hours: get(&v, &["globals", "tiles", "expires_hours"]).and_then(f64_of),
        };

        // Only grids that caches use are built: others may be base-only templates
        // (tnt-mapproxy's `utm` has no srs), which MapProxy never instantiates either.
        let empty = Mapping::new();
        let grid_map = v.get("grids").and_then(|g| g.as_mapping()).unwrap_or(&empty);
        let used: std::collections::BTreeSet<String> = v
            .get("caches")
            .and_then(|c| c.as_mapping())
            .into_iter()
            .flatten()
            .flat_map(|(_, c)| strings(c.get("grids")))
            .collect();
        let mut grids = BTreeMap::new();
        for k in used {
            let g = build_grid(&k, &grid_conf(&k, grid_map, 0)?).with_context(|| format!("grid {k}"))?;
            grids.insert(k, g);
        }

        let mut coverages: HashMap<String, Arc<Coverage>> = HashMap::new();
        let mut sources = BTreeMap::new();
        if let Some(m) = v.get("sources").and_then(|s| s.as_mapping()) {
            for (k, s) in m {
                let name = k.as_str().unwrap_or_default().to_string();
                match s.get("type").and_then(|t| t.as_str()) {
                    Some("wms") => {}
                    other => bail!("source {name}: type {other:?} not supported"),
                }
                let req = s.get("req").and_then(|r| r.as_mapping()).ok_or_else(|| anyhow!("source {name}: no req"))?;
                let mut url = None;
                let mut params = vec![];
                for (rk, rv) in req {
                    let rk = rk.as_str().unwrap_or_default().to_ascii_lowercase();
                    let val = match rv {
                        Value::Sequence(items) => items.iter().filter_map(str_of).collect::<Vec<_>>().join(","),
                        Value::Bool(b) => b.to_string(),
                        v => str_of(v).unwrap_or_default(),
                    };
                    if rk == "url" { url = Some(val) } else { params.push((rk, val)) }
                }
                let coverage = match s.get("coverage") {
                    Some(c) => {
                        let key = serde_yaml::to_string(c)?;
                        Some(match coverages.get(&key) {
                            Some(c) => c.clone(),
                            None => {
                                let cov = Arc::new(Coverage::from_conf(c, &abspath)?);
                                coverages.insert(key, cov.clone());
                                cov
                            }
                        })
                    }
                    None => None,
                };
                let on_error_transparent = get(s, &["on_error", "other", "response"])
                    .and_then(|r| r.as_str())
                    == Some("transparent");
                sources.insert(
                    name.clone(),
                    SourceConf {
                        url: url.ok_or_else(|| anyhow!("source {name}: no req.url"))?,
                        params,
                        supported_srs: strings(s.get("supported_srs"))
                            .iter()
                            .map(|c| Srs::new(c))
                            .collect::<Result<_>>()?,
                        min_res: s.get("min_res").and_then(f64_of),
                        max_res: s.get("max_res").and_then(f64_of),
                        coverage,
                        on_error_transparent,
                        client_timeout: get(s, &["http", "client_timeout"]).and_then(f64_of).unwrap_or(globals.client_timeout),
                        name,
                    },
                );
            }
        }

        let mut caches = BTreeMap::new();
        if let Some(m) = v.get("caches").and_then(|s| s.as_mapping()) {
            for (k, c) in m {
                let name = k.as_str().unwrap_or_default().to_string();
                if let Some(t) = get(c, &["cache", "type"]).and_then(|t| t.as_str()) {
                    if t != "file" {
                        bail!("cache {name}: type {t} not supported");
                    }
                }
                let img = c.get("image");
                let colors = img.and_then(|i| i.get("colors")).and_then(|c| c.as_u64()).map(|c| c as u32).filter(|&c| c > 0);
                caches.insert(
                    name.clone(),
                    CacheConf {
                        grids: strings(c.get("grids")),
                        sources: strings(c.get("sources")),
                        meta_size: pair(c.get("meta_size"), (4, 4)),
                        meta_buffer: c.get("meta_buffer").and_then(|b| b.as_i64()).unwrap_or(80),
                        image: ImageOpts {
                            colors,
                            quantizer: img
                                .and_then(|i| get(i, &["encoding_options", "quantizer"]))
                                .and_then(str_of),
                            transparent: img.and_then(|i| i.get("transparent")).and_then(|t| t.as_bool()),
                        },
                        name,
                    },
                );
            }
        }

        let mut layers = vec![];
        layers_of(v.get("layers"), &mut layers);

        let md = get(&v, &["services", "wmts", "md"]);
        let wmts = WmtsConf {
            title: md.and_then(|m| m.get("title")).and_then(str_of).unwrap_or_else(|| "WMTS".into()),
            abstract_: md.and_then(|m| m.get("abstract")).and_then(str_of).unwrap_or_default(),
            online_resource: md.and_then(|m| m.get("online_resource")).and_then(str_of).unwrap_or_default(),
        };

        let cfg = Config { globals, grids, caches, sources, layers, wmts };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        for c in self.caches.values() {
            for g in &c.grids {
                if !self.grids.contains_key(g) {
                    bail!("cache {}: unknown grid {g}", c.name);
                }
            }
            for s in &c.sources {
                if !self.sources.contains_key(s) && !self.caches.contains_key(s) {
                    bail!("cache {}: unknown source {s}", c.name);
                }
            }
        }
        Ok(())
    }

    /// `<base_dir>/<cache>_<grid srs without ':'>` (`config/configuration/cache.py`).
    pub fn cache_dir(&self, cache: &str, grid: &str) -> PathBuf {
        let srs = self.grids[grid].srs.code.replace(':', "");
        self.globals.base_dir.join(format!("{cache}_{srs}"))
    }
}
