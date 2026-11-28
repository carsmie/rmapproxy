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
    /// Answer this many next GetMaps with 503, like a mesh sidecar for a dead pod.
    fail_next: AtomicUsize,
}

const DELAY: Duration = Duration::from_millis(200);

async fn getmap(State(f): State<Arc<FakeWms>>, Query(q): Query<HashMap<String, String>>) -> axum::response::Response {
    f.total.fetch_add(1, Ordering::SeqCst);
    // fetch_update is deprecated (→ try_update) on stable 1.99, but try_update is newer than the MSRV.
    #[allow(deprecated)]
    let fail = f.fail_next.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok();
    if fail {
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "upstream connect error").into_response();
    }
    let n = f.inflight.fetch_add(1, Ordering::SeqCst) + 1;
    f.max_inflight.fetch_max(n, Ordering::SeqCst);
    tokio::time::sleep(DELAY).await;
    f.inflight.fetch_sub(1, Ordering::SeqCst);
    let dim = |k: &str| q.get(k).and_then(|v| v.parse().ok()).unwrap_or(1);
    let img = Rgba::filled(dim("width"), dim("height"), [16, 128, 240, 255]);
    ([(header::CONTENT_TYPE, "image/png")], encode_png_rgba(&img, crate::image::PNG_BALANCED).unwrap()).into_response()
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
async fn transient_503s_are_retried_until_backoff_runs_out() {
    let env = Env::new("retry").await;
    let base = spawn(crate::server::app(env.mgr(0), None).unwrap()).await;
    // Two failures: retried, the metatile is rendered and stored.
    env.wms.fail_next.store(2, Ordering::SeqCst);
    assert_eq!(get_all(vec![tile_url(&base, 0, 0)]).await, vec![200]);
    assert_eq!((env.requests(), env.cached_tiles()), (4, 16));
    // A source failing on every attempt (1 + 2 retries) fails the metatile, nothing stored.
    env.wms.fail_next.store(usize::MAX, Ordering::SeqCst);
    let status = get_all(vec![tile_url(&base, 0, 4)]).await[0];
    assert_ne!(status, 200);
    let failed = env.requests() - 4;
    assert!((3..=6).contains(&failed), "{failed} requests");
    assert_eq!(env.cached_tiles(), 16);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_tiles_do_not_leak_backend_details() {
    let env = Env::new("leak").await;
    let base = spawn(crate::server::app(env.mgr(0), None).unwrap()).await;
    env.wms.fail_next.store(usize::MAX, Ordering::SeqCst);
    let r = reqwest::get(tile_url(&base, 0, 0)).await.unwrap();
    assert_eq!(r.status(), 500);
    let body = r.text().await.unwrap();
    assert!(body.contains(crate::server::INTERNAL_ERROR), "{body}");
    // The error chain names the source URL (fake WMS on 127.0.0.1) and its 503 body.
    assert!(!body.contains("127.0.0.1") && !body.contains("503") && !body.contains("source s1"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capabilities_escape_a_forged_host() {
    let env = Env::new("caps").await;
    let base = spawn(crate::server::app(env.mgr(0), None).unwrap()).await;
    let client = reqwest::Client::new();
    for path in ["/wmts/1.0.0/WMTSCapabilities.xml", "/service?SERVICE=WMTS&REQUEST=GetCapabilities"] {
        let body = client.get(format!("{base}{path}")).header("x-forwarded-host", "evil\"><x y=\"").send().await.unwrap().text().await.unwrap();
        assert!(!body.contains("evil\"><x"), "{path}: {body}");
        assert!(body.contains("evil&quot;&gt;&lt;x y=&quot;"), "{path}: {body}");
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
        login: None,
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
async fn admin_login_guards_everything_but_the_login_page() {
    let env = Env::new("login").await;
    std::fs::create_dir_all(env.dir.join("admin")).unwrap();
    for f in ["admin.html", "admin.js", "admin.css"] {
        std::fs::write(env.dir.join("admin").join(f), "gui").unwrap();
    }
    let opts = crate::admin::AdminOpts {
        mapproxy_conf: env.dir.join("mapproxy.yaml"),
        seed_conf: env.dir.join("seed.yaml"),
        assets: env.dir.join("admin"),
        login: Some(crate::login::Login::new("hemmelig".into())),
    };
    let base = spawn(crate::server::app(env.mgr(0), Some(opts)).unwrap()).await;
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let get = |path: &str, cookie: &str| client.get(format!("{base}{path}")).header(header::COOKIE, cookie).send();
    let post = |path: &str, cookie: &str, body: &'static str| {
        client.post(format!("{base}{path}")).header(header::COOKIE, cookie).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded").body(body).send()
    };

    for p in ["/_admin", "/_admin/"] {
        let r = get(p, "").await.unwrap();
        assert_eq!((r.status().as_u16(), &r.headers()[header::LOCATION]), (303, &"/_admin/login".parse().unwrap()));
    }
    for p in ["status", "tasks", "history", "jobs/x", "admin.html", "admin.js", "%61dmin.html"] {
        assert_eq!(get(&format!("/_admin/{p}"), "rmapproxy_admin=guess").await.unwrap().status(), 401, "{p}");
    }
    // Without a worker token the work API is closed too (it shares the ingress).
    for p in ["jobs", "estimate", "jobs/x/stop", "stats/refresh", "legends", "work/claim", "work/complete"] {
        assert_eq!(post(&format!("/_admin/{p}"), "", "{}").await.unwrap().status(), 401, "{p}");
    }
    for p in ["/_admin/login", "/_admin/admin.css", "/healthz"] {
        assert_eq!(get(p, "").await.unwrap().status(), 200, "{p}");
    }
    let r = post("/_admin/login", "", "password=feil").await.unwrap();
    assert_eq!((r.status().as_u16(), r.headers().get(header::SET_COOKIE)), (401, None));

    let r = post("/_admin/login", "", "username=admin&password=hemmelig").await.unwrap();
    assert_eq!(r.status(), 303);
    let set = r.headers()[header::SET_COOKIE].to_str().unwrap().to_string();
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Lax"), "{set}");
    let cookie = set.split(';').next().unwrap();
    assert_eq!(get("/_admin", cookie).await.unwrap().status(), 200);
    assert_eq!(get("/_admin/status", cookie).await.unwrap().status(), 200);
    assert_eq!(post("/_admin/stats/refresh", cookie, "").await.unwrap().status(), 200);
    assert_eq!(get("/_admin/login", cookie).await.unwrap().status(), 303);
    // A session is no worker token, and logout ends it on the server.
    assert_eq!(post("/_admin/work/claim", cookie, "{}").await.unwrap().status(), 401);
    assert_eq!(post("/_admin/logout", cookie, "").await.unwrap().status(), 303);
    assert_eq!(get("/_admin/status", cookie).await.unwrap().status(), 401);
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

/// POST JSON to the admin; (status, parsed body or Null).
async fn admin_post(base: &str, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let r = reqwest::Client::new()
        .post(format!("{base}/_admin/{path}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, serde_json::from_slice(&r.bytes().await.unwrap()).unwrap_or_default())
}

/// Run a one-chunk distributed job of task `s` with a fake worker whose chunk round `n`
/// (1-based) reports `errors(n)` failed metatiles; (rounds claimed, final job state).
async fn run_distributed(base: &str, errors: impl Fn(u32) -> u64) -> (u32, String) {
    let (_, job) = admin_post(base, "jobs", serde_json::json!({"tasks": [{"name": "s", "levels_from": 1, "levels_to": 1}], "distributed": true})).await;
    let id = job["id"].as_str().expect("job id").to_string();
    let mut rounds = 0;
    loop {
        let (status, c) = admin_post(base, "work/claim", serde_json::json!({"worker": "w"})).await;
        if status == 204 {
            break;
        }
        rounds += 1;
        let body = serde_json::json!({"worker": "w", "job": id, "chunk": c["chunk"], "tiles": 16, "errors": errors(rounds), "metatiles": 2});
        admin_post(base, "work/complete", body).await;
    }
    let r = reqwest::get(format!("{base}/_admin/jobs/{id}")).await.unwrap().bytes().await.unwrap();
    let j: serde_json::Value = serde_json::from_slice(&r).unwrap();
    (rounds, j["state"].as_str().unwrap_or_default().to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distributed_failed_chunks_are_retried_before_the_job_fails() {
    let env = Env::new("autoretry").await;
    let opts = crate::admin::AdminOpts {
        mapproxy_conf: env.dir.join("mapproxy.yaml"),
        seed_conf: env.dir.join("seed.yaml"),
        assets: env.dir.join("admin"),
        login: None,
    };
    let base = spawn(crate::server::app(env.mgr(0), Some(opts)).unwrap()).await;
    // Source down for two rounds (rGIS OOM storm), then fine: the job ends done.
    assert_eq!(run_distributed(&base, |n| (n <= 2) as u64).await, (3, "done".into()));
    // Down on every round: two automatic retries, then failed (Fortsett is left).
    assert_eq!(run_distributed(&base, |_| 1).await, (3, "failed".into()));
}

/// A finished distributed job records per-level worker time and wall time in the history, and
/// the estimate then prices that (cache, level) from it (`basis: levels`) with the ramp added.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distributed_history_feeds_the_level_estimate() {
    let env = Env::new("estmodel").await;
    let opts = crate::admin::AdminOpts {
        mapproxy_conf: env.dir.join("mapproxy.yaml"),
        seed_conf: env.dir.join("seed.yaml"),
        assets: env.dir.join("admin"),
        login: None,
    };
    let base = spawn(crate::server::app(env.mgr(0), Some(opts)).unwrap()).await;
    let task = serde_json::json!({"tasks": [{"name": "s", "levels_from": 1, "levels_to": 1}]});
    let (_, before) = admin_post(&base, "estimate", task.clone()).await;
    assert_ne!(before["basis"], "levels");
    let mut req = task.clone();
    req["distributed"] = true.into();
    let (_, job) = admin_post(&base, "jobs", req).await;
    let id = job["id"].as_str().unwrap().to_string();
    loop {
        let (status, c) = admin_post(&base, "work/claim", serde_json::json!({"worker": "w"})).await;
        if status == 204 {
            break;
        }
        let body = serde_json::json!({"worker": "w", "job": id, "chunk": c["chunk"], "tiles": 600, "errors": 0, "metatiles": 150, "seconds": 30.0});
        admin_post(&base, "work/complete", body).await;
    }
    let h: serde_json::Value = serde_json::from_slice(&reqwest::get(format!("{base}/_admin/history")).await.unwrap().bytes().await.unwrap()).unwrap();
    let last = h.as_array().unwrap().iter().rev().find(|e| e["key"] == id.as_str()).expect("history entry");
    assert!(last["wall_seconds"].as_f64().unwrap() >= 0.0, "{last}");
    assert_eq!(last["levels"][0]["z"], 1);
    assert!(last["levels"][0]["metatiles"].as_u64().unwrap() >= 150, "{last}");
    let (st, e) = admin_post(&base, "estimate", task).await;
    assert_eq!((st, &e["basis"]), (200, &serde_json::json!("levels")), "{e}");
    assert_eq!(e["model_jobs"], 1);
    let (lo, mid, hi) = (e["seconds_low"].as_f64().unwrap(), e["seconds"].as_f64().unwrap(), e["seconds_high"].as_f64().unwrap());
    assert!(lo <= mid && mid <= hi, "{e}");
}

/// Bbox tasks on every grid of the cache, and refresh orders: levels split at `delete_from`,
/// bad requests start nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refresh_orders_split_levels_and_cover_all_grids() {
    use serde_json::json;
    let env = Env::new("order").await;
    let conf = env.dir.join("mapproxy.yaml");
    let y = std::fs::read_to_string(&conf).unwrap();
    let y = y.replace("caches:\n  c: {grids: [g]", "  g32: {srs: 'EPSG:25832', origin: nw, bbox: [-2000000, 3500000, 3545984, 9045984], tile_size: [256, 256], res: [21664, 10832]}\ncaches:\n  c: {grids: [g, g32]");
    std::fs::write(&conf, y).unwrap();
    let seed = std::fs::read_to_string(env.dir.join("seed.yaml")).unwrap();
    std::fs::write(env.dir.join("seed.yaml"), seed.replace("s: {caches: [c],", "s: {caches: [c], grids: [g],")).unwrap();
    let opts = crate::admin::AdminOpts {
        mapproxy_conf: conf.clone(),
        seed_conf: env.dir.join("seed.yaml"),
        assets: env.dir.join("admin"),
        login: None,
    };
    let base = spawn(crate::server::app(env.mgr(0), Some(opts)).unwrap()).await;
    let bbox = json!([5.0, 1.0, 6.0, 2.0]);
    let grids_of = |v: &serde_json::Value| -> Vec<String> { v["tasks"].as_array().unwrap().iter().map(|t| t["grid"].as_str().unwrap().to_string()).collect() };
    let t = |g: serde_json::Value| json!({"tasks": [{"name": "a", "bbox": bbox, "caches": ["c"], "grids": g}]});
    for (g, want) in [(json!(null), vec!["g"]), (json!("all"), vec!["g", "g32"]), (json!(["g32"]), vec!["g32"])] {
        let (st, e) = admin_post(&base, "estimate", t(g)).await;
        assert_eq!((st, grids_of(&e)), (200, want.iter().map(|s| s.to_string()).collect()));
    }
    assert_eq!(admin_post(&base, "estimate", t(json!(["nope"]))).await.0, 400);
    assert_eq!(admin_post(&base, "estimate", t(json!([]))).await.0, 400);

    let ok = json!({"name": "a", "bbox": bbox, "caches": ["c"], "grids": "all", "levels_from": 0, "levels_to": 1});
    let with = |k: &str, v: serde_json::Value| {
        let mut x = ok.clone();
        x[k] = v;
        x
    };
    let order = |tasks: serde_json::Value| json!({"mode": "refresh", "delete_from": 2, "distributed": true, "tasks": tasks});
    for bad in [
        json!([]),
        json!([ok, ok]),
        json!([ok, {"name": "b", "bbox": [5, 1, 6], "caches": ["c"]}]),
        json!([with("caches", json!(["c", "nope"]))]),
        json!([with("caches", json!(null))]),
        json!([with("name", json!("s"))]),
        json!([with("delete_from", json!("x"))]),
        json!([with("levels_from", json!(2))]),
        json!([with("grids", json!(["nope"]))]),
    ] {
        let (st, e) = admin_post(&base, "jobs", order(bad.clone())).await;
        assert_eq!(st, 400, "{bad} -> {e}");
    }
    let mut no_split = order(json!([ok]));
    no_split["delete_from"] = json!(null);
    assert_eq!(admin_post(&base, "jobs", no_split).await.0, 400);
    let get = |path: String| async move { serde_json::from_slice::<serde_json::Value>(&reqwest::get(path).await.unwrap().bytes().await.unwrap()).unwrap() };
    assert_eq!(get(format!("{base}/_admin/status")).await["jobs"], json!([]));

    let (st, o) = admin_post(&base, "jobs", order(json!([ok]))).await;
    assert_eq!((st, o["jobs"].as_array().map(Vec::len)), (200, Some(1)), "{o}");
    let url = format!("{base}/_admin/orders/{}", o["id"].as_str().unwrap());
    let v = get(url.clone()).await;
    assert_eq!((&v["state"], v["jobs"][0]["progress"].as_array().map(Vec::len)), (&json!("running"), Some(2)), "{v}");
    loop {
        let (st, c) = admin_post(&base, "work/claim", json!({"worker": "w"})).await;
        if st == 204 {
            break;
        }
        admin_post(&base, "work/complete", json!({"worker": "w", "job": c["job"].as_str().unwrap_or(o["jobs"][0].as_str().unwrap()), "chunk": c["chunk"], "tiles": 1, "errors": 0, "metatiles": 1})).await;
    }
    assert_eq!(get(url).await["state"], "done");
}

/// Renders, coalesced waits, source retries and routes are counted; client input is not a label.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_count_tiles_and_sources_without_client_labels() {
    let env = Env::new("metrics").await;
    let base = spawn(crate::server::app(env.mgr(0), None).unwrap()).await;
    let tile = tile_url(&base, 0, 0);
    env.wms.fail_next.store(1, Ordering::SeqCst);
    assert_eq!(get_all(vec![tile.clone(), tile.clone()]).await, vec![200, 200]);
    assert_eq!(get_all(vec![tile]).await, vec![200]);
    reqwest::get(format!("{base}/minted-path-label")).await.unwrap();
    reqwest::get(format!("{base}/healthz")).await.unwrap();

    let metrics = spawn(crate::metrics::router()).await;
    let text = reqwest::get(format!("{metrics}/metrics")).await.unwrap().text().await.unwrap();
    for want in [
        r#"rmapproxy_tile_requests_total{cache="c",grid="g",result="miss",zoom="1"} "#,
        r#"rmapproxy_tile_requests_total{cache="c",grid="g",result="coalesced",zoom="1"} "#,
        r#"rmapproxy_tile_requests_total{cache="c",grid="g",result="hit",zoom="1"} "#,
        r#"rmapproxy_metatiles_total{cache="c",grid="g",result="stored"} "#,
        r#"rmapproxy_source_requests_total{result="retry",source="s1"} "#,
        r#"rmapproxy_http_requests_total{route="/wmts/1.0.0/{layer}/{style}/{set}/{z}/{row}/{file}",status="200"} "#,
        r#"rmapproxy_http_requests_total{route="unmatched",status="404"} "#,
    ] {
        // s1 or s2 draws the injected 503, whichever asks first.
        let want_alt = want.replace("s1", "s2");
        assert!(text.contains(want) || text.contains(&want_alt), "{want}\n{text}");
    }
    assert!(!text.contains("minted-path-label") && !text.contains("healthz"), "{text}");
    assert!(reqwest::get(format!("{base}/metrics")).await.unwrap().text().await.unwrap().contains("ExceptionReport"));
}

/// Sets a path's mtime (files and directories) `days_ago` days back.
fn set_age(p: &Path, days_ago: u64) {
    let t = std::time::SystemTime::now() - Duration::from_secs(days_ago * 86400);
    let s = t.duration_since(std::time::UNIX_EPOCH).unwrap();
    let ts = libc::timespec { tv_sec: s.as_secs() as libc::time_t, tv_nsec: 0 };
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [ts, ts].as_ptr(), 0) }, 0, "{}", p.display());
}

/// A cache on disk without sources: grid `g` (EPSG:3857, 16 px tiles, 8 levels; level 7 =
/// 2048² tiles = 262k 4×4 metatiles, past seed.rs's walk threshold; level 1 is planned), coverage `box` and
/// `outside` = difference(grid, box), cleanup `sweep` (outside, older than 3 days). Level 7:
/// `old_out` (2000,2000) 10 d, `old_in` (100,30, inside box) 10 d, `new_out` (2000,2001) fresh,
/// `link_quiet` (1500,1500) hardlink to a 10-day-old colour file in a 10-day-old dir,
/// `link_busy` (1600,1600) the same link in a fresh dir. Level 1: `l1_old_out` (31,31) 10 d.
pub(crate) struct SweepFixture {
    pub dir: PathBuf,
    pub cache: PathBuf,
}

impl SweepFixture {
    pub fn new(name: &str) -> SweepFixture {
        let dir = std::env::temp_dir().join(format!("rmapproxy-sweep-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mapproxy.yaml"),
            "globals:\n  cache: {base_dir: ./cache}\n\
             services:\n  wms: {}\n\
             grids:\n  g: {srs: 'EPSG:3857', origin: nw, bbox: [0, 0, 1048576, 1048576], tile_size: [16, 16], \
             res: [4096, 2048, 1024, 512, 256, 128, 64, 32]}\n\
             caches:\n  c: {grids: [g], sources: [s1], meta_size: [4, 4], meta_buffer: 0}\n\
             sources:\n  s1: {type: wms, req: {url: 'http://127.0.0.1:9/', layers: a, transparent: true}}\n\
             layers:\n  - {name: l, title: L, sources: [c]}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("seed.yaml"),
            "coverages:\n  box: {bbox: [10000, 927504, 252144, 1038576], srs: 'EPSG:3857'}\n  \
             outside:\n    difference:\n      - {bbox: [0, 0, 1048576, 1048576], srs: 'EPSG:3857'}\n      \
             - {bbox: [10000, 927504, 252144, 1038576], srs: 'EPSG:3857'}\n\
             cleanups:\n  sweep: {caches: [c], coverages: [outside], remove_before: {days: 3}}\n",
        )
        .unwrap();
        let cache = dir.join("cache/c_EPSG3857");
        let fc = crate::cache::FileCache::new(cache.clone(), dir.join("locks"), 1.0, true);
        let put = |c: crate::grid::TileCoord, days: u64| {
            let p = fc.tile_path(c);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"png").unwrap();
            set_age(&p, days);
        };
        put((2000, 2000, 7), 10);
        put((100, 30, 7), 10);
        put((2000, 2001, 7), 0);
        put((31, 31, 1), 10);
        let colour = cache.join("single_color_tiles/0000ffff.png");
        std::fs::create_dir_all(colour.parent().unwrap()).unwrap();
        std::fs::write(&colour, b"png").unwrap();
        set_age(&colour, 10);
        for c in [(1500, 1500, 7), (1600, 1600, 7)] {
            let p = fc.tile_path(c);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::hard_link(&colour, &p).unwrap();
        }
        set_age(fc.tile_path((1500, 1500, 7)).parent().unwrap(), 10);
        SweepFixture { dir, cache }
    }

    pub fn exists(&self, c: crate::grid::TileCoord) -> bool {
        crate::cache::FileCache::new(self.cache.clone(), self.dir.join("locks"), 1.0, true).tile_path(c).exists()
    }

    pub fn mgr(&self) -> Arc<TileManager> {
        let cfg = Config::load(&self.dir.join("mapproxy.yaml")).unwrap();
        Arc::new(TileManager::with_max_renders(Arc::new(cfg), 0).unwrap())
    }
}

impl Drop for SweepFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// difference coverage sweep: level 7 by walking files, level 1 by planning; old tiles outside
/// the box go, the inside one and fresh ones stay; a hardlink is aged by its directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outside_sweep_walks_files_and_ages_hardlinks_by_dir() {
    let f = SweepFixture::new("outside");
    let mgr = f.mgr();
    let conf = crate::config::load_yaml_with_bases(&f.dir.join("seed.yaml")).unwrap();
    let tasks = crate::seed::load_tasks_from(&mgr, &conf, &f.dir, &["sweep".into()], crate::seed::Mode::Cleanup).unwrap();
    crate::seed::run(mgr, tasks, crate::seed::Mode::Cleanup, 2, None).await.unwrap();
    assert!(!f.exists((2000, 2000, 7)), "old tile outside");
    assert!(f.exists((100, 30, 7)), "old tile inside the box");
    assert!(f.exists((2000, 2001, 7)), "fresh tile outside");
    assert!(!f.exists((1500, 1500, 7)), "hardlink in a quiet dir");
    assert!(f.exists((1600, 1600, 7)), "hardlink in a fresh dir");
    assert!(!f.exists((31, 31, 1)), "planned level");
    assert!(f.cache.join("single_color_tiles/0000ffff.png").exists());
}
