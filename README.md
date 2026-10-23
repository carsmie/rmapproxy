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

Note: MapProxy's mediancut path marks transparent pixels as palette index 255, which lies
outside the written palette/tRNS (or gets truncated to a lower bit depth), so those pixels
decode as opaque black. rMapproxy reproduces that byte for byte (parity); fixing it would
change tiles relative to MapProxy.

## Not yet

1. `fastoctree` quantizer (MapProxy's default, only `qgis_hoydekurve_cache` uses it): written
   as RGBA with a warning.
2. Reprojection for `utm32n` / `utm35n` (those cache/grid pairs are skipped and not
   advertised); WMS service; legends; the admin GUI (stays in tnt-mapproxy).
3. Seeding: `refresh_before.time/mtime`, multiple coverages per task.

Deployment: [kartverket/tnt-rmapproxy](https://github.com/kartverket/tnt-rmapproxy) builds a
pinned commit of this repository with tnt-mapproxy's configuration.
