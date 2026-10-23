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

## Status (2026-10-23)

Verified against MapProxy 7.0 (tnt-mapproxy image) on the same rendered config and the
same local rGIS: Oslo, `topo_cache`, z6–13, 816 tiles.

| | MapProxy 7.0 | rMapproxy |
|---|---|---|
| Tiles, files, single-colour hardlinks | 816 / 817 / 2 | identical set |
| Pixels | reference | **817 / 817 identical** |
| Wall time, `-c 4`, 4 dedicated cores | 107.9 s | 13.7–14.7 s |
| CPU (user + sys) | 121.5 s | 36–43 s |
| Peak RSS | 249 MB | 650 MB |

Why it's faster: the 33 sources of a metatile are fetched concurrently (MapProxy: one
after another), compositing is a table-driven port of Pillow's integer `alpha_composite`
with exact fast paths, tiles are encoded in parallel.

Done: config subset (base includes, merge keys, grids incl. `base:`), grid/metatile
geometry, coverages (bbox + GeoJSON polygons, strict/shapely intersect semantics),
resolution ranges, coverage sub-requests, `on_error` transparent/uncached, Pillow-exact
merge, `tc` file cache with atomic writes, single-colour hardlinks and MapProxy-compatible
flock files, WMTS GetTile KVP + REST + GetCapabilities, CORS/cache headers, seeding
(TileWalker order, coverage, levels, refresh_before / overwrite-all, `-c`).

## Not yet

1. **Paletted PNG** (`image.colors`, `encoding_options.quantizer`): `topo_cache_2x`
   (mediancut) and `qgis_hoydekurve_cache` are written as RGBA for now (warning logged).
   Needs a port of Pillow's quantizers (libImaging/Quant.c) for pixel parity.
2. Seeding: `--continue` / `--progress-file` (resume), `--cleanup`, `refresh_before.time/mtime`,
   progress percentages matching mapproxy-seed's.
3. Peak memory: decoded source images of all in-flight metatiles are held; decode into the
   composite incrementally or bound concurrency by megapixels.
4. Reprojection for `utm32n` / `utm35n` (those cache/grid pairs are skipped and not
   advertised); WMS service; legends.
5. Packaging: Dockerfile, the admin app calling `rmapproxy seed`, a tnt-apps deployment
   next to MapProxy for an A/B on dev.
