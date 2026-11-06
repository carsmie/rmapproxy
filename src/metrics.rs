//! Prometheus metrics, for `serve` and `seed-worker`. Served on their own port
//! (RMAPPROXY_METRICS_ADDR), never the service port, which is behind a public ingress. Label
//! values come from the config or the router, never from client input: a raw path or layer
//! parameter would let anyone mint unlimited series.

use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use prometheus::{Histogram, HistogramVec, IntCounter, IntCounterVec, IntGauge, histogram_opts, register_histogram, register_histogram_vec, register_int_counter_vec, register_int_gauge};

const RENDER_BUCKETS: &[f64] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];

static HTTP_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!("rmapproxy_http_requests_total", "HTTP requests by route and status", &["route", "status"]).expect("static metric")
});
static HTTP_SECONDS: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        histogram_opts!("rmapproxy_http_request_duration_seconds", "HTTP request duration", vec![0.001, 0.005, 0.025, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]),
        &["route"]
    )
    .expect("static metric")
});
// With nginx in front, hits on existing tiles rarely get here: this mostly shows misses.
static TILES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rmapproxy_tile_requests_total",
        "Tile lookups: hit (cached), coalesced (waited for another render), miss (rendered)",
        &["cache", "grid", "zoom", "result"]
    )
    .expect("static metric")
});
static METATILES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rmapproxy_metatiles_total",
        "Metatile renders: stored, not_cached (a source failed, transparent), not_stored, error",
        &["cache", "grid", "result"]
    )
    .expect("static metric")
});
static TILES_RENDERED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!("rmapproxy_tiles_rendered_total", "Tiles out of successful metatile renders", &["cache", "grid"]).expect("static metric")
});
static METATILE_SECONDS: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(histogram_opts!("rmapproxy_metatile_render_seconds", "Metatile render time, sources to store", RENDER_BUCKETS.to_vec()), &["cache", "grid"]).expect("static metric")
});
static RENDER_WAIT: LazyLock<Histogram> = LazyLock::new(|| {
    register_histogram!(histogram_opts!("rmapproxy_render_queue_wait_seconds", "Wait for a RMAPPROXY_MAX_RENDERS slot", RENDER_BUCKETS.to_vec())).expect("static metric")
});
static RENDERS_IN_FLIGHT: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!("rmapproxy_renders_in_flight", "On-demand metatile renders running").expect("static metric")
});
static SOURCE_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rmapproxy_source_requests_total",
        "GetMap attempts: ok, retry (failed, will retry), error (gave up)",
        &["source", "result"]
    )
    .expect("static metric")
});
static SOURCE_SECONDS: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(histogram_opts!("rmapproxy_source_request_seconds", "GetMap attempt duration", RENDER_BUCKETS.to_vec()), &["source"]).expect("static metric")
});
static SOURCE_BYTES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!("rmapproxy_source_bytes_total", "Image bytes received from sources", &["source"]).expect("static metric")
});

pub fn http_request(route: &str, status: &str, took: Duration) {
    HTTP_REQUESTS.with_label_values(&[route, status]).inc();
    HTTP_SECONDS.with_label_values(&[route]).observe(took.as_secs_f64());
}

/// Tile lookups of one cache and grid. Resolved per zoom on first use: this is the hit path, and
/// creating them up front would publish a zero series for every zoom of every cache.
pub struct TileCounters {
    cache: String,
    grid: String,
    zooms: Vec<[OnceLock<IntCounter>; 3]>,
}

#[derive(Clone, Copy)]
pub enum TileResult {
    Hit,
    Coalesced,
    Miss,
}

impl TileCounters {
    pub fn new(cache: &str, grid: &str, levels: usize) -> Self {
        let zooms = (0..levels).map(|_| Default::default()).collect();
        TileCounters { cache: cache.into(), grid: grid.into(), zooms }
    }

    pub fn inc(&self, zoom: usize, result: TileResult) {
        let Some(slots) = self.zooms.get(zoom) else { return };
        slots[result as usize]
            .get_or_init(|| {
                let label = match result {
                    TileResult::Hit => "hit",
                    TileResult::Coalesced => "coalesced",
                    TileResult::Miss => "miss",
                };
                TILES.with_label_values(&[self.cache.as_str(), self.grid.as_str(), zoom.to_string().as_str(), label])
            })
            .inc();
    }
}

/// `result`: see [`METATILES`]; `tiles` is 0 for errors.
pub fn metatile(cache: &str, grid: &str, result: &str, tiles: usize, took: Duration) {
    METATILES.with_label_values(&[cache, grid, result]).inc();
    METATILE_SECONDS.with_label_values(&[cache, grid]).observe(took.as_secs_f64());
    if tiles > 0 {
        TILES_RENDERED.with_label_values(&[cache, grid]).inc_by(tiles as u64);
    }
}

pub fn render_queue_waited(took: Duration) {
    RENDER_WAIT.observe(took.as_secs_f64());
}

/// An on-demand render running until dropped, failures included.
pub struct RenderInFlight(());

pub fn render_in_flight() -> RenderInFlight {
    RENDERS_IN_FLIGHT.inc();
    RenderInFlight(())
}

impl Drop for RenderInFlight {
    fn drop(&mut self) {
        RENDERS_IN_FLIGHT.dec();
    }
}

/// `result`: ok, retry or error.
pub fn source_attempt(source: &str, result: &str, took: Duration, bytes: usize) {
    SOURCE_REQUESTS.with_label_values(&[source, result]).inc();
    SOURCE_SECONDS.with_label_values(&[source]).observe(took.as_secs_f64());
    if bytes > 0 {
        SOURCE_BYTES.with_label_values(&[source]).inc_by(bytes as u64);
    }
}

/// The metrics endpoint: `GET /metrics` in Prometheus text format.
pub fn router() -> axum::Router {
    axum::Router::new().route(
        "/metrics",
        axum::routing::get(|| async {
            use axum::response::IntoResponse;
            match prometheus::TextEncoder::new().encode_to_string(&prometheus::gather()) {
                Ok(text) => ([("content-type", prometheus::TEXT_FORMAT)], text).into_response(),
                Err(e) => {
                    tracing::warn!("metrics encoding failed: {e}");
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }),
    )
}

/// Serve `router` on RMAPPROXY_METRICS_ADDR, if set. Off by default, and needs a runtime.
pub async fn start() -> anyhow::Result<()> {
    if let Ok(addr) = std::env::var("RMAPPROXY_METRICS_ADDR") {
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!("metrics on http://{addr}/metrics");
        tokio::spawn(axum::serve(listener, router()).into_future());
    }
    Ok(())
}
