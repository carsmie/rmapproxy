//! End to end: `serve`'s routes and the seeder against a fake WMS that counts its requests.
//! Each test has its own fake WMS, server and cache directory (ports picked by the OS).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::header;
use axum::response::IntoResponse;

use crate::config::Config;
use crate::image::{Rgba, decode_png, encode_png_rgba};
use crate::tile::TileManager;

/// Answers every GetMap after `DELAY` with an opaque image of the requested size.
#[derive(Default)]
struct FakeWms {
    total: AtomicUsize,
    inflight: AtomicUsize,
    max_inflight: AtomicUsize,
}

const DELAY: Duration = Duration::from_millis(200);

async fn getmap(State(f): State<Arc<FakeWms>>, Query(q): Query<HashMap<String, String>>) -> axum::response::Response {
    f.total.fetch_add(1, Ordering::SeqCst);
    let n = f.inflight.fetch_add(1, Ordering::SeqCst) + 1;
    f.max_inflight.fetch_max(n, Ordering::SeqCst);
    tokio::time::sleep(DELAY).await;
    f.inflight.fetch_sub(1, Ordering::SeqCst);
    let dim = |k: &str| q.get(k).and_then(|v| v.parse().ok()).unwrap_or(1);
    let img = Rgba::filled(dim("width"), dim("height"), [16, 128, 240, 255]);
    ([(header::CONTENT_TYPE, "image/png")], encode_png_rgba(&img, png::Compression::Balanced).unwrap()).into_response()
}

/// Serve `router` on an ephemeral port; returns its base URL.
async fn spawn(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

struct Env {
    dir: PathBuf,
    wms: Arc<FakeWms>,
}

impl Env {
    /// A cache `c` of two sources on the fake WMS, in grid `g` (EPSG:3857, 16 px tiles,
    /// 4×4 metatiles without buffer; level 1 is 32×32 tiles), served as layer `l` over WMTS
    /// and WMS, and a seed.yaml with task `s` (two level-1 metatiles) and `s_refresh`.
    async fn new(name: &str) -> Env {
        let dir = std::env::temp_dir().join(format!("rmapproxy-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wms = Arc::new(FakeWms::default());
        let url = spawn(axum::Router::new().route("/", axum::routing::get(getmap)).with_state(wms.clone())).await;
        std::fs::write(
            dir.join("mapproxy.yaml"),
            format!(
                "globals:\n  cache: {{base_dir: ./cache}}\n\
                 services:\n  wms: {{}}\n\
                 grids:\n  g: {{srs: 'EPSG:3857', origin: nw, bbox: [0, 0, 1048576, 1048576], tile_size: [16, 16], res: [4096, 2048]}}\n\
                 caches:\n  c: {{grids: [g], sources: [s1, s2], meta_size: [4, 4], meta_buffer: 0}}\n\
                 sources:\n  s1: {{type: wms, req: {{url: '{url}/', layers: a, transparent: true}}}}\n  \
                 s2: {{type: wms, req: {{url: '{url}/', layers: b, transparent: true}}}}\n\
                 layers:\n  - {{name: l, title: L, sources: [c]}}\n"
            ),
        )
        .unwrap();
        // Level-1 metatiles are 131072 m wide; this box lies inside the top-left two.
        std::fs::write(
            dir.join("seed.yaml"),
            "coverages:\n  box: {bbox: [10000, 927504, 252144, 1038576], srs: 'EPSG:3857'}\n\
             seeds:\n  s: {caches: [c], coverages: [box], levels: [1]}\n  \
             s_refresh: {caches: [c], coverages: [box], levels: [1], refresh_before: {hours: 1}}\n",
        )
        .unwrap();
        Env { dir, wms }
    }

    fn mgr(&self, max_renders: usize) -> Arc<TileManager> {
        let cfg = Config::load(&self.dir.join("mapproxy.yaml")).unwrap();
        Arc::new(TileManager::with_max_renders(Arc::new(cfg), max_renders).unwrap())
    }

    fn requests(&self) -> usize {
        self.wms.total.load(Ordering::SeqCst)
    }

    fn cached_tiles(&self) -> usize {
        fn count(p: &Path) -> usize {
            std::fs::read_dir(p)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| if e.path().is_dir() { count(&e.path()) } else { (e.path().extension() == Some("png".as_ref())) as usize })
                .sum()
        }
        count(&self.dir.join("cache/c_EPSG3857"))
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn tile_url(base: &str, row: i64, col: i64) -> String {
    format!("{base}/wmts/1.0.0/l/default/g/1/{row}/{col}.png")
}

async fn get_all(urls: Vec<String>) -> Vec<u16> {
    let client = reqwest::Client::new();
    let reqs = urls.into_iter().map(|u| {
        let c = client.clone();
        async move { c.get(u).send().await.unwrap().status().as_u16() }
    });
    futures::future::join_all(reqs).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_tiles_of_one_metatile_render_it_once() {
    let env = Env::new("singleflight").await;
    let base = spawn(crate::server::app(env.mgr(0), None).unwrap()).await;
    let urls = (0..4).flat_map(|r| (0..4).map(move |c| (r, c))).map(|(r, c)| tile_url(&base, r, c)).collect();
    assert_eq!(get_all(urls).await, vec![200; 16]);
    // One request per source for the whole metatile, all 16 tiles stored.
    assert_eq!(env.requests(), 2);
    assert_eq!(env.cached_tiles(), 16);
    // From the cache now.
    assert_eq!(get_all(vec![tile_url(&base, 3, 3)]).await, vec![200]);
    assert_eq!(env.requests(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn render_limit_bounds_concurrent_metatiles() {
    for (limit, max_inflight) in [(1, 2), (0, 8)] {
        let env = Env::new(&format!("limit{limit}")).await;
        let base = spawn(crate::server::app(env.mgr(limit), None).unwrap()).await;
        // Four different metatiles at once, two sources each.
        let urls = [0, 4, 8, 12].iter().map(|&c| tile_url(&base, 0, c)).collect();
        assert_eq!(get_all(urls).await, vec![200; 4]);
        assert_eq!(env.requests(), 8);
        assert_eq!(env.wms.max_inflight.load(Ordering::SeqCst), max_inflight, "limit {limit}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wms_getmap_from_the_cache() {
    let env = Env::new("wms").await;
    let base = spawn(crate::server::app(env.mgr(0), None).unwrap()).await;
    let client = reqwest::Client::new();
    let getmap = |w: u32| format!("{base}/wms?REQUEST=GetMap&VERSION=1.1.1&LAYERS=l&SRS=EPSG:3857&BBOX=0,917504,131072,1048576&WIDTH={w}&HEIGHT=64&FORMAT=image/png");
    let r = client.get(getmap(64)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()[header::CONTENT_TYPE], "image/png");
    let (img, _) = decode_png(&r.bytes().await.unwrap()).unwrap();
    assert_eq!((img.w, img.h), (64, 64));
    assert_eq!(img.single_color(), Some([16, 128, 240, 255]));
    let r = client.get(getmap(0)).send().await.unwrap();
    assert!(r.text().await.unwrap().contains("MissingDimensionValue"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_has_no_cors_and_refuses_cross_site_posts() {
    let env = Env::new("admin").await;
    let opts = crate::admin::AdminOpts {
        mapproxy_conf: env.dir.join("mapproxy.yaml"),
        seed_conf: env.dir.join("seed.yaml"),
        assets: env.dir.join("admin"),
    };
    let base = spawn(crate::server::app(env.mgr(0), Some(opts)).unwrap()).await;
    let client = reqwest::Client::new();
    let acao = |r: &reqwest::Response| r.headers().get("access-control-allow-origin").cloned();

    let r = client.get(tile_url(&base, 0, 0)).send().await.unwrap();
    assert_eq!(acao(&r).unwrap(), "*");
    let r = client.get(format!("{base}/_admin/status")).send().await.unwrap();
    assert_eq!((r.status().as_u16(), acao(&r)), (200, None));

    let post = |site: &'static str| client.post(format!("{base}/_admin/stats/refresh")).header("sec-fetch-site", site).send();
    assert_eq!(post("cross-site").await.unwrap().status(), 403);
    assert_eq!(post("same-origin").await.unwrap().status(), 200);
    let r = client.post(format!("{base}/_admin/stats/refresh")).header("origin", "https://elsewhere.example").send().await.unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeding_renders_the_coverage_and_refresh_skips_fresh_tiles() {
    let env = Env::new("seed").await;
    let mgr = env.mgr(0);
    let conf = crate::config::load_yaml_with_bases(&env.dir.join("seed.yaml")).unwrap();
    let tasks = |name: &str| crate::seed::load_tasks_from(&mgr, &conf, &env.dir, &[name.to_string()], crate::seed::Mode::Seed).unwrap();

    crate::seed::run(mgr.clone(), tasks("s"), crate::seed::Mode::Seed, 4, None).await.unwrap();
    // Two metatiles × two sources; 2 × 16 tiles.
    assert_eq!((env.requests(), env.cached_tiles()), (4, 32));

    // refresh_before 1 h: everything is newer, nothing to render.
    crate::seed::run(mgr.clone(), tasks("s_refresh"), crate::seed::Mode::Seed, 4, None).await.unwrap();
    assert_eq!(env.requests(), 4);

    // Without refresh_before every metatile is rendered again.
    crate::seed::run(mgr.clone(), tasks("s"), crate::seed::Mode::Seed, 4, None).await.unwrap();
    assert_eq!(env.requests(), 8);
}
