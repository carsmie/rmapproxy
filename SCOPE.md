# rMapproxy scope

A Rust replacement for the parts of [MapProxy](https://mapproxy.org) 7.0 that
[tnt-mapproxy](https://github.com/kartverket/tnt-mapproxy) uses. Goal: the same
tiles on disk (same pixels, same paths) with far less CPU, so it can replace MapProxy
behind the existing nginx frontend without a cache wipe.

Reference: MapProxy 7.0.0 as shipped in the tnt-mapproxy image. File/line references
below point into its `mapproxy/` package.

## In scope (v1)

| Area | What tnt-mapproxy uses | MapProxy reference |
|---|---|---|
| Config | `base:` includes, `grids`, `caches`, `sources`, `layers`, `globals.cache/image/http/tiles`; YAML merge keys in seed.yaml | `config/loader.py` |
| Grids | `srs`, `bbox`, `bbox_srs` (same as srs), `origin: nw`, `res_factor: 2`, `num_levels`, `min_res`, `tile_size`, `base:` | `grid/tile_grid.py`, `grid/resolutions.py` |
| Caches | file cache, default `tc` layout, dir `<cache>_<srs without ':'>`, `meta_size`, `meta_buffer`, `image.colors` (+ `encoding_options.quantizer`) | `cache/file.py`, `cache/path.py`, `grid/meta_grid.py` |
| Cache globals | `base_dir`, `tile_lock_dir`, `tile_lock_timeout`, `link_single_color_images: hardlink` | `cache/base.py`, `util/lock.py` |
| Sources | `type: wms` (1.1.1 GetMap), `req.{url,layers,transparent,dpi,format}`, `supported_srs`, `min_res`/`max_res`, `coverage` (`datasource` GeoJSON or `bbox`, with `srs`), `on_error.other: {response: transparent, cache: false}`, `http.client_timeout` | `source/wms.py`, `client/wms.py`, `util/coverage.py` |
| Merge | transparent RGBA canvas + Pillow `alpha_composite` per source, bottom first | `image/merge.py` |
| Services | WMTS GetTile KVP (`/service`) and REST (`/wmts/1.0.0/{layer}/{style}/{set}/{z}/{row}/{col}.png`), GetCapabilities | `service/wmts.py` |
| Seeding | `mapproxy-seed -s seed.yaml -f mapproxy.yaml --seed a,b -c N --continue --progress-file F` subset: `levels`, `coverages`, `refresh_before`, `grids`, `caches` | `seed/` |

## Out of scope for v1 (decide later)

* WMS service (GetMap from caches, reprojection, GetFeatureInfo, GetLegendGraphic) — prod
  WMS traffic is near zero; legends are files served by the admin app.
* Reprojecting sources: the `utm32n` / `utm35n` grids of `topo_cache` (sources don't support
  those SRSs). `webmercator` works without reprojection (sources list EPSG:3857).
* Demo service, TMS, KML, other cache backends, dimensions, `clip: true` coverages.
* The admin app (`app/main.py`) stays Python and calls `rmapproxy seed` instead of
  `mapproxy-seed`; its log parser and progress file are the compatibility contract.

## Parity rules

Output-determining semantics follow MapProxy exactly (tile/metatile bboxes incl. Python
`round(x, 12)`, buffer clamping at the grid bbox, resolution-range and coverage tests,
coverage sub-requests with `int()` pixel offsets, Pillow's integer `alpha_composite`,
single-colour detection). Mechanics are free: sources are fetched concurrently, images
are composited without decoding to Pillow objects, PNG encoding uses a faster zlib.
