//! Tile creation (`cache/tile.py` TileManager/TileCreator + `image/merge.py`): metatile,
//! sources, merge, split, encode, store. Sources are fetched concurrently (MapProxy fetches
//! them one after another); the merge order and arithmetic are MapProxy's.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use futures::stream::FuturesOrdered;
use tracing::{debug, info, warn};

use crate::cache::FileCache;
use crate::config::{Config, ImageOpts};
use crate::grid::{MetaGrid, MetaTile, TileCoord};
use crate::image::{MergeMode, Rgba, encode_png_rgba};
use crate::source::{SourceImage, WmsSource};

/// One cache in one grid (MapProxy creates one TileManager per pair).
pub struct CacheGrid {
    pub cache: String,
    pub grid: String,
    pub file: FileCache,
    pub meta: MetaGrid,
    pub sources: Vec<Arc<WmsSource>>,
    pub image: ImageOpts,
}

pub struct Tile {
    pub data: Vec<u8>,
    pub mtime: Option<SystemTime>,
}

pub struct TileManager {
    pub cfg: Arc<Config>,
    caches: HashMap<(String, String), Arc<CacheGrid>>,
}

impl TileManager {
    pub fn new(cfg: Arc<Config>) -> Result<TileManager> {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(64)
            .user_agent(concat!("rmapproxy/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let mut sources: HashMap<String, Arc<WmsSource>> = HashMap::new();
        for (name, s) in &cfg.sources {
            sources.insert(name.clone(), Arc::new(WmsSource::new(s.clone(), client.clone())));
        }
        let mut caches = HashMap::new();
        for c in cfg.caches.values() {
            let srcs: Vec<Arc<WmsSource>> = match c
                .sources
                .iter()
                .map(|s| sources.get(s).cloned().ok_or_else(|| anyhow!("cache {}: source {s} is not a WMS source", c.name)))
                .collect::<Result<_>>()
            {
                Ok(s) => s,
                Err(e) => {
                    warn!("{e:#}; cache skipped");
                    continue;
                }
            };
            // compatible_image_options: the cache is transparent if any source asks for
            // `transparent: true` and none for false (or the cache sets it). Opaque caches
            // merge onto white RGB with paste — not implemented.
            let transparent = c.image.transparent.or_else(|| {
                let ts: Vec<Option<bool>> = srcs
                    .iter()
                    .map(|s| s.conf.params.iter().find(|(k, _)| k == "transparent").map(|(_, v)| v.eq_ignore_ascii_case("true")))
                    .collect();
                if ts.contains(&Some(false)) { Some(false) } else { ts.contains(&Some(true)).then_some(true) }
            });
            if transparent != Some(true) {
                warn!("cache {}: opaque caches are not supported; skipped", c.name);
                continue;
            }
            for g in &c.grids {
                let grid = cfg.grids[g].clone();
                // MapProxy reprojects sources that don't support the grid SRS; rMapproxy
                // doesn't (yet), so such cache/grid pairs are not served.
                if let Some(s) = srcs.iter().find(|s| !s.conf.supported_srs.is_empty() && !s.conf.supported_srs.contains(&grid.srs)) {
                    warn!("cache {} grid {g}: source {} needs reprojection (not supported); skipped", c.name, s.conf.name);
                    continue;
                }
                let cg = CacheGrid {
                    cache: c.name.clone(),
                    grid: g.clone(),
                    file: FileCache::new(
                        cfg.cache_dir(&c.name, g),
                        cfg.globals.tile_lock_dir.clone(),
                        // MapProxy's TileLocker waits http.client_timeout, not tile_lock_timeout.
                        cfg.globals.client_timeout,
                        cfg.globals.link_single_color,
                    ),
                    meta: MetaGrid { grid, meta_size: c.meta_size, meta_buffer: c.meta_buffer },
                    sources: srcs.clone(),
                    image: c.image.clone(),
                };
                caches.insert((c.name.clone(), g.clone()), Arc::new(cg));
            }
        }
        Ok(TileManager { cfg, caches })
    }

    pub fn cache_grid(&self, cache: &str, grid: &str) -> Option<Arc<CacheGrid>> {
        self.caches.get(&(cache.to_string(), grid.to_string())).cloned()
    }

    /// A tile from the cache, created (with its whole metatile) if missing.
    pub async fn tile(&self, cg: &Arc<CacheGrid>, coord: TileCoord) -> Result<Tile> {
        if !cg.meta.grid.contains(coord) {
            bail!("tile {coord:?} out of range for grid {}", cg.grid);
        }
        if let Some(data) = cg.file.load(coord) {
            return Ok(Tile { data, mtime: cg.file.mtime(coord) });
        }
        let meta = cg.meta.meta_tile(coord);
        let main = meta.main_tile().ok_or_else(|| anyhow!("empty metatile"))?;
        let lock_cg = cg.clone();
        let _lock = tokio::task::spawn_blocking(move || lock_cg.file.lock(main)).await??;
        // Another process may have created it while we waited for the lock.
        if let Some(data) = cg.file.load(coord) {
            return Ok(Tile { data, mtime: cg.file.mtime(coord) });
        }
        let tiles = self.create_meta(cg, &meta, true).await?;
        tiles
            .into_iter()
            .find(|(c, _)| *c == coord)
            .map(|(_, data)| Tile { data, mtime: Some(SystemTime::now()) })
            .ok_or_else(|| anyhow!("tile {coord:?} not in its metatile"))
    }

    /// Render a metatile and (if all sources succeeded and `store`) write all its tiles.
    /// Returns the encoded tiles.
    pub async fn create_meta(&self, cg: &Arc<CacheGrid>, meta: &MetaTile, store: bool) -> Result<Vec<(TileCoord, Vec<u8>)>> {
        let t0 = std::time::Instant::now();
        let srs = &cg.meta.grid.srs;
        let size = meta.size;
        // All sources in flight at once; composited in config order as soon as each one's
        // predecessors are done, so only out-of-order images are held in memory.
        let mut stream: FuturesOrdered<_> = cg
            .sources
            .iter()
            .map(|s| async move { (s, s.get_map(meta.bbox, meta.size, srs).await) })
            .collect();
        let mut merger = Merger::new(size);
        let mut cacheable = true;
        while let Some((s, r)) = stream.next().await {
            match r.with_context(|| format!("source {}", s.conf.name))? {
                SourceImage::Skip => {}
                SourceImage::Transparent => {
                    cacheable = false;
                    merger.add_transparent();
                }
                SourceImage::Png { data, size: sub, place } => {
                    // Decode + composite on this worker without stalling the runtime.
                    tokio::task::block_in_place(|| -> Result<()> {
                        let (img, mode) = SourceImage::decode(&data, sub, place, size)
                            .with_context(|| format!("source {}", s.conf.name))?;
                        merger.add(img, mode);
                        Ok(())
                    })?;
                }
            }
        }
        let t_fetch = t0.elapsed();
        let tile_size = cg.meta.grid.tile_size;
        let tiles: Vec<(TileCoord, (u32, u32))> = meta.tiles.iter().filter_map(|(c, off)| c.map(|c| (c, *off))).collect();
        let image = cg.image.clone();
        let encoded = tokio::task::spawn_blocking(move || -> Result<Vec<(TileCoord, Vec<u8>, Option<[u8; 4]>)>> {
            let merged = merger.finish();
            let parts: Vec<_> = tiles
                .iter()
                .map(|&(c, (x, y))| (c, merged.crop(x, y, tile_size.0, tile_size.1)))
                .collect();
            std::thread::scope(|s| {
                let handles: Vec<_> = parts
                    .iter()
                    .map(|(c, img)| {
                        let image = &image;
                        s.spawn(move || -> Result<_> { Ok((*c, encode(img, image)?, img.single_color())) })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().map_err(|_| anyhow!("encoder panicked"))?).collect()
            })
        })
        .await??;
        if store && cacheable {
            let cg2 = cg.clone();
            let to_store: Vec<_> = encoded.iter().map(|(c, d, sc)| (*c, d.clone(), *sc)).collect();
            tokio::task::spawn_blocking(move || -> Result<()> {
                for (c, d, sc) in to_store {
                    cg2.file.store(c, &d, sc)?;
                }
                Ok(())
            })
            .await??;
        } else if !cacheable {
            info!("metatile {:?} of {} not cached (source error)", meta.main_tile(), cg.cache);
        }
        debug!(
            "metatile {:?} {}x{} {}: fetch {:?}, total {:?}",
            meta.main_tile(),
            size.0,
            size.1,
            cg.cache,
            t_fetch,
            t0.elapsed()
        );
        Ok(encoded.into_iter().map(|(c, d, _)| (c, d)).collect())
    }
}

/// `LayerMerger.merge` for a transparent RGBA result, fed one layer at a time (bottom first).
struct Merger {
    size: (u32, u32),
    /// The first layer is kept as-is: a single layer of the right size is returned
    /// without compositing onto the canvas, as MapProxy does.
    first: Option<(Rgba, MergeMode)>,
    canvas: Option<Rgba>,
}

impl Merger {
    fn new(size: (u32, u32)) -> Self {
        Merger { size, first: None, canvas: None }
    }

    fn apply(canvas: &mut Rgba, img: &Rgba, mode: MergeMode) {
        match mode {
            MergeMode::Composite => canvas.alpha_composite(img),
            MergeMode::Paste => canvas.paste(img, 0, 0),
        }
    }

    /// A fully transparent layer (`on_error: transparent`): compositing it changes nothing,
    /// but it still counts as a layer for the single-layer shortcut.
    fn add_transparent(&mut self) {
        if self.canvas.is_none() {
            match self.first.take() {
                Some((f, fm)) => {
                    let mut c = Rgba::blank(self.size.0, self.size.1);
                    Self::apply(&mut c, &f, fm);
                    self.canvas = Some(c);
                }
                None => self.first = Some((Rgba::blank(self.size.0, self.size.1), MergeMode::Composite)),
            }
        }
    }

    fn add(&mut self, img: Rgba, mode: MergeMode) {
        if let Some(c) = &mut self.canvas {
            Self::apply(c, &img, mode);
        } else if let Some((f, fm)) = self.first.take() {
            let mut c = Rgba::blank(self.size.0, self.size.1);
            Self::apply(&mut c, &f, fm);
            Self::apply(&mut c, &img, mode);
            self.canvas = Some(c);
        } else {
            self.first = Some((img, mode));
        }
    }

    fn finish(self) -> Rgba {
        match (self.canvas, self.first) {
            (Some(c), _) => c,
            (None, Some((f, _))) if (f.w, f.h) == self.size => f,
            (None, Some((f, fm))) => {
                let mut c = Rgba::blank(self.size.0, self.size.1);
                Self::apply(&mut c, &f, fm);
                c
            }
            (None, None) => Rgba::blank(self.size.0, self.size.1),
        }
    }
}

fn encode(img: &Rgba, opts: &ImageOpts) -> Result<Vec<u8>> {
    if let Some(colors) = opts.colors {
        if opts.quantizer.as_deref() == Some("mediancut") {
            // Caches here are transparent (checked in TileManager::new): MapProxy's alpha path.
            let (pal, idx) = crate::image::quantize_mediancut_alpha(img, colors);
            return crate::image::encode_png_paletted(img.w, img.h, &pal, &idx, true, png::Compression::Balanced);
        }
        // TODO(parity): Pillow's fastoctree (the default quantizer).
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| warn!("image.colors with the fastoctree quantizer is not implemented; writing RGBA"));
    }
    encode_png_rgba(img, png::Compression::Balanced)
}
