//! Distributed seeding. The admin (`serve --admin`) splits a GUI seed job into chunks — blocks
//! of up to 16×16 metatiles of one task level that touch the task's coverage — and
//! `rmapproxy seed-worker` pods lease them over HTTP (`/_admin/work/*`), render them with the
//! seeder's own per-metatile code and report back. A lease that is not renewed (worker gone)
//! puts its chunk back in the queue; done and failed chunks are kept as index ranges in the job
//! record, so a job survives admin restarts and failed chunks can be retried.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value as Json, json};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::seed::{self, Mode, SeedTask};
use crate::tile::TileManager;

/// Chunk side in metatiles (≤ 256 metatiles ≈ 4096 tiles per chunk).
const BLOCK: i64 = 16;
/// A lease not renewed for this long is given to another worker.
pub const LEASE: Duration = Duration::from_secs(120);
const HEARTBEAT: Duration = Duration::from_secs(15);
const MAX_CHUNKS: usize = 5_000_000;

#[derive(Clone)]
pub struct Chunk {
    pub task: u32,
    pub z: u32,
    pub mx: (i64, i64),
    pub my: (i64, i64),
    /// Metatiles in the block (bbox-based; the coverage test happens on the worker).
    pub est: u32,
}

#[derive(Clone)]
enum St {
    Pending,
    Leased { worker: String, until: Instant, done: u64, tiles: u64 },
    Done,
    Failed,
}

pub struct Queue {
    /// (name, cache, grid) per task index, as `seed::load_tasks_from` returned them.
    pub tasks: Vec<(String, String, String)>,
    chunks: Vec<Chunk>,
    state: Vec<St>,
    cursor: usize,
    requeue: VecDeque<usize>,
    leased: HashSet<usize>,
    pub done: usize,
    pub failed: usize,
    pub est_total: u64,
    pub est_done: u64,
    /// Estimated metatiles (done, total) per task index.
    pub per_task: Vec<(u64, u64)>,
    pub tiles: u64,
    pub errors: u64,
}

impl Queue {
    pub fn build(tasks: &[SeedTask]) -> Result<Queue> {
        let mut chunks = vec![];
        let mut per_task = vec![(0, 0); tasks.len()];
        for (ti, t) in tasks.iter().enumerate() {
            for &z in &t.levels {
                let Some((mx0, mx1, my0, my1)) = seed::level_meta_range(t, z)? else { continue };
                let ms = t.cg.meta.meta_size(z);
                let mut by = my0;
                while by <= my1 {
                    let ey = (by + BLOCK - 1).min(my1);
                    let mut bx = mx0;
                    while bx <= mx1 {
                        let ex = (bx + BLOCK - 1).min(mx1);
                        // Skip blocks whose metatiles cannot touch the coverage (sea, Sweden).
                        let keep = match &t.coverage {
                            Some(c) => {
                                let a = t.cg.meta.meta_tile((bx * ms.0, by * ms.1, z)).bbox;
                                let b = t.cg.meta.meta_tile((ex * ms.0, ey * ms.1, z)).bbox;
                                let bbox = [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])];
                                c.intersects(bbox, &t.cg.meta.grid.srs)?
                            }
                            None => true,
                        };
                        if keep {
                            let est = ((ex - bx + 1) * (ey - by + 1)) as u32;
                            per_task[ti].1 += est as u64;
                            chunks.push(Chunk { task: ti as u32, z: z as u32, mx: (bx, ex), my: (by, ey), est });
                            if chunks.len() > MAX_CHUNKS {
                                bail!("over {MAX_CHUNKS} chunks: velg færre nivåer eller et mindre område");
                            }
                        }
                        bx = ex + 1;
                    }
                    by = ey + 1;
                }
            }
        }
        let est_total = chunks.iter().map(|c| c.est as u64).sum();
        Ok(Queue {
            tasks: tasks.iter().map(|t| (t.name.clone(), t.cg.cache.clone(), t.cg.grid.clone())).collect(),
            state: vec![St::Pending; chunks.len()],
            chunks,
            cursor: 0,
            requeue: VecDeque::new(),
            leased: HashSet::new(),
            done: 0,
            failed: 0,
            est_total,
            est_done: 0,
            per_task,
            tiles: 0,
            errors: 0,
        })
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn leased_count(&self) -> usize {
        self.leased.len()
    }

    /// Restore done/failed chunks from a job record (`[[from, to], …]` inclusive ranges).
    pub fn restore(&mut self, done: &Json, failed: &Json) {
        for (v, st) in [(done, St::Done), (failed, St::Failed)] {
            for r in v.as_array().into_iter().flatten() {
                let (Some(a), Some(b)) = (r[0].as_u64(), r[1].as_u64()) else { continue };
                for i in a as usize..=(b as usize).min(self.chunks.len().saturating_sub(1)) {
                    self.finish(i, st.clone());
                }
            }
        }
    }

    pub fn ranges(&self, failed: bool) -> Json {
        let mut out = vec![];
        let mut start: Option<usize> = None;
        for (i, s) in self.state.iter().enumerate() {
            let hit = if failed { matches!(s, St::Failed) } else { matches!(s, St::Done) };
            match (hit, start) {
                (true, None) => start = Some(i),
                (false, Some(a)) => {
                    out.push(json!([a, i - 1]));
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(a) = start {
            out.push(json!([a, self.state.len() - 1]));
        }
        Json::Array(out)
    }

    fn finish(&mut self, i: usize, st: St) {
        if matches!(self.state[i], St::Done | St::Failed) {
            return;
        }
        self.leased.remove(&i);
        let c = &self.chunks[i];
        match st {
            St::Done => {
                self.done += 1;
                self.est_done += c.est as u64;
                self.per_task[c.task as usize].0 += c.est as u64;
            }
            St::Failed => self.failed += 1,
            _ => {}
        }
        self.state[i] = st;
    }

    /// Expired leases back to the queue.
    pub fn expire(&mut self) -> Vec<(usize, String)> {
        let now = Instant::now();
        let expired: Vec<(usize, String)> = self
            .leased
            .iter()
            .filter_map(|&i| match &self.state[i] {
                St::Leased { worker, until, .. } if *until < now => Some((i, worker.clone())),
                _ => None,
            })
            .collect();
        for (i, _) in &expired {
            self.release(*i);
        }
        expired
    }

    pub fn release(&mut self, i: usize) {
        if matches!(self.state[i], St::Leased { .. }) {
            self.leased.remove(&i);
            self.state[i] = St::Pending;
            self.requeue.push_back(i);
        }
    }

    /// Every leased chunk back to pending (job stopped; workers see the lost lease).
    pub fn release_all(&mut self) {
        for i in self.leased.clone() {
            self.release(i);
        }
    }

    pub fn retry_failed(&mut self) {
        for i in 0..self.state.len() {
            if matches!(self.state[i], St::Failed) {
                self.failed -= 1;
                self.state[i] = St::Pending;
                self.requeue.push_back(i);
            }
        }
    }

    pub fn claim(&mut self, worker: &str) -> Option<(usize, Chunk)> {
        let i = loop {
            if let Some(i) = self.requeue.pop_front() {
                if matches!(self.state[i], St::Pending) {
                    break i;
                }
                continue;
            }
            while self.cursor < self.state.len() && !matches!(self.state[self.cursor], St::Pending) {
                self.cursor += 1;
            }
            if self.cursor >= self.state.len() {
                return None;
            }
            self.cursor += 1;
            break self.cursor - 1;
        };
        self.state[i] = St::Leased { worker: worker.into(), until: Instant::now() + LEASE, done: 0, tiles: 0 };
        self.leased.insert(i);
        Some((i, self.chunks[i].clone()))
    }

    /// Renew `worker`'s lease on chunk `i`; false when it lost it (job stopped, expired).
    pub fn heartbeat(&mut self, i: usize, worker: &str, done: u64, tiles: u64) -> bool {
        match self.state.get_mut(i) {
            Some(St::Leased { worker: w, until, done: d, tiles: t }) if w == worker => {
                *until = Instant::now() + LEASE;
                *d = done;
                *t = tiles;
                true
            }
            // The admin restarted (or the lease just expired) while the worker kept rendering.
            Some(St::Pending) => {
                self.state[i] = St::Leased { worker: worker.into(), until: Instant::now() + LEASE, done, tiles };
                self.leased.insert(i);
                true
            }
            _ => false,
        }
    }

    /// A worker finished chunk `i` (also accepted after a lost lease: the tiles are written).
    pub fn complete(&mut self, i: usize, tiles: u64, errors: u64) -> bool {
        if i >= self.state.len() || matches!(self.state[i], St::Done | St::Failed) {
            return false;
        }
        self.tiles += tiles;
        self.errors += errors;
        self.finish(i, if errors == 0 { St::Done } else { St::Failed });
        true
    }

    pub fn inflight_tiles(&self) -> u64 {
        self.leased.iter().map(|&i| if let St::Leased { tiles, .. } = &self.state[i] { *tiles } else { 0 }).sum()
    }

    pub fn finished(&self) -> bool {
        self.done + self.failed == self.chunks.len()
    }
}

// ── worker ──────────────────────────────────────────────────────────────────

/// `rmapproxy seed-worker`: lease chunks from the admin at `admin` until SIGTERM.
pub async fn run_worker(mgr: Arc<TileManager>, admin: String, name: String, bind: String) -> Result<()> {
    let admin = admin.trim_end_matches('/').to_string();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else { return };
            tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
            info!("worker: stopping after the current chunk's in-flight metatiles");
            stop.store(true, Ordering::SeqCst);
        });
    }
    // Probes only.
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tokio::spawn(async move {
        let app = axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok\n" }));
        let _ = axum::serve(listener, app).await;
    });
    let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let mut task_cache: HashMap<String, Arc<Vec<Arc<SeedTask>>>> = HashMap::new();
    info!("worker {name}: leasing chunks from {admin}");
    while !stop.load(Ordering::SeqCst) {
        let claim = match post_json(&client, &format!("{admin}/_admin/work/claim"), &json!({ "worker": name })).await {
            Ok(r) if r.status() == reqwest::StatusCode::NO_CONTENT => None,
            Ok(r) if r.status().is_success() => r.bytes().await.ok().and_then(|b| serde_json::from_slice::<Json>(&b).ok()),
            Ok(r) => {
                warn!("worker: claim: HTTP {}", r.status());
                None
            }
            Err(e) => {
                warn!("worker: claim: {e}");
                None
            }
        };
        let Some(c) = claim else {
            for _ in 0..10 {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            continue;
        };
        let job = c["job"].as_str().unwrap_or_default().to_string();
        let chunk = c["chunk"].as_u64().unwrap_or(0);
        let tasks = match task_cache.get(&job) {
            Some(t) => t.clone(),
            None => {
                let loaded = (|| -> Result<Vec<Arc<SeedTask>>> {
                    let v: serde_yaml::Value = serde_yaml::from_str(c["conf"].as_str().unwrap_or_default())?;
                    let names: Vec<String> = c["names"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(String::from)).collect();
                    Ok(seed::load_tasks_from(&mgr, &v, Path::new("/"), &names, Mode::Seed)?.into_iter().map(Arc::new).collect())
                })();
                match loaded {
                    Ok(t) => {
                        task_cache.retain(|_, _| false); // one job at a time is the norm
                        let t = Arc::new(t);
                        task_cache.insert(job.clone(), t.clone());
                        t
                    }
                    Err(e) => {
                        warn!("worker: job {job}: {e:#}");
                        report(&client, &admin, "complete", json!({ "worker": name, "job": job, "chunk": chunk, "tiles": 0, "errors": 1, "messages": [format!("{e:#}")] })).await;
                        continue;
                    }
                }
            }
        };
        let r = render_chunk(&mgr, &client, &admin, &name, &c, &tasks, &stop).await;
        match r {
            Ok(Outcome::Done { tiles, errors, metas, messages, secs }) => {
                info!("worker: {job} chunk {chunk}: {metas} metatiles, {tiles} tiles, {errors} errors in {secs:.1} s");
                report(&client, &admin, "complete", json!({ "worker": name, "job": job, "chunk": chunk, "tiles": tiles, "errors": errors, "metatiles": metas, "messages": messages, "seconds": secs })).await;
            }
            Ok(Outcome::Lost) => info!("worker: {job} chunk {chunk}: lease lost (job stopped?), abandoned"),
            Ok(Outcome::Stopping) => {
                report(&client, &admin, "release", json!({ "worker": name, "job": job, "chunk": chunk })).await;
            }
            Err(e) => {
                warn!("worker: {job} chunk {chunk}: {e:#}");
                report(&client, &admin, "complete", json!({ "worker": name, "job": job, "chunk": chunk, "tiles": 0, "errors": 1, "messages": [format!("{e:#}")] })).await;
            }
        }
    }
    info!("worker {name}: stopped");
    Ok(())
}

/// reqwest is built without its `json` feature.
async fn post_json(client: &reqwest::Client, url: &str, body: &Json) -> reqwest::Result<reqwest::Response> {
    client.post(url).header(reqwest::header::CONTENT_TYPE, "application/json").body(body.to_string()).send().await
}

async fn report(client: &reqwest::Client, admin: &str, what: &str, body: Json) {
    for attempt in 0..5 {
        match post_json(client, &format!("{admin}/_admin/work/{what}"), &body).await {
            Ok(r) if r.status().is_success() || r.status().is_client_error() => return,
            Ok(r) => warn!("worker: {what}: HTTP {}", r.status()),
            Err(e) => warn!("worker: {what}: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(2 << attempt)).await;
    }
}

enum Outcome {
    Done { tiles: u64, errors: u64, metas: u64, messages: Vec<String>, secs: f64 },
    Lost,
    Stopping,
}

async fn render_chunk(
    mgr: &Arc<TileManager>,
    client: &reqwest::Client,
    admin: &str,
    name: &str,
    c: &Json,
    tasks: &[Arc<SeedTask>],
    stop: &AtomicBool,
) -> Result<Outcome> {
    let t0 = Instant::now();
    let ti = c["task"].as_u64().ok_or_else(|| anyhow!("claim without task"))? as usize;
    let task = tasks.get(ti).ok_or_else(|| anyhow!("task {ti} not in job"))?.clone();
    if Some(task.name.as_str()) != c["name"].as_str() || Some(task.cg.cache.as_str()) != c["cache"].as_str() {
        bail!("task {ti} is {}/{}, the admin expected {}/{}", task.name, task.cg.cache, c["name"], c["cache"]);
    }
    let z = c["z"].as_u64().context("z")? as usize;
    let r = |k: &str, i: usize| c[k][i].as_i64().ok_or_else(|| anyhow!("{k}"));
    let (mx0, mx1, my0, my1) = (r("mx", 0)?, r("mx", 1)?, r("my", 0)?, r("my", 1)?);
    let ms = task.cg.meta.meta_size(z);
    let mut metas = vec![];
    for my in my0..=my1 {
        for mx in mx0..=mx1 {
            let main = (mx * ms.0, my * ms.1, z);
            if seed::meta_in_coverage(&task, main)? {
                metas.push(main);
            }
        }
    }
    let conc = c["concurrency"].as_u64().unwrap_or(4).clamp(1, 64) as usize;
    let (done, tiles) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let lost = Arc::new(AtomicBool::new(false));
    // Heartbeats renew the lease and carry progress; a refused one means the job was stopped.
    let hb = {
        let (client, url) = (client.clone(), format!("{admin}/_admin/work/heartbeat"));
        let body = json!({ "worker": name, "job": c["job"], "chunk": c["chunk"], "total": metas.len() });
        let (done, tiles, lost) = (done.clone(), tiles.clone(), lost.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT).await;
                let mut b = body.clone();
                b["done"] = done.load(Ordering::Relaxed).into();
                b["tiles"] = tiles.load(Ordering::Relaxed).into();
                if let Ok(r) = post_json(&client, &url, &b).await {
                    if let Some(v) = r.bytes().await.ok().and_then(|b| serde_json::from_slice::<Json>(&b).ok()) {
                        if v["keep"] == false {
                            lost.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
        })
    };
    let sem = Arc::new(Semaphore::new(conc));
    let mut set: JoinSet<Result<usize>> = JoinSet::new();
    let (mut errors, mut messages) = (0u64, vec![]);
    let mut on_result = |r: Result<Result<usize>, tokio::task::JoinError>| match r {
        Ok(Ok(n)) => {
            tiles.fetch_add(n as u64, Ordering::Relaxed);
            done.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Err(e)) => {
            errors += 1;
            done.fetch_add(1, Ordering::Relaxed);
            if messages.len() < 5 {
                messages.push(format!("{e:#}"));
            }
        }
        Err(e) => {
            errors += 1;
            if messages.len() < 5 {
                messages.push(e.to_string());
            }
        }
    };
    for main in metas.iter().copied() {
        if stop.load(Ordering::SeqCst) || lost.load(Ordering::SeqCst) {
            break;
        }
        let permit = sem.clone().acquire_owned().await?;
        let (m, t) = (mgr.clone(), task.clone());
        set.spawn(async move {
            let _p = permit;
            seed::process_meta(&m, &t, main, true).await.with_context(|| format!("metatile {main:?}"))
        });
        while let Some(r) = set.try_join_next() {
            on_result(r);
        }
    }
    while let Some(r) = set.join_next().await {
        on_result(r);
    }
    hb.abort();
    if lost.load(Ordering::SeqCst) {
        return Ok(Outcome::Lost);
    }
    if done.load(Ordering::Relaxed) < metas.len() as u64 && stop.load(Ordering::SeqCst) {
        return Ok(Outcome::Stopping);
    }
    Ok(Outcome::Done { tiles: tiles.load(Ordering::Relaxed), errors, metas: metas.len() as u64, messages, secs: t0.elapsed().as_secs_f64() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(n: usize) -> Queue {
        let chunks: Vec<Chunk> = (0..n).map(|i| Chunk { task: 0, z: 1, mx: (i as i64, i as i64), my: (0, 0), est: 2 }).collect();
        Queue {
            tasks: vec![("t".into(), "c".into(), "g".into())],
            state: vec![St::Pending; n],
            est_total: 2 * n as u64,
            per_task: vec![(0, 2 * n as u64)],
            chunks,
            cursor: 0,
            requeue: VecDeque::new(),
            leased: HashSet::new(),
            done: 0,
            failed: 0,
            est_done: 0,
            tiles: 0,
            errors: 0,
        }
    }

    #[test]
    fn claim_release_complete_restore() {
        let mut q = queue(5);
        let (a, _) = q.claim("w1").unwrap();
        let (b, _) = q.claim("w2").unwrap();
        assert_eq!((a, b, q.leased_count()), (0, 1, 2));
        q.release(a); // SIGTERM: back first
        assert_eq!(q.claim("w2").unwrap().0, 0);
        assert!(q.heartbeat(1, "w2", 3, 40));
        assert!(!q.heartbeat(1, "w1", 3, 40)); // not w1's lease
        assert!(q.complete(0, 10, 0) && q.complete(1, 5, 2) && !q.complete(1, 5, 0));
        assert_eq!((q.done, q.failed, q.tiles, q.errors, q.est_done), (1, 1, 15, 2, 2));
        assert!(q.heartbeat(3, "w3", 0, 0)); // pending chunk adopted (admin restarted)
        assert_eq!(q.claim("w1").unwrap().0, 2);
        assert_eq!(q.claim("w1").unwrap().0, 4);
        assert!(q.claim("w1").is_none());
        let (done, failed) = (q.ranges(false), q.ranges(true));
        assert_eq!((done.to_string(), failed.to_string()), ("[[0,0]]".into(), "[[1,1]]".into()));
        let mut r = queue(5);
        r.restore(&json!([[0, 0], [3, 4]]), &json!([[1, 1]]));
        assert_eq!((r.done, r.failed, r.claim("w").unwrap().0), (3, 1, 2));
        r.retry_failed();
        assert_eq!((r.failed, r.claim("w").unwrap().0), (0, 1));
        assert!(!r.finished());
        r.complete(1, 0, 0);
        r.complete(2, 0, 0);
        assert!(r.finished());
    }
}
