# rMapproxy

The MapProxy features [tnt-mapproxy](https://github.com/kartverket/tnt-mapproxy) uses,
rewritten in Rust: WMTS tiles rendered from WMS sources (rGIS) into MapProxy's file cache,
plus seeding. Same pixels and the same files on disk as MapProxy 7.0, so it can replace
MapProxy behind tnt-mapproxy's nginx frontend without wiping the cache. Scope and parity
rules: [SCOPE.md](SCOPE.md).

## Usage

```sh
cargo build --release
rmapproxy check -f mapproxy.yaml                                   # load config, list layers/caches/dirs
rmapproxy serve -f mapproxy.yaml --bind 0.0.0.0:8080               # WMTS KVP (/service) + REST (/wmts/1.0.0/...)
rmapproxy seed  -f mapproxy.yaml -s seed.yaml --seed task1,task2 -c 16
rmapproxy tile  -f mapproxy.yaml --cache topo_cache --grid norway_25833 13/4079/3539 -o t.png
```

It reads tnt-mapproxy's *rendered* config (`mapproxy.yaml` + `rendered/` as written by its
`entrypoint.sh`, including `rendered/hidpi.yaml`). `RUST_LOG=rmapproxy=debug` logs every
source URL and per-metatile timings.

`RMAPPROXY_MAX_RENDERS` (default 32, 0 = unbounded) caps the metatiles `serve` renders on
demand at once; requests for tiles of a metatile already being rendered wait for it.
`/_admin` sends no CORS headers and refuses non-GET requests from other sites. With
`RMAPPROXY_WORKER_TOKEN` set on the admin and the workers, `/_admin/work/*` requires it as a
bearer token.

## Status (2026-10-26)

Verified against MapProxy 7.0 (tnt-mapproxy image) on the same rendered config and the
same local rGIS, Oslo box, `-c 4`, 4 dedicated cores:

| | MapProxy 7.0 | rMapproxy |
|---|---|---|
| `topo_cache` z6–13, 816 tiles | reference | **817 / 817 files pixel-identical** |
| — wall / CPU | 107.9 s / 121.5 s | 13.7–16.5 s / 36 s |
| `topo_cache_2x` z6–12 (mediancut PNG8), 200 tiles | reference | **200 / 200 pixel-identical, same PLTE/tRNS** |
| — wall / CPU | 79.5 s / 146.6 s | 14.8 s / 40 s |
| Peak RSS (`-c 4` / `-c 16`) | 249 MB / – | 166–216 MB / 386–509 MB |

Why it's faster: the 33 sources of a metatile are fetched concurrently (MapProxy: one
after another), compositing is a table-driven port of Pillow's integer `alpha_composite`
with exact fast paths, tiles are encoded in parallel. Source responses stay encoded until
their turn in the merge, so memory stays at about one decoded layer per metatile.

Done: config subset (base includes, merge keys, grids incl. `base:`), grid/metatile
geometry, coverages (bbox + GeoJSON polygons, strict/shapely intersect semantics),
resolution ranges, coverage sub-requests, `on_error` transparent/uncached, Pillow-exact
merge, Pillow's median-cut quantizer (Quant.c port, incl. MapProxy's 1-bit alpha handling),
`tc` file cache with atomic writes, single-colour hardlinks and MapProxy-compatible flock
files, WMTS GetTile KVP + REST + GetCapabilities, CORS/cache headers, seeding (TileWalker
order, coverage, levels, refresh_before / overwrite-all, `-c`), `--cleanup` (incl. complete
extent), `--progress-file` (pickle readable by tnt-mapproxy's admin) + `--continue`. Invoked
as `mapproxy-seed` it behaves as the seed command. Static musl `Dockerfile` (rustls).
`serve --admin [--seed-conf seed.yaml] [--admin-assets DIR]` adds the cache-admin API on
`/_admin/*` (`src/admin.rs`: seed/cleanup jobs as child processes with stop/continue, estimates,
other seeders' progress files, cache statistics, legends, history); the GUI lives in
tnt-rmapproxy's `admin/`. A seed job can be distributed: the admin splits it into chunks
(≤16×16 metatiles of one level that touch the coverage) and `rmapproxy seed-worker --admin URL`
pods lease, render and report them (`src/work.rs`; leases expire after 2 min, SIGTERM hands the
chunk back). Verified: the chunked job writes exactly the tile set of `rmapproxy seed` for the
same tasks (3 237/3 237 files, byte-identical, incl. a polygon coverage).

2026-10-26, against MapProxy 7.0 with a deterministic fake WMS (RGBA, paletted+tRNS, opaque
RGB, polygon coverage sub-queries):

| | rMapproxy vs MapProxy 7.0 |
|---|---|
| Reprojected seeding, `utm32n` + `utm35n` z2–13 | **2 741 / 2 741 tiles pixel-identical**, same tile set |
| WMS GetMap (PNG): tile passthrough, crop, bilinear EXTENT, MESH to 4326/3857, 1.3.0 axis order, bgcolor, multi-layer, extent clipping | **17 / 17 pixel-identical**, same RGB/RGBA mode |
| WMS GetCapabilities 1.1.1 / 1.3.0 | same document except GetFeatureInfo (not offered) and last-digit float noise |

Note: MapProxy's mediancut path marks transparent pixels as palette index 255, which lies
outside the written palette/tRNS (or gets truncated to a lower bit depth), so those pixels
decode as opaque black. rMapproxy reproduces that byte for byte (parity); fixing it would
change tiles relative to MapProxy.

## Not yet

1. `fastoctree` quantizer (MapProxy's default, only `qgis_hoydekurve_cache` uses it): written
   as RGBA with a warning.
2. WMS GetFeatureInfo / GetLegendGraphic / `tiled=true` (WMS-C); JPEG is encoded with
   `jpeg-encoder` (not byte-identical to Pillow's libjpeg); the `/demo` UI.
3. Seeding: `refresh_before.time/mtime`, multiple coverages per task.

Deployment: [kartverket/tnt-rmapproxy](https://github.com/kartverket/tnt-rmapproxy) builds a
pinned commit of this repository with tnt-mapproxy's configuration.
