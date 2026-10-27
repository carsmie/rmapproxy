//! WMTS 1.0.0 (`service/wmts.py`): KVP on /service, REST under /wmts/1.0.0, capabilities.
//! Responses carry the headers tnt-mapproxy's WSGI wrapper adds (CORS) and MapProxy's
//! cache headers (`globals.tiles.expires_hours`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tracing::{info, warn};

use crate::config::LayerConf;
use crate::grid::TileGrid;
use crate::tile::{CacheGrid, TileManager};

#[derive(Clone)]
struct App {
    mgr: Arc<TileManager>,
    /// WMTS layer name -> its cache (layers with exactly one cache source).
    layers: Arc<HashMap<String, (LayerConf, String)>>,
    /// WMS from the caches, when `services.wms` is configured.
    wms: Option<Arc<crate::wms::Wms>>,
}

pub async fn serve(mgr: Arc<TileManager>, bind: &str, admin: Option<crate::admin::AdminOpts>) -> anyhow::Result<()> {
    let router = app(mgr, admin)?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!("listening on {bind}");
    axum::serve(listener, router).with_graceful_shutdown(shutdown_signal()).await?;
    info!("stopped");
    Ok(())
}

/// All routes of `serve`; with `admin`, the admin API too (which starts its background tasks,
/// so this needs a runtime).
pub fn app(mgr: Arc<TileManager>, admin: Option<crate::admin::AdminOpts>) -> anyhow::Result<Router> {
    let mut layers = HashMap::new();
    for l in &mgr.cfg.layers {
        match l.sources.as_slice() {
            [c] if mgr.cfg.caches.contains_key(c) => {
                layers.insert(l.name.clone(), (l.clone(), c.clone()));
            }
            _ => warn!("layer {}: only single-cache layers are served over WMTS", l.name),
        }
    }
    let wms = crate::wms::Wms::new(mgr.clone()).map(Arc::new);
    let admin = admin.map(|o| crate::admin::Admin::new(mgr.clone(), o)).transpose()?;
    let app = App { mgr, layers: Arc::new(layers), wms };
    let mut router = Router::new()
        .route("/service", get(kvp))
        .route("/wmts", get(kvp))
        .route("/wms", get(wms_path))
        .route("/wmts/1.0.0/WMTSCapabilities.xml", get(rest_caps))
        .route("/wmts/1.0.0/{layer}/{style}/{set}/{z}/{row}/{file}", get(rest_tile))
        .route("/legend/{file}", get(legend))
        .route("/healthz", get(|| async { "ok\n" }))
        .fallback(|| async { ows_error(StatusCode::NOT_FOUND, "NotFound", "unknown path") })
        .with_state(app)
        // The open CORS headers are for the map services only, never for /_admin.
        .layer(axum::middleware::from_fn(cors));
    // `serve --admin` only: the serving replicas never expose /_admin.
    if let Some(a) = admin {
        info!("admin API on /_admin");
        router = router.merge(crate::admin::router(a));
    }
    Ok(router.layer(axum::middleware::from_fn(access_log)))
}

/// SIGTERM (pod shutdown) or SIGINT: in-flight requests finish, new connections are refused.
pub async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
        return std::future::pending().await;
    };
    tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
    info!("shutting down");
}

/// tnt-mapproxy's `/legend/topo-<scale>.png`: files the admin app writes to
/// MAPPROXY_LEGEND_DIR (default `<cache base_dir>/legend`).
async fn legend(State(app): State<App>, Path(file): Path<String>) -> Response {
    let ok = file.strip_prefix("topo-").and_then(|f| f.strip_suffix(".png")).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let dir = std::env::var_os("MAPPROXY_LEGEND_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| app.mgr.cfg.globals.base_dir.join("legend"));
    match ok.then(|| std::fs::read(dir.join(&file))) {
        Some(Ok(data)) => (
            [(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=3600")],
            data,
        )
            .into_response(),
        _ => (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain")], "Not found").into_response(),
    }
}

/// One line per request (target `rmapproxy::access`); /healthz probes are skipped.
async fn access_log(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let t0 = std::time::Instant::now();
    let uri = req.uri().clone();
    if uri.path() == "/healthz" {
        return next.run(req).await;
    }
    let method = req.method().clone();
    let (remote, ua) = {
        let h = |n: &str| req.headers().get(n).and_then(|v| v.to_str().ok()).unwrap_or("-").to_string();
        (h("x-forwarded-for"), h("user-agent"))
    };
    let resp = next.run(req).await;
    // Content-Length is only set later by hyper; the body knows its size.
    let bytes = axum::body::HttpBody::size_hint(resp.body()).exact().map_or("-".into(), |n| n.to_string());
    info!(
        target: "rmapproxy::access",
        "{method} {uri} {} {bytes} {}ms remote={remote} ua={ua:?}",
        resp.status().as_u16(),
        t0.elapsed().as_millis()
    );
    resp
}

async fn cors(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut resp = if req.method() == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let h = resp.headers_mut();
    h.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    h.insert("access-control-allow-methods", HeaderValue::from_static("GET, OPTIONS"));
    h.insert("access-control-allow-headers", HeaderValue::from_static("*"));
    h.insert("access-control-allow-private-network", HeaderValue::from_static("true"));
    resp
}

fn ows_error(status: StatusCode, code: &str, text: &str) -> Response {
    let body = format!(
        "<?xml version=\"1.0\"?>\n<ExceptionReport xmlns=\"http://www.opengis.net/ows/1.1\" version=\"1.1.0\">\n  \
         <Exception exceptionCode=\"{code}\">\n    <ExceptionText>{}</ExceptionText>\n  </Exception>\n</ExceptionReport>\n",
        xml_escape(text)
    );
    (status, [(header::CONTENT_TYPE, "text/xml")], body).into_response()
}

async fn kvp(State(app): State<App>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    let q: HashMap<String, String> = q.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();
    let p = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
    if p("service").eq_ignore_ascii_case("wms") {
        return wms_kvp(&app, &q, &headers, "/service").await;
    }
    if !p("service").eq_ignore_ascii_case("wmts") {
        return ows_error(StatusCode::BAD_REQUEST, "MissingParameterValue", "service must be WMTS");
    }
    match p("request").to_ascii_lowercase().as_str() {
        "gettile" => {
            let fmt = p("format");
            if !fmt.is_empty() && !fmt.eq_ignore_ascii_case("image/png") && !fmt.eq_ignore_ascii_case("png") {
                return ows_error(StatusCode::BAD_REQUEST, "InvalidParameterValue", "unsupported format");
            }
            tile(&app, p("layer"), p("tilematrixset"), p("tilematrix"), p("tilerow"), p("tilecol")).await
        }
        "getcapabilities" => caps(&app, &headers, false),
        r => ows_error(StatusCode::BAD_REQUEST, "OperationNotSupported", &format!("unknown request {r}")),
    }
}

/// `/wms`: SERVICE defaults to WMS.
async fn wms_path(State(app): State<App>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    let q: HashMap<String, String> = q.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();
    wms_kvp(&app, &q, &headers, "/wms").await
}

async fn wms_kvp(app: &App, q: &HashMap<String, String>, headers: &HeaderMap, path: &str) -> Response {
    match &app.wms {
        Some(w) => w.handle(q, &format!("{}{path}", request_base(headers))).await,
        None => ows_error(StatusCode::BAD_REQUEST, "InvalidParameterValue", "WMS is not configured"),
    }
}

async fn rest_tile(
    State(app): State<App>,
    Path((layer, _style, set, z, row, file)): Path<(String, String, String, String, String, String)>,
) -> Response {
    let Some(col) = file.strip_suffix(".png") else {
        return ows_error(StatusCode::BAD_REQUEST, "InvalidParameterValue", "unsupported format");
    };
    tile(&app, &layer, &set, &z, &row, col).await
}

async fn rest_caps(State(app): State<App>, headers: HeaderMap) -> Response {
    caps(&app, &headers, true)
}

#[allow(clippy::result_large_err)] // the error is the ready response
fn resolve(app: &App, layer: &str, set: &str) -> Result<Arc<CacheGrid>, Response> {
    let (_, cache) = app
        .layers
        .get(layer)
        .ok_or_else(|| ows_error(StatusCode::BAD_REQUEST, "InvalidParameterValue", &format!("unknown layer: {layer}")))?;
    app.mgr
        .cache_grid(cache, set)
        .ok_or_else(|| ows_error(StatusCode::BAD_REQUEST, "InvalidParameterValue", &format!("unknown tilematrixset: {set}")))
}

async fn tile(app: &App, layer: &str, set: &str, z: &str, row: &str, col: &str) -> Response {
    let cg = match resolve(app, layer, set) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let (Ok(z), Ok(row), Ok(col)) = (z.parse::<usize>(), row.parse::<i64>(), col.parse::<i64>()) else {
        return ows_error(StatusCode::BAD_REQUEST, "InvalidParameterValue", "invalid tile coordinates");
    };
    let g = &cg.meta.grid;
    if z >= g.res.len() {
        return ows_error(StatusCode::BAD_REQUEST, "TileOutOfRange", "invalid tilematrix");
    }
    // WMTS rows count from the top; MapProxy's internal y does too only for nw grids.
    let y = if g.flipped { row } else { g.grid_sizes[z].1 - 1 - row };
    let coord = (col, y, z);
    if !g.contains(coord) {
        return ows_error(StatusCode::BAD_REQUEST, "TileOutOfRange", "tile out of range");
    }
    match app.mgr.tile(&cg, coord).await {
        Ok(t) => {
            let mut r = Response::new(Body::from(t.data));
            let h = r.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            if let Some(hours) = app.mgr.cfg.globals.expires_hours {
                let secs = (hours * 3600.0) as u64;
                if let Ok(v) = HeaderValue::from_str(&format!("public, max-age={secs}, s-maxage={secs}")) {
                    h.insert(header::CACHE_CONTROL, v);
                }
            }
            if let Some(m) = t.mtime && let Ok(v) = HeaderValue::from_str(&httpdate::fmt_http_date(m)) {
                h.insert(header::LAST_MODIFIED, v);
            }
            r
        }
        Err(e) => {
            warn!("{layer}/{set}/{z}/{row}/{col}: {e:#}");
            ows_error(StatusCode::INTERNAL_SERVER_ERROR, "NoApplicableCode", &format!("{e:#}"))
        }
    }
}

/// The request's own base URL (X-Forwarded-Proto/-Host first, as MapProxy behind nginx).
fn request_base(headers: &HeaderMap) -> String {
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(String::from);
    let proto = h("x-forwarded-proto").unwrap_or_else(|| "http".into());
    let host = h("x-forwarded-host").or_else(|| h("host")).unwrap_or_else(|| "localhost".into());
    format!("{proto}://{host}")
}

/// Public base URL: MAPPROXY_PUBLIC_URL / services online_resource, else from the request.
fn base_url(app: &App, headers: &HeaderMap) -> String {
    let or = app.mgr.cfg.wmts.online_resource.trim_end_matches("/service").trim_end_matches('/');
    if or.is_empty() { request_base(headers) } else { or.to_string() }
}

fn caps(app: &App, headers: &HeaderMap, restful: bool) -> Response {
    let base = base_url(app, headers);
    let cfg = &app.mgr.cfg;
    let mut layers_xml = String::new();
    let mut sets: Vec<&TileGrid> = vec![];
    let mut names: Vec<&String> = app.layers.keys().collect();
    names.sort_by_key(|n| cfg.layers.iter().position(|l| &&l.name == n));
    for name in names {
        let (l, cache) = &app.layers[name];
        let c = &cfg.caches[cache];
        let grids: Vec<&TileGrid> = c.grids.iter().filter_map(|g| app.mgr.cache_grid(cache, g).map(|_| &cfg.grids[g])).collect();
        if grids.is_empty() {
            continue;
        }
        let ll = wgs84_bbox(grids[0]);
        let mut links = String::new();
        for g in &grids {
            links.push_str(&format!("      <TileMatrixSetLink><TileMatrixSet>{}</TileMatrixSet></TileMatrixSetLink>\n", g.name));
            if !sets.iter().any(|s| s.name == g.name) {
                sets.push(g);
            }
        }
        layers_xml.push_str(&format!(
            "    <Layer>\n      <ows:Title>{}</ows:Title>\n      <ows:Abstract>{}</ows:Abstract>\n      \
             <ows:WGS84BoundingBox>\n        <ows:LowerCorner>{} {}</ows:LowerCorner>\n        \
             <ows:UpperCorner>{} {}</ows:UpperCorner>\n      </ows:WGS84BoundingBox>\n      \
             <ows:Identifier>{}</ows:Identifier>\n      <Style>\n        <ows:Identifier>default</ows:Identifier>\n      </Style>\n      \
             <Format>image/png</Format>\n{links}      \
             <ResourceURL format=\"image/png\" resourceType=\"tile\" template=\"{base}/wmts/1.0.0/{}/{{Style}}/{{TileMatrixSet}}/{{TileMatrix}}/{{TileRow}}/{{TileCol}}.png\"/>\n    </Layer>\n",
            xml_escape(&l.title),
            xml_escape(&l.abstract_),
            ll[0], ll[1], ll[2], ll[3],
            xml_escape(&l.name),
            xml_escape(&l.name),
        ));
    }
    let mut sets_xml = String::new();
    for g in sets {
        let epsg = g.srs.code.trim_start_matches("EPSG:");
        sets_xml.push_str(&format!(
            "    <TileMatrixSet>\n      <ows:Identifier>{}</ows:Identifier>\n      <ows:SupportedCRS>urn:ogc:def:crs:EPSG::{epsg}</ows:SupportedCRS>\n",
            g.name
        ));
        let mpu = if g.srs.geographic { 2.0 * std::f64::consts::PI * 6378137.0 / 360.0 } else { 1.0 };
        // WMTS TopLeftCorner is the grid's upper-left corner whatever its internal origin.
        let (left, top) = (g.bbox[0], g.bbox[3]);
        for (z, res) in g.res.iter().enumerate() {
            sets_xml.push_str(&format!(
                "      <TileMatrix>\n        <ows:Identifier>{z:02}</ows:Identifier>\n        <ScaleDenominator>{}</ScaleDenominator>\n        \
                 <TopLeftCorner>{left} {top}</TopLeftCorner>\n        <TileWidth>{}</TileWidth>\n        <TileHeight>{}</TileHeight>\n        \
                 <MatrixWidth>{}</MatrixWidth>\n        <MatrixHeight>{}</MatrixHeight>\n      </TileMatrix>\n",
                res * mpu / 0.00028,
                g.tile_size.0,
                g.tile_size.1,
                g.grid_sizes[z].0,
                g.grid_sizes[z].1
            ));
        }
        sets_xml.push_str("    </TileMatrixSet>\n");
    }
    let w = &cfg.wmts;
    let ops = if restful {
        String::new()
    } else {
        let op = |n: &str| {
            format!(
                "    <ows:Operation name=\"{n}\">\n      <ows:DCP>\n        <ows:HTTP>\n          <ows:Get xlink:href=\"{base}/service?\">\n            \
                 <ows:Constraint name=\"GetEncoding\">\n              <ows:AllowedValues>\n                <ows:Value>KVP</ows:Value>\n              \
                 </ows:AllowedValues>\n            </ows:Constraint>\n          </ows:Get>\n        </ows:HTTP>\n      </ows:DCP>\n    </ows:Operation>\n"
            )
        };
        format!("  <ows:OperationsMetadata>\n{}{}  </ows:OperationsMetadata>\n", op("GetCapabilities"), op("GetTile"))
    };
    let body = format!(
        "<?xml version=\"1.0\"?>\n<Capabilities xmlns=\"http://www.opengis.net/wmts/1.0\" xmlns:ows=\"http://www.opengis.net/ows/1.1\" \
         xmlns:xlink=\"http://www.w3.org/1999/xlink\" xmlns:gml=\"http://www.opengis.net/gml\" version=\"1.0.0\">\n  \
         <ows:ServiceIdentification>\n    <ows:Title>{}</ows:Title>\n    <ows:Abstract>{}</ows:Abstract>\n    \
         <ows:ServiceType>OGC WMTS</ows:ServiceType>\n    <ows:ServiceTypeVersion>1.0.0</ows:ServiceTypeVersion>\n  </ows:ServiceIdentification>\n\
         {ops}  <Contents>\n{layers_xml}{sets_xml}  </Contents>\n</Capabilities>\n",
        xml_escape(&w.title),
        xml_escape(&w.abstract_)
    );
    (StatusCode::OK, [(header::CONTENT_TYPE, "application/xml")], body).into_response()
}

fn wgs84_bbox(g: &TileGrid) -> [f64; 4] {
    let wgs = crate::srs::Srs::new("EPSG:4326").expect("EPSG:4326");
    g.srs.transform_bbox(&wgs, g.bbox).map(|b| [b[0].max(-180.0), b[1].max(-90.0), b[2].min(180.0), b[3].min(90.0)])
        .unwrap_or([-180.0, -90.0, 180.0, 90.0])
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
