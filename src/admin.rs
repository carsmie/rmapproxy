//! `serve --admin`: the cache admin API behind tnt-rmapproxy's `/_admin` GUI (replaces
//! tnt-mapproxy's Python admin app). Seeds and cleanups run as `rmapproxy seed` child
//! processes, one `--progress-file` per job; job records live in
//! `<base_dir>/.rmapproxy-admin/`, so a stopped, failed or interrupted (pod restart) job can be
//! continued. Other processes' `<base_dir>/.mapproxy_seed_progress.*` files (SKIPJobs,
//! tnt-mapproxy's admin) are listed read-only. History stays in tnt-mapproxy's
//! `.seed_history.json` format. A seed job can instead be distributed: split into chunks that
//! `rmapproxy seed-worker` pods lease over `/_admin/work/*` (`work.rs`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use parking_lot::Mutex;
use serde_json::{Value as Json, json};
use serde_yaml::{Mapping, Value as Yaml};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{info, warn};

use crate::cache::write_atomic;
use crate::config::load_yaml_with_bases;
use crate::seed::{self, Mode};
use crate::tile::TileManager;
use crate::work::Queue;

const LOG_MAX: usize = 400;
const LOG_LINE_MAX: usize = 2000;
/// Running, but no output for this long (rmapproxy seed prints progress every 30 s).
const STALL_SECS: u64 = 300;
/// Another process's progress file written this recently: that seed is probably running.
const EXTERNAL_FRESH_SECS: u64 = 900;
const HISTORY_MAX: usize = 100;
/// Finished (deleted) jobs kept in memory for the job list.
const DONE_KEEP: usize = 20;
const LEGEND_SCALES: [u32; 8] = [5000, 10000, 25000, 50000, 100000, 250000, 500000, 1000000];
/// refresh "missing": tiles from the last ~27 years count as fresh, so only missing ones render
/// (the cutoff must stay after 1970).
const MISSING_DAYS: u64 = 10000;

pub struct AdminOpts {
    pub mapproxy_conf: PathBuf,
    pub seed_conf: PathBuf,
    pub assets: PathBuf,
}

pub struct Admin {
    mgr: Arc<TileManager>,
    opts: AdminOpts,
    /// Resolved at startup: after a binary update /proc/self/exe names the deleted file.
    exe: PathBuf,
    base: PathBuf,
    dir: PathBuf,
    concurrency: usize,
    max_jobs: usize,
    stats_interval: Duration,
    legend_cache: String,
    legend_dir: PathBuf,
    /// seed.yaml with bases and merge keys applied.
    seed_conf: Yaml,
    /// `/_admin/tasks` body, computed once (seed.yaml is part of the image).
    tasks_json: Json,
    jobs: Mutex<Vec<Job>>,
    history_lock: Mutex<()>,
    stats: Mutex<Stats>,
    stats_kick: tokio::sync::Notify,
    legends_running: AtomicBool,
    legends_last: Mutex<Option<Json>>,
    /// Seed workers by name, from their claims/heartbeats.
    workers: Mutex<HashMap<String, WorkerInfo>>,
}

struct WorkerInfo {
    last_seen: Instant,
    job: Option<String>,
    chunk: Option<usize>,
    chunks_done: u64,
    tiles: u64,
    busy_secs: f64,
}

/// A worker that has not called in for this long is not listed as connected.
const WORKER_SEEN_SECS: u64 = 60;

#[derive(Default)]
struct Stats {
    data: Option<Json>,
    scanning: bool,
}

struct Job {
    id: String,
    mode: Mode,
    /// Normalised request tasks: {name, levels_from, levels_to, bbox?, caches?}.
    tasks: Json,
    names: Vec<String>,
    refresh: String,
    concurrency: usize,
    created_at: SystemTime,
    run_started_at: SystemTime,
    run_started: Instant,
    finished_at: Option<SystemTime>,
    exit_code: Option<i32>,
    /// running | stopping | done | failed | stopped | interrupted
    state: &'static str,
    pid: Option<u32>,
    runs: u32,
    log: VecDeque<String>,
    last_activity: SystemTime,
    /// Tiles of finished tasks (summary lines) + of the current task (progress lines).
    tiles: u64,
    task_tiles: u64,
    errors: u64,
    /// Last progress read from the job's progress sidecar.
    progress: Vec<TaskProgress>,
    /// Distributed job: its chunks (no local process).
    queue: Option<Queue>,
    last_persist: Instant,
}

#[derive(Clone)]
struct TaskProgress {
    name: String,
    cache: String,
    grid: String,
    levels: Vec<u64>,
    done: u64,
    total: u64,
}

impl TaskProgress {
    fn json(&self) -> Json {
        json!({
            "name": self.name, "cache": self.cache, "grid": self.grid,
            "levels": [self.levels.first(), self.levels.last()],
            "done": self.done, "total": self.total,
        })
    }
}

fn mode_str(m: Mode) -> &'static str {
    if m == Mode::Seed { "seed" } else { "cleanup" }
}

fn secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn from_secs(s: f64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs_f64(s.max(0.0))
}

/// RFC 3339 UTC (`2026-10-27T10:15:30Z`), civil-from-days.
pub fn iso(t: SystemTime) -> String {
    let s = secs(t) as i64;
    let (days, rem) = (s.div_euclid(86400), s.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

fn age(t: SystemTime) -> u64 {
    SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0)
}

fn env_num<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// Sidecar of an rmapproxy `--progress-file` F: `F.rmapproxy.json` = {"name|cache|grid|[levels]": [done, total]}.
fn sidecar(progress: &Path) -> PathBuf {
    PathBuf::from(format!("{}.rmapproxy.json", progress.display()))
}

fn read_progress(progress: &Path) -> Option<Vec<TaskProgress>> {
    let text = std::fs::read_to_string(sidecar(progress)).ok()?;
    let Ok(Json::Object(m)) = serde_json::from_str::<Json>(&text) else { return None };
    let mut out = vec![];
    for (id, v) in m {
        let mut parts = id.splitn(4, '|');
        let (Some(name), Some(cache), Some(grid), Some(levels)) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let levels = levels.trim_matches(|c| c == '[' || c == ']').split(',').filter_map(|l| l.trim().parse().ok()).collect();
        out.push(TaskProgress {
            name: name.into(),
            cache: cache.into(),
            grid: grid.into(),
            levels,
            done: v[0].as_u64().unwrap_or(0),
            total: v[1].as_u64().unwrap_or(0),
        });
    }
    Some(out)
}

/// Mean per-task fraction over `names` (tasks without progress yet count as 0).
fn percent(names: &[String], progress: &[TaskProgress]) -> Option<f64> {
    if names.is_empty() || progress.is_empty() {
        return None;
    }
    let sum: f64 = names
        .iter()
        .map(|n| {
            let (d, t) = progress.iter().filter(|p| &p.name == n).fold((0, 0), |(d, t), p| (d + p.done, t + p.total));
            if t == 0 { 0.0 } else { d as f64 / t as f64 }
        })
        .sum();
    Some(100.0 * sum / names.len() as f64)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for c in it.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else if !c.is_control() || c == '\t' {
            out.push(c);
        }
    }
    out
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 80 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.') && !s.starts_with('.')
}

fn resp(status: StatusCode, body: Json) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], body.to_string()).into_response()
}

fn msg(status: StatusCode, text: impl Into<String>) -> Response {
    resp(status, json!({ "message": text.into() }))
}

impl Admin {
    pub fn new(mgr: Arc<TileManager>, opts: AdminOpts) -> Result<Arc<Admin>> {
        let base = mgr.cfg.globals.base_dir.clone();
        let dir = base.join(".rmapproxy-admin");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let seed_conf = load_yaml_with_bases(&opts.seed_conf)?;
        let legend_dir = std::env::var_os("MAPPROXY_LEGEND_DIR").map(PathBuf::from).unwrap_or_else(|| base.join("legend"));
        let tasks_json = task_list(&mgr, &seed_conf, opts.seed_conf.parent().unwrap_or(Path::new(".")));
        let admin = Arc::new(Admin {
            exe: std::env::current_exe().context("current_exe")?,
            concurrency: env_num("SEED_CONCURRENCY", 4usize).clamp(1, 64),
            max_jobs: env_num("RMAPPROXY_ADMIN_MAX_JOBS", 2usize).max(1),
            stats_interval: Duration::from_secs(env_num("RMAPPROXY_ADMIN_STATS_INTERVAL", 900u64).max(60)),
            legend_cache: std::env::var("RMAPPROXY_LEGEND_CACHE").unwrap_or_else(|_| "topo_cache".into()),
            mgr,
            opts,
            base,
            dir,
            legend_dir,
            seed_conf,
            tasks_json,
            jobs: Mutex::new(vec![]),
            history_lock: Mutex::new(()),
            stats: Mutex::new(Stats::default()),
            stats_kick: tokio::sync::Notify::new(),
            legends_running: AtomicBool::new(false),
            legends_last: Mutex::new(None),
            workers: Mutex::new(HashMap::new()),
        });
        admin.load_records();
        Ok(admin)
    }

    /// Unfinished jobs from earlier runs of this admin; one still "running" lost its
    /// process with the old pod: interrupted (continuable).
    fn load_records(&self) {
        let mut jobs = vec![];
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "json") || p.to_string_lossy().ends_with(".rmapproxy.json") {
                continue;
            }
            let Some(r) = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<Json>(&t).ok()) else { continue };
            let Some(mut job) = Job::from_record(&r) else { continue };
            if r["distributed"] == true {
                // Chunks are rebuilt from the job configuration; running jobs keep running (the
                // workers carry on, their leases are re-adopted on the next heartbeat).
                match self.make_queue(&job.id, &job.names) {
                    Ok(mut q) if q.len() as u64 == r["chunks"].as_u64().unwrap_or(0) => {
                        q.restore(&r["chunks_done"], &r["chunks_failed"]);
                        q.tiles = job.tiles;
                        q.errors = job.errors;
                        job.queue = Some(q);
                        if job.state == "stopping" {
                            job.state = "stopped";
                        }
                    }
                    Ok(_) => {
                        warn!("admin: job {}: chunk layout changed, not loaded", job.id);
                        continue;
                    }
                    Err(e) => {
                        warn!("admin: job {}: {e:#}", job.id);
                        continue;
                    }
                }
                jobs.push(job);
                continue;
            }
            if matches!(job.state, "running" | "stopping") {
                // Died with the old pod: about when it last wrote progress.
                let pp = self.progress_path(&job.id);
                let mtime = |p: &Path| p.metadata().and_then(|m| m.modified()).ok();
                job.state = "interrupted";
                job.finished_at = [mtime(&p), mtime(&sidecar(&pp))].into_iter().flatten().max().or(Some(SystemTime::now()));
                self.append_history(&job);
                self.persist(&job);
            }
            job.progress = read_progress(&self.progress_path(&job.id)).unwrap_or_default();
            jobs.push(job);
        }
        jobs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        info!("admin: {} unfinished job(s) in {}", jobs.len(), self.dir.display());
        *self.jobs.lock() = jobs;
    }

    fn make_queue(&self, id: &str, names: &[String]) -> Result<Queue> {
        let conf = load_yaml_with_bases(&self.job_conf_path(id))?;
        Queue::build(&seed::load_tasks_from(&self.mgr, &conf, &self.dir, names, Mode::Seed)?)
    }

    fn job_conf_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.yaml"))
    }

    fn progress_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.progress"))
    }

    fn persist(&self, job: &Job) {
        if let Err(e) = write_atomic(&self.dir.join(format!("{}.json", job.id)), job.record().to_string().as_bytes()) {
            warn!("admin: {e:#}");
        }
    }

    fn remove_files(&self, id: &str) {
        let p = self.progress_path(id);
        for f in [self.dir.join(format!("{id}.json")), self.job_conf_path(id), sidecar(&p), p] {
            let _ = std::fs::remove_file(f);
        }
    }

    fn history_path(&self) -> PathBuf {
        self.base.join(".seed_history.json")
    }

    fn read_history(&self) -> Vec<Json> {
        std::fs::read_to_string(self.history_path())
            .ok()
            .and_then(|t| serde_json::from_str::<Json>(&t).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    }

    fn append_history(&self, job: &Job) {
        let _g = self.history_lock.lock();
        let mut h = self.read_history();
        h.push(json!({
            "key": job.id,
            "started_at": iso(job.run_started_at),
            "finished_at": job.finished_at.map(iso),
            "exit_code": job.exit_code,
            "state": job.state,
            "tasks": job.tasks,
            "mode": mode_str(job.mode),
            "source": "manual",
            "refresh": job.refresh,
            "tiles": job.tiles,
            "errors": job.errors,
        }));
        let skip = h.len().saturating_sub(HISTORY_MAX);
        if let Err(e) = write_atomic(&self.history_path(), Json::Array(h.split_off(skip)).to_string().as_bytes()) {
            warn!("admin: seed history: {e:#}");
        }
    }

    /// Tiles/s over recent successful seed runs (for time estimates).
    fn recent_rate(&self) -> Option<f64> {
        let (mut tiles, mut secs) = (0.0, 0.0);
        for e in self.read_history().iter().rev().filter(|e| e["mode"] == "seed" && e["exit_code"] == 0).take(20) {
            let t = e["tiles"].as_f64().unwrap_or(0.0);
            let d = match (e["started_at"].as_str(), e["finished_at"].as_str()) {
                (Some(a), Some(b)) => parse_iso(b).zip(parse_iso(a)).map(|(b, a)| b - a).unwrap_or(0.0),
                _ => 0.0,
            };
            if t > 0.0 && d > 0.0 {
                tiles += t;
                secs += d;
            }
        }
        (secs > 0.0).then(|| tiles / secs)
    }

    // ── jobs ────────────────────────────────────────────────────────────────

    /// The job's seed configuration (`seeds:` or `cleanups:` + the coverages it uses, datasources
    /// absolute), task names and normalised request tasks.
    fn build_conf(&self, req: &Json, mode: Mode) -> Result<(Yaml, Vec<String>, Json, String)> {
        let empty = Mapping::new();
        let seeds = self.seed_conf.get("seeds").and_then(|s| s.as_mapping()).unwrap_or(&empty);
        let coverages = self.seed_conf.get("coverages").and_then(|s| s.as_mapping()).unwrap_or(&empty);
        let seed_dir = self.opts.seed_conf.parent().unwrap_or(Path::new("."));
        let refresh = match mode {
            Mode::Seed => match req["refresh"].as_str().unwrap_or("all") {
                r @ ("all" | "missing" | "default") => r.to_string(),
                "older" => {
                    let h = req["older_than_hours"].as_f64().filter(|h| *h > 0.0).ok_or_else(|| anyhow!("older_than_hours må være > 0"))?;
                    format!("older:{h}")
                }
                r => bail!("ukjent refresh {r:?}"),
            },
            Mode::Cleanup => String::new(),
        };
        let refresh_value = |h: f64| -> Yaml {
            let mut m = Mapping::new();
            m.insert("hours".into(), h.into());
            Yaml::Mapping(m)
        };
        let cache_names: HashSet<&str> =
            seeds.values().flat_map(|s| s.get("caches").and_then(|c| c.as_sequence()).into_iter().flatten()).filter_map(|c| c.as_str()).collect();
        let first = seeds.values().next().cloned().unwrap_or(Yaml::Null);

        let mut entries = Mapping::new();
        let mut covs = Mapping::new();
        let mut names = vec![];
        let mut norm = vec![];
        for t in req["tasks"].as_array().map(Vec::as_slice).unwrap_or_default() {
            let raw = t["name"].as_str().unwrap_or_default();
            let name: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).take(64).collect();
            if name.is_empty() || names.contains(&name) {
                continue;
            }
            let lf = t["levels_from"].as_u64();
            let lt = t["levels_to"].as_u64();
            let mut nt = json!({ "name": name, "levels_from": lf, "levels_to": lt });
            let mut entry = if let Some(e) = seeds.get(name.as_str()) {
                e.as_mapping().cloned().ok_or_else(|| anyhow!("{name}: ugyldig oppgave i seed.yaml"))?
            } else if let Some(b) = t["bbox"].as_array() {
                let b: Vec<f64> = b.iter().filter_map(|v| v.as_f64()).collect();
                if b.len() != 4 || !(b[0] < b[2] && b[1] < b[3] && b[0] >= -180.0 && b[2] <= 180.0 && b[1] >= -90.0 && b[3] <= 90.0) {
                    bail!("{name}: ugyldig bbox (V < Ø, S < N, WGS84)");
                }
                let caches: Vec<String> = t["caches"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.as_str())
                    .filter(|c| cache_names.contains(c))
                    .map(String::from)
                    .collect();
                let caches_y: Yaml = if caches.is_empty() {
                    first.get("caches").cloned().unwrap_or_else(|| vec!["topo_cache"].into())
                } else {
                    caches.iter().map(String::as_str).collect::<Vec<_>>().into()
                };
                // The grid must fit the cache (topo_cache_2x has only norway_25833_2x): from a
                // seed.yaml task with the same caches.
                let grids = seeds.values().find(|s| s.get("caches") == Some(&caches_y)).unwrap_or(&first).get("grids").cloned();
                let cov = format!("{name}_cov");
                let mut c = Mapping::new();
                c.insert("bbox".into(), b.clone().into());
                c.insert("srs".into(), "EPSG:4326".into());
                covs.insert(cov.clone().into(), Yaml::Mapping(c));
                let mut e = Mapping::new();
                e.insert("caches".into(), caches_y.clone());
                if let Some(g) = grids {
                    e.insert("grids".into(), g);
                }
                e.insert("coverages".into(), vec![cov].into());
                nt["bbox"] = json!(b);
                nt["caches"] = serde_json::to_value(caches_y.as_sequence().map(|s| s.iter().filter_map(|c| c.as_str()).collect::<Vec<_>>()))?;
                e
            } else {
                bail!("ukjent oppgave {name:?} uten bbox");
            };
            if lf.is_some() || lt.is_some() {
                let (mut a, mut b) = (lf.unwrap_or(0).min(30), lt.unwrap_or(30).min(30));
                if a > b {
                    std::mem::swap(&mut a, &mut b);
                }
                let mut l = Mapping::new();
                l.insert("from".into(), a.into());
                l.insert("to".into(), b.into());
                entry.insert("levels".into(), Yaml::Mapping(l));
            }
            match refresh.as_str() {
                "default" => {}
                "missing" => {
                    entry.insert("refresh_before".into(), refresh_value(MISSING_DAYS as f64 * 24.0));
                }
                r if r.starts_with("older:") => {
                    entry.insert("refresh_before".into(), refresh_value(r[6..].parse().unwrap_or(24.0)));
                }
                _ => {
                    // "all", and cleanups (remove everything in the task's area and levels).
                    entry.remove("refresh_before");
                }
            }
            for c in entry.get("coverages").and_then(|c| c.as_sequence()).into_iter().flatten().filter_map(|c| c.as_str()) {
                if covs.contains_key(c) {
                    continue;
                }
                let mut conf = coverages.get(c).cloned().ok_or_else(|| anyhow!("{name}: ukjent dekning {c}"))?;
                // Datasource files resolve against seed.yaml's directory, not the job's.
                if let Some(Yaml::String(ds)) = conf.get("datasource") {
                    let p = Path::new(ds);
                    if ds.ends_with(".json") && !p.is_absolute() {
                        let abs = seed_dir.join(p).to_string_lossy().into_owned();
                        conf.as_mapping_mut().map(|m| m.insert("datasource".into(), abs.into()));
                    }
                }
                covs.insert(c.into(), conf);
            }
            entries.insert(name.clone().into(), Yaml::Mapping(entry));
            names.push(name);
            norm.push(nt);
        }
        if names.is_empty() {
            bail!("ingen gyldige oppgaver valgt");
        }
        let mut root = Mapping::new();
        root.insert(if mode == Mode::Seed { "seeds" } else { "cleanups" }.into(), Yaml::Mapping(entries));
        root.insert("coverages".into(), Yaml::Mapping(covs));
        Ok((Yaml::Mapping(root), names, Json::Array(norm), refresh))
    }

    /// Local (in-pod) jobs running; distributed jobs run on the workers and don't count.
    fn running_count(&self) -> usize {
        self.jobs.lock().iter().filter(|j| j.queue.is_none() && matches!(j.state, "running" | "stopping")).count()
    }

    /// Tasks of the new job that another (own or external, fresh) seed works on right now.
    fn overlap_warning(&self, names: &[String]) -> Option<String> {
        let mut busy: Vec<String> = vec![];
        for j in self.jobs.lock().iter().filter(|j| matches!(j.state, "running" | "stopping")) {
            busy.extend(j.names.iter().filter(|n| names.contains(n)).map(|n| format!("{n} (jobb {})", j.id)));
        }
        for x in self.externals() {
            if x["running"] == true {
                let key = x["key"].as_str().unwrap_or_default();
                for p in x["progress"].as_array().into_iter().flatten() {
                    if let Some(n) = p["name"].as_str().filter(|n| names.iter().any(|m| m == n)) {
                        busy.push(format!("{n} ({key})"));
                    }
                }
            }
        }
        busy.dedup();
        (!busy.is_empty()).then(|| format!("Seedes allerede av en annen prosess: {}. Flislåsene hindrer dobbel rendering, men jobbene konkurrerer om backend.", busy.join(", ")))
    }

    fn new_id(&self, mode: Mode) -> String {
        let stamp: String = iso(SystemTime::now()).chars().filter(|c| c.is_ascii_digit()).collect();
        let base = format!("{}-{}-{}", &stamp[..8], &stamp[8..14], if mode == Mode::Seed { "seed" } else { "slett" });
        let taken = |id: &str| self.jobs.lock().iter().any(|j| j.id == id) || self.dir.join(format!("{id}.json")).exists();
        let mut id = base.clone();
        let mut n = 2;
        while taken(&id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }

    fn start(self: &Arc<Self>, req: &Json) -> Result<Json> {
        let mode = match req["mode"].as_str().unwrap_or("seed") {
            "seed" => Mode::Seed,
            "cleanup" => Mode::Cleanup,
            m => bail!("ukjent modus {m:?}"),
        };
        let (conf, names, tasks, refresh) = self.build_conf(req, mode)?;
        // Validate like the child will (unknown caches/grids/coverages fail here, not in the job).
        let seed_tasks = seed::load_tasks_from(&self.mgr, &conf, &self.dir, &names, mode)?;
        let distributed = req["distributed"] == true;
        if distributed && mode != Mode::Seed {
            bail!("sletting kjøres i admin-poden, ikke på arbeiderne");
        }
        if !distributed && self.running_count() >= self.max_jobs {
            bail!("maks {} samtidige jobber i admin-poden kjører allerede", self.max_jobs);
        }
        let queue = if distributed { Some(Queue::build(&seed_tasks)?) } else { None };
        let concurrency = req["concurrency"].as_u64().map_or(self.concurrency, |c| (c as usize).clamp(1, 64));
        let warning = self.overlap_warning(&names);
        let id = self.new_id(mode);
        std::fs::write(self.job_conf_path(&id), serde_yaml::to_string(&conf)?).context("writing job configuration")?;
        let now = SystemTime::now();
        let job = Job {
            id: id.clone(),
            mode,
            tasks,
            names,
            refresh,
            concurrency,
            created_at: now,
            run_started_at: now,
            run_started: Instant::now(),
            finished_at: None,
            exit_code: None,
            state: "running",
            pid: None,
            runs: 0,
            log: VecDeque::new(),
            last_activity: now,
            tiles: 0,
            task_tiles: 0,
            errors: 0,
            progress: vec![],
            queue,
            last_persist: Instant::now(),
        };
        if let Some((n, est)) = job.queue.as_ref().map(|q| (q.len(), q.est_total)) {
            let mut job = job;
            job.runs = 1;
            job.log.push_back(format!("Fordelt: {n} chunks (~{est} metafliser) til seed-arbeiderne."));
            self.persist(&job);
            self.jobs.lock().push(job);
            let mut out = json!({ "id": id, "message": format!("Jobb {id} startet, fordelt på {n} chunks.") });
            if self.workers_connected() == 0 {
                out["warning"] = "Ingen seed-arbeidere er tilkoblet; jobben venter til en kobler seg på.".into();
            } else if let Some(w) = warning {
                out["warning"] = w.into();
            }
            return Ok(out);
        }
        self.jobs.lock().push(job);
        if let Err(e) = self.launch(&id) {
            self.jobs.lock().retain(|j| j.id != id);
            self.remove_files(&id);
            return Err(e);
        }
        let mut out = json!({ "id": id, "message": format!("Jobb {id} startet.") });
        if let Some(w) = warning {
            out["warning"] = w.into();
        }
        Ok(out)
    }

    /// Spawn `rmapproxy seed` for job `id` (state already set to running).
    fn launch(self: &Arc<Self>, id: &str) -> Result<()> {
        let (mode, names, conc) = {
            let jobs = self.jobs.lock();
            let j = jobs.iter().find(|j| j.id == id).ok_or_else(|| anyhow!("ukjent jobb"))?;
            (j.mode, j.names.join(","), j.concurrency)
        };
        let mut cmd = tokio::process::Command::new(&self.exe);
        cmd.arg("seed")
            .arg("-f")
            .arg(&self.opts.mapproxy_conf)
            .arg("-s")
            .arg(self.job_conf_path(id))
            .arg(if mode == Mode::Seed { "--seed" } else { "--cleanup" })
            .arg(&names)
            .arg("-c")
            .arg(conc.to_string())
            .arg("--progress-file")
            .arg(self.progress_path(id))
            .arg("--continue")
            .current_dir(&self.dir)
            .env("NO_COLOR", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().context("starting rmapproxy seed")?;
        let pid = child.id();
        {
            let mut jobs = self.jobs.lock();
            if let Some(j) = jobs.iter_mut().find(|j| j.id == id) {
                let now = SystemTime::now();
                j.pid = pid;
                j.runs += 1;
                j.run_started_at = now;
                j.run_started = Instant::now();
                j.last_activity = now;
                j.finished_at = None;
                j.exit_code = None;
                j.tiles = 0;
                j.task_tiles = 0;
                j.errors = 0;
                j.log.clear();
                self.persist(j);
            }
        }
        info!("admin: job {id} started (pid {pid:?}): {} {names}", mode_str(mode));
        let readers: Vec<_> = [child.stdout.take().map(|o| Box::new(o) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            child.stderr.take().map(|e| Box::new(e) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)]
        .into_iter()
        .flatten()
        .map(|r| {
            let (me, id) = (self.clone(), id.to_string());
            tokio::spawn(async move {
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    me.on_line(&id, &l);
                }
            })
        })
        .collect();
        let (me, id) = (self.clone(), id.to_string());
        tokio::spawn(async move {
            let status = child.wait().await;
            for r in readers {
                let _ = r.await;
            }
            me.on_exit(&id, status.ok());
        });
        Ok(())
    }

    fn on_line(&self, id: &str, line: &str) {
        let mut line = strip_ansi(line);
        if line.len() > LOG_LINE_MAX {
            let cut = (0..=LOG_LINE_MAX).rev().find(|&i| line.is_char_boundary(i)).unwrap_or(0);
            line.truncate(cut);
            line.push_str(" …");
        }
        info!(target: "rmapproxy::admin", "{id}: {line}");
        let mut jobs = self.jobs.lock();
        let Some(j) = jobs.iter_mut().find(|j| j.id == id) else { return };
        // Per-task summary "    N metatiles, T tiles rendered, E errors in S s"; progress
        // "[hh:mm:ss] z pct% bbox (T tiles)" carries the current task's running count.
        let w: Vec<&str> = line.split_whitespace().collect();
        if w.len() >= 7 && w[1] == "metatiles," && w[3] == "tiles" && w[6].starts_with("errors") {
            j.tiles += w[2].parse::<u64>().unwrap_or(0);
            j.errors += w[5].parse::<u64>().unwrap_or(0);
            j.task_tiles = 0;
        } else if line.starts_with('[') && line.ends_with(" tiles)") {
            if let Some(n) = line.rsplit_once('(').and_then(|(_, r)| r.split_whitespace().next()?.parse().ok()) {
                j.task_tiles = n;
            }
        }
        j.last_activity = SystemTime::now();
        if j.log.len() >= LOG_MAX {
            j.log.pop_front();
        }
        j.log.push_back(line);
    }

    fn on_exit(&self, id: &str, status: Option<std::process::ExitStatus>) {
        let code = status.map(|s| s.code().unwrap_or_else(|| -s.signal().unwrap_or(0)));
        let mut jobs = self.jobs.lock();
        let Some(j) = jobs.iter_mut().find(|j| j.id == id) else { return };
        j.tiles += std::mem::take(&mut j.task_tiles);
        j.exit_code = code;
        j.finished_at = Some(SystemTime::now());
        j.pid = None;
        j.state = match (j.state, code) {
            ("stopping", _) => "stopped",
            (_, Some(0)) => "done",
            _ => "failed",
        };
        if j.state == "done" {
            // The seed removed its progress files; everything it planned is finished.
            for p in &mut j.progress {
                p.done = p.total;
            }
            self.remove_files(id);
        } else {
            if let Some(p) = read_progress(&self.progress_path(id)) {
                j.progress = p;
            }
            self.persist(j);
        }
        info!("admin: job {id} {} (exit {code:?}, {} tiles, {} errors)", j.state, j.tiles, j.errors);
        self.append_history(j);
        // Keep only the newest DONE_KEEP finished-and-deleted jobs.
        let done: Vec<String> = jobs.iter().filter(|j| j.state == "done").map(|j| j.id.clone()).collect();
        if done.len() > DONE_KEEP {
            let drop: HashSet<&String> = done[..done.len() - DONE_KEEP].iter().collect();
            jobs.retain(|j| !drop.contains(&j.id));
        }
        drop(jobs);
        self.stats_kick.notify_one();
    }

    fn stop(self: &Arc<Self>, id: &str) -> Response {
        let pid = {
            let mut jobs = self.jobs.lock();
            let Some(j) = jobs.iter_mut().find(|j| j.id == id) else { return msg(StatusCode::NOT_FOUND, "Ukjent jobb.") };
            if let Some(q) = j.queue.as_mut() {
                if j.state != "running" {
                    return msg(StatusCode::CONFLICT, "Jobben kjører ikke.");
                }
                // Workers lose their leases on the next heartbeat and drop their chunks.
                q.release_all();
                j.state = "stopped";
                j.finished_at = Some(SystemTime::now());
                j.tiles = q.tiles;
                j.errors = q.errors;
                j.log.push_back("Stoppet; arbeiderne slipper chunkene sine.".into());
                self.append_history(j);
                self.persist(j);
                return msg(StatusCode::OK, "Jobben er stoppet. Den kan fortsettes senere.");
            }
            match (j.state, j.pid) {
                ("running", Some(pid)) => {
                    j.state = "stopping";
                    pid
                }
                ("stopping", _) => return msg(StatusCode::OK, "Jobben stoppes allerede."),
                _ => return msg(StatusCode::CONFLICT, "Jobben kjører ikke."),
            }
        };
        // The child is ours and not yet reaped (state is "stopping" until wait() returns),
        // so the pid cannot have been reused.
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        let (me, id) = (self.clone(), id.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            if me.jobs.lock().iter().any(|j| j.id == id && j.state == "stopping" && j.pid == Some(pid)) {
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
        });
        msg(StatusCode::OK, "Jobben stoppes. Den kan fortsettes senere.")
    }

    fn resume(self: &Arc<Self>, id: &str) -> Response {
        {
            let mut jobs = self.jobs.lock();
            if let Some(j) = jobs.iter_mut().find(|j| j.id == id && j.queue.is_some()) {
                if !matches!(j.state, "stopped" | "failed" | "interrupted") {
                    return msg(StatusCode::CONFLICT, "Bare stoppede eller feilede jobber kan fortsettes.");
                }
                let q = j.queue.as_mut().expect("distributed");
                let retried = q.failed;
                q.retry_failed();
                j.state = "running";
                j.runs += 1;
                j.run_started_at = SystemTime::now();
                j.run_started = Instant::now();
                j.finished_at = None;
                j.log.push_back(format!("Fortsetter: {} chunks igjen ({retried} feilede prøves på nytt).", q.len() - q.done));
                self.persist(j);
                return msg(StatusCode::OK, format!("Jobb {id} fortsetter på arbeiderne."));
            }
        }
        if self.running_count() >= self.max_jobs {
            return msg(StatusCode::CONFLICT, format!("Maks {} samtidige jobber kjører allerede.", self.max_jobs));
        }
        if !self.job_conf_path(id).exists() {
            return msg(StatusCode::CONFLICT, "Jobbkonfigurasjonen finnes ikke lenger.");
        }
        {
            let mut jobs = self.jobs.lock();
            let Some(j) = jobs.iter_mut().find(|j| j.id == id) else { return msg(StatusCode::NOT_FOUND, "Ukjent jobb.") };
            if !matches!(j.state, "stopped" | "failed" | "interrupted") {
                return msg(StatusCode::CONFLICT, "Bare stoppede, feilede eller avbrutte jobber kan fortsettes.");
            }
            j.state = "running";
        }
        match self.launch(id) {
            Ok(()) => msg(StatusCode::OK, format!("Jobb {id} fortsetter der den slapp.")),
            Err(e) => {
                if let Some(j) = self.jobs.lock().iter_mut().find(|j| j.id == id) {
                    j.state = "failed";
                }
                msg(StatusCode::INTERNAL_SERVER_ERROR, format!("Kunne ikke starte: {e:#}"))
            }
        }
    }

    fn discard(&self, id: &str) -> Response {
        let mut jobs = self.jobs.lock();
        match jobs.iter().find(|j| j.id == id) {
            None => return msg(StatusCode::NOT_FOUND, "Ukjent jobb."),
            Some(j) if matches!(j.state, "running" | "stopping") => return msg(StatusCode::CONFLICT, "Stopp jobben først."),
            _ => {}
        }
        jobs.retain(|j| j.id != id);
        drop(jobs);
        self.remove_files(id);
        msg(StatusCode::OK, format!("Jobb {id} er forkastet (fliser som allerede er seedet blir liggende)."))
    }

    fn job_json(&self, j: &mut Job, with_log: bool) -> Json {
        if j.queue.is_some() {
            return self.distributed_json(j, with_log);
        }
        if matches!(j.state, "running" | "stopping") {
            if let Some(p) = read_progress(&self.progress_path(&j.id)) {
                j.progress = p;
            }
        }
        let running = matches!(j.state, "running" | "stopping");
        let duration = match j.finished_at {
            Some(f) => f.duration_since(j.run_started_at).map(|d| d.as_secs()).unwrap_or(0),
            None => j.run_started.elapsed().as_secs(),
        };
        let current = j.progress.iter().find(|p| p.done < p.total).map(|p| p.name.clone());
        let mut v = json!({
            "id": j.id,
            "mode": mode_str(j.mode),
            "state": j.state,
            "pid": j.pid,
            "created_at": iso(j.created_at),
            "started_at": iso(j.run_started_at),
            "finished_at": j.finished_at.map(iso),
            "duration_seconds": duration,
            "exit_code": j.exit_code,
            "tasks": j.tasks,
            "refresh": j.refresh,
            "concurrency": j.concurrency,
            "runs": j.runs,
            "tiles": j.tiles + j.task_tiles,
            "errors": j.errors,
            "percent": if j.state == "done" { Some(100.0) } else { percent(&j.names, &j.progress) },
            "current_task": if running { current } else { None },
            "progress": j.progress.iter().map(TaskProgress::json).collect::<Vec<_>>(),
            "last_activity_age_seconds": age(j.last_activity),
            "stalled": running && age(j.last_activity) > STALL_SECS,
            "resumable": matches!(j.state, "stopped" | "failed" | "interrupted") && self.job_conf_path(&j.id).exists(),
        });
        if with_log {
            v["log"] = json!(j.log.iter().collect::<Vec<_>>());
        }
        v
    }

    fn distributed_json(&self, j: &mut Job, with_log: bool) -> Json {
        let running = j.state == "running";
        for (i, w) in j.queue.as_mut().map(|q| q.expire()).unwrap_or_default() {
            j.log.push_back(format!("Chunk {i}: {w} svarte ikke; legges tilbake i køen."));
        }
        let q = j.queue.as_ref().expect("distributed");
        let levels = |name: &str| {
            j.tasks.as_array().into_iter().flatten().find(|t| t["name"] == name).map(|t| json!([t["levels_from"], t["levels_to"]])).unwrap_or(json!([null, null]))
        };
        let progress: Vec<Json> = q
            .tasks
            .iter()
            .zip(&q.per_task)
            .map(|((name, cache, grid), (d, t))| json!({ "name": name, "cache": cache, "grid": grid, "levels": levels(name), "done": d, "total": t }))
            .collect();
        let duration = match j.finished_at {
            Some(f) => f.duration_since(j.run_started_at).map(|d| d.as_secs()).unwrap_or(0),
            None => j.run_started.elapsed().as_secs(),
        };
        let leased = q.leased_count();
        let mut v = json!({
            "id": j.id,
            "mode": mode_str(j.mode),
            "state": j.state,
            "distributed": { "chunks": q.len(), "done": q.done, "failed": q.failed, "leased": leased },
            "pid": null,
            "created_at": iso(j.created_at),
            "started_at": iso(j.run_started_at),
            "finished_at": j.finished_at.map(iso),
            "duration_seconds": duration,
            "exit_code": j.exit_code,
            "tasks": j.tasks,
            "refresh": j.refresh,
            "concurrency": j.concurrency,
            "runs": j.runs,
            "tiles": q.tiles + q.inflight_tiles(),
            "errors": q.errors,
            "percent": if q.est_total == 0 { 100.0 } else { 100.0 * q.est_done as f64 / q.est_total as f64 },
            "current_task": null,
            "progress": progress,
            "last_activity_age_seconds": age(j.last_activity),
            "stalled": false,
            "waiting_for_workers": running && leased == 0 && !q.finished(),
            "resumable": matches!(j.state, "stopped" | "failed" | "interrupted"),
        });
        if with_log {
            v["log"] = json!(j.log.iter().collect::<Vec<_>>());
        }
        v
    }

    // ── seed workers (`/_admin/work/*`) ─────────────────────────────────────

    fn workers_connected(&self) -> usize {
        self.workers.lock().values().filter(|w| w.last_seen.elapsed().as_secs() < WORKER_SEEN_SECS).count()
    }

    fn seen(&self, worker: &str, job: Option<&str>, chunk: Option<usize>) {
        let mut ws = self.workers.lock();
        ws.retain(|_, w| w.last_seen.elapsed().as_secs() < 600);
        let w = ws.entry(worker.to_string()).or_insert(WorkerInfo { last_seen: Instant::now(), job: None, chunk: None, chunks_done: 0, tiles: 0, busy_secs: 0.0 });
        w.last_seen = Instant::now();
        w.job = job.map(String::from);
        w.chunk = chunk;
    }

    /// Next chunk of the oldest running distributed job.
    fn claim(&self, worker: &str) -> Option<Json> {
        let mut jobs = self.jobs.lock();
        for j in jobs.iter_mut().filter(|j| j.state == "running") {
            let Some(q) = j.queue.as_mut() else { continue };
            for (i, w) in q.expire() {
                j.log.push_back(format!("Chunk {i}: {w} svarte ikke; legges tilbake i køen."));
            }
            let Some((i, c)) = q.claim(worker) else { continue };
            let (name, cache, grid) = q.tasks[c.task as usize].clone();
            let conf = match std::fs::read_to_string(self.job_conf_path(&j.id)) {
                Ok(t) => t,
                Err(e) => {
                    warn!("admin: job {}: {e}", j.id);
                    q.release(i);
                    continue;
                }
            };
            j.last_activity = SystemTime::now();
            let out = json!({
                "job": j.id, "chunk": i, "task": c.task, "name": name, "cache": cache, "grid": grid,
                "z": c.z, "mx": [c.mx.0, c.mx.1], "my": [c.my.0, c.my.1], "concurrency": j.concurrency,
                "names": j.names, "conf": conf,
            });
            let id = j.id.clone();
            drop(jobs);
            self.seen(worker, Some(&id), Some(i));
            return Some(out);
        }
        drop(jobs);
        self.seen(worker, None, None);
        None
    }

    fn heartbeat(&self, b: &Json) -> Json {
        let (worker, job, chunk) = (b["worker"].as_str().unwrap_or("?"), b["job"].as_str().unwrap_or_default(), b["chunk"].as_u64().unwrap_or(u64::MAX) as usize);
        let keep = {
            let mut jobs = self.jobs.lock();
            match jobs.iter_mut().find(|j| j.id == job && j.state == "running") {
                Some(j) => {
                    j.last_activity = SystemTime::now();
                    j.queue.as_mut().is_some_and(|q| q.heartbeat(chunk, worker, b["done"].as_u64().unwrap_or(0), b["tiles"].as_u64().unwrap_or(0)))
                }
                None => false,
            }
        };
        self.seen(worker, Some(job), Some(chunk));
        json!({ "keep": keep })
    }

    fn complete(&self, b: &Json) {
        let (worker, job, chunk) = (b["worker"].as_str().unwrap_or("?"), b["job"].as_str().unwrap_or_default(), b["chunk"].as_u64().unwrap_or(u64::MAX) as usize);
        let (tiles, errors) = (b["tiles"].as_u64().unwrap_or(0), b["errors"].as_u64().unwrap_or(0));
        let mut finished = false;
        {
            let mut jobs = self.jobs.lock();
            let Some(j) = jobs.iter_mut().find(|j| j.id == job) else { return };
            let Some(q) = j.queue.as_mut() else { return };
            if !q.complete(chunk, tiles, errors) {
                return;
            }
            j.last_activity = SystemTime::now();
            if errors > 0 {
                j.log.push_back(format!("Chunk {chunk} ({worker}): {errors} feil av {} metafliser", b["metatiles"]));
                for m in b["messages"].as_array().into_iter().flatten().filter_map(|m| m.as_str()) {
                    j.log.push_back(format!("  {m}"));
                }
            }
            while j.log.len() > LOG_MAX {
                j.log.pop_front();
            }
            j.tiles = q.tiles;
            j.errors = q.errors;
            if q.finished() && j.state == "running" {
                let failed = q.failed;
                j.state = if failed == 0 { "done" } else { "failed" };
                j.exit_code = Some(if failed == 0 { 0 } else { 1 });
                j.finished_at = Some(SystemTime::now());
                j.log.push_back(format!("Ferdig: {} fliser, {} feilede chunks.", j.tiles, failed));
                info!("admin: distributed job {job} {} ({} tiles, {failed} failed chunks)", j.state, j.tiles);
                self.append_history(j);
                if failed == 0 {
                    self.remove_files(job);
                } else {
                    self.persist(j);
                }
                finished = true;
            } else if j.last_persist.elapsed() > Duration::from_secs(5) {
                j.last_persist = Instant::now();
                self.persist(j);
            }
        }
        {
            let mut ws = self.workers.lock();
            if let Some(w) = ws.get_mut(worker) {
                w.chunks_done += 1;
                w.tiles += tiles;
                w.busy_secs += b["seconds"].as_f64().unwrap_or(0.0);
                w.last_seen = Instant::now();
                w.chunk = None;
            }
        }
        if finished {
            self.stats_kick.notify_one();
        }
    }

    fn release(&self, b: &Json) {
        let (job, chunk) = (b["job"].as_str().unwrap_or_default(), b["chunk"].as_u64().unwrap_or(u64::MAX) as usize);
        if let Some(q) = self.jobs.lock().iter_mut().find(|j| j.id == job).and_then(|j| j.queue.as_mut()) {
            if chunk < q.len() {
                q.release(chunk);
            }
        }
        self.seen(b["worker"].as_str().unwrap_or("?"), None, None);
    }

    fn workers_json(&self) -> Json {
        let mut ws: Vec<Json> = self
            .workers
            .lock()
            .iter()
            .map(|(name, w)| {
                json!({
                    "name": name, "last_seen_seconds": w.last_seen.elapsed().as_secs(),
                    "connected": w.last_seen.elapsed().as_secs() < WORKER_SEEN_SECS,
                    "job": w.job, "chunk": w.chunk, "chunks_done": w.chunks_done, "tiles": w.tiles,
                    "tiles_per_second": (w.busy_secs > 0.0).then(|| w.tiles as f64 / w.busy_secs),
                })
            })
            .collect();
        ws.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Json::Array(ws)
    }

    // ── other processes' progress files ─────────────────────────────────────

    fn externals(&self) -> Vec<Json> {
        let mut out = vec![];
        for e in std::fs::read_dir(&self.base).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(key) = name.strip_prefix(".mapproxy_seed_progress.") else { continue };
            if key.ends_with(".rmapproxy.json") || key.contains(".tmp-") || !valid_id(key) {
                continue;
            }
            let p = e.path();
            let mtime = |p: &Path| p.metadata().and_then(|m| m.modified()).ok();
            let Some(m) = [mtime(&p), mtime(&sidecar(&p))].into_iter().flatten().max() else { continue };
            let progress = read_progress(&p);
            let a = age(m);
            out.push(json!({
                "key": key,
                "format": if progress.is_some() { "rmapproxy" } else { "mapproxy" },
                "last_progress_at": iso(m),
                "age_seconds": a,
                "running": a < EXTERNAL_FRESH_SECS,
                "progress": progress.unwrap_or_default().iter().map(TaskProgress::json).collect::<Vec<_>>(),
            }));
        }
        out.sort_by(|a, b| a["age_seconds"].as_u64().cmp(&b["age_seconds"].as_u64()));
        out
    }

    fn discard_external(&self, key: &str) -> Response {
        if !valid_id(key) {
            return msg(StatusCode::BAD_REQUEST, "Ugyldig nøkkel.");
        }
        let p = self.base.join(format!(".mapproxy_seed_progress.{key}"));
        let Ok(m) = p.metadata().and_then(|m| m.modified()) else { return msg(StatusCode::NOT_FOUND, "Fremdriftsfila finnes ikke.") };
        if age(m) < EXTERNAL_FRESH_SECS {
            return msg(StatusCode::CONFLICT, "Fila er skrevet nylig — prosessen ser ut til å kjøre.");
        }
        let _ = std::fs::remove_file(sidecar(&p));
        match std::fs::remove_file(&p) {
            Ok(()) => msg(StatusCode::OK, format!("Fremdriftsfila for {key} er slettet; neste kjøring starter fra begynnelsen.")),
            Err(e) => msg(StatusCode::INTERNAL_SERVER_ERROR, format!("Kunne ikke slette: {e}")),
        }
    }

    // ── estimate ────────────────────────────────────────────────────────────

    fn estimate(&self, req: &Json) -> Result<Json> {
        let mode = if req["mode"] == "cleanup" { Mode::Cleanup } else { Mode::Seed };
        let (conf, names, _, _) = self.build_conf(req, mode)?;
        let tasks = seed::load_tasks_from(&self.mgr, &conf, &self.dir, &names, mode)?;
        let (mut metas, mut tiles) = (0u64, 0u64);
        let mut per = vec![];
        for t in &tasks {
            let (m, n) = estimate_task(t)?;
            metas += m;
            tiles += n;
            per.push(json!({ "name": t.name, "cache": t.cg.cache, "grid": t.cg.grid, "metatiles": m, "tiles": n }));
        }
        let rate = self.recent_rate();
        Ok(json!({
            "tasks": per, "metatiles": metas, "tiles": tiles,
            "rate_tiles_per_second": rate,
            "seconds": rate.map(|r| tiles as f64 / r),
        }))
    }

    // ── cache statistics ────────────────────────────────────────────────────

    async fn stats_loop(self: Arc<Self>) {
        loop {
            self.stats.lock().scanning = true;
            let (base, t0) = (self.base.clone(), Instant::now());
            match tokio::task::spawn_blocking(move || scan_cache(&base)).await {
                Ok(mut v) => {
                    v["computed_at"] = iso(SystemTime::now()).into();
                    v["scan_seconds"] = t0.elapsed().as_secs().into();
                    info!("admin: cache scan: {} tiles in {:.0} s", v["tile_count"], t0.elapsed().as_secs_f64());
                    self.stats.lock().data = Some(v);
                }
                Err(e) => warn!("admin: cache scan: {e}"),
            }
            self.stats.lock().scanning = false;
            tokio::select! {
                _ = tokio::time::sleep(self.stats_interval) => {}
                _ = self.stats_kick.notified() => {}
            }
        }
    }

    // ── legends (tnt-mapproxy's /legend/topo-<scale>.png) ──────────────────

    async fn generate_legends(self: Arc<Self>, force: bool) {
        if self.legends_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let r = self.legends_inner(force).await;
        let v = match r {
            Ok((made, failed)) => json!({ "at": iso(SystemTime::now()), "generated": made, "failed": failed }),
            Err(e) => json!({ "at": iso(SystemTime::now()), "error": format!("{e:#}") }),
        };
        info!("admin: legends: {v}");
        *self.legends_last.lock() = Some(v);
        self.legends_running.store(false, Ordering::SeqCst);
    }

    async fn legends_inner(&self, force: bool) -> Result<(Vec<u32>, Vec<u32>)> {
        let cfg = &self.mgr.cfg;
        let cache = cfg.caches.get(&self.legend_cache).ok_or_else(|| anyhow!("no cache {}", self.legend_cache))?;
        let sources: Vec<(String, String)> = cache
            .sources
            .iter()
            .filter_map(|s| cfg.sources.get(s))
            .filter_map(|s| s.params.iter().find(|(k, _)| k == "layers").map(|(_, l)| (s.url.clone(), l.clone())))
            .collect();
        if sources.is_empty() {
            bail!("{} has no WMS sources", self.legend_cache);
        }
        std::fs::create_dir_all(&self.legend_dir)?;
        let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
        let (mut made, mut failed) = (vec![], vec![]);
        for scale in LEGEND_SCALES {
            let out = self.legend_dir.join(format!("topo-{scale}.png"));
            if out.exists() && !force {
                continue;
            }
            let fetches = sources.iter().map(|(url, layers)| {
                let sep = if url.contains('?') { '&' } else { '?' };
                let u = format!(
                    "{url}{sep}SERVICE=WMS&REQUEST=GetLegendGraphic&VERSION=1.3.0&SLD_VERSION=1.1.0&FORMAT=image%2Fpng\
                     &STYLE=default&LAYERTITLE=false&LAYER={}&SCALE={scale}",
                    url_escape(layers)
                );
                let c = client.clone();
                async move {
                    let r = async { c.get(&u).send().await?.error_for_status()?.bytes().await }.await;
                    match r {
                        Ok(b) if b.len() >= 100 => crate::image::decode_png(&b).ok().map(|(img, _)| img),
                        Ok(_) => None,
                        Err(e) => {
                            warn!("legend {layers} at 1:{scale}: {e}");
                            None
                        }
                    }
                }
            });
            let imgs: Vec<crate::image::Rgba> = futures::future::join_all(fetches).await.into_iter().flatten().collect();
            if imgs.is_empty() {
                failed.push(scale);
                continue;
            }
            let png = crate::image::encode_png_rgba(&stack_on_white(&imgs), png::Compression::Balanced)?;
            write_atomic(&out, &png)?;
            made.push(scale);
        }
        Ok((made, failed))
    }

    fn status(&self) -> Json {
        let mut jobs: Vec<Json> = {
            let mut g = self.jobs.lock();
            g.iter_mut().rev().map(|j| self.job_json(j, false)).collect()
        };
        // Running first, then newest first.
        jobs.sort_by_key(|j| !matches!(j["state"].as_str(), Some("running" | "stopping")));
        let legends: serde_json::Map<String, Json> = LEGEND_SCALES
            .iter()
            .map(|s| (s.to_string(), json!(self.legend_dir.join(format!("topo-{s}.png")).metadata().ok().map(|m| m.len()))))
            .collect();
        let (stats, scanning) = {
            let s = self.stats.lock();
            (s.data.clone(), s.scanning)
        };
        json!({
            "instance": std::env::var("TOPO_CLUSTER").ok(),
            "jobs": jobs,
            "external": self.externals(),
            "workers": self.workers_json(),
            "workers_connected": self.workers_connected(),
            "max_jobs": self.max_jobs,
            "default_concurrency": self.concurrency,
            "stall_threshold_seconds": STALL_SECS,
            "external_fresh_seconds": EXTERNAL_FRESH_SECS,
            "stats": stats,
            "stats_scanning": scanning,
            "stats_interval_seconds": self.stats_interval.as_secs(),
            "disk": disk_usage(&self.base),
            "legend_dir": self.legend_dir,
            "legend_files": legends,
            "legends_running": self.legends_running.load(Ordering::SeqCst),
            "legends_last": *self.legends_last.lock(),
        })
    }
}

impl Job {
    fn record(&self) -> Json {
        let mut r = json!({
            "id": self.id, "mode": mode_str(self.mode), "tasks": self.tasks, "names": self.names,
            "refresh": self.refresh, "concurrency": self.concurrency,
            "created_at": secs(self.created_at), "run_started_at": secs(self.run_started_at),
            "finished_at": self.finished_at.map(secs), "exit_code": self.exit_code, "state": self.state,
            "runs": self.runs, "tiles": self.tiles, "errors": self.errors,
        });
        if let Some(q) = &self.queue {
            r["distributed"] = true.into();
            r["chunks"] = q.len().into();
            r["chunks_done"] = q.ranges(false);
            r["chunks_failed"] = q.ranges(true);
        }
        r
    }

    fn from_record(r: &Json) -> Option<Job> {
        let state = match r["state"].as_str()? {
            "running" => "running",
            "stopping" => "stopping",
            "stopped" => "stopped",
            "failed" => "failed",
            "interrupted" => "interrupted",
            _ => return None,
        };
        let started = from_secs(r["run_started_at"].as_f64()?);
        Some(Job {
            id: r["id"].as_str().filter(|i| valid_id(i))?.to_string(),
            mode: if r["mode"] == "cleanup" { Mode::Cleanup } else { Mode::Seed },
            tasks: r["tasks"].clone(),
            names: r["names"].as_array()?.iter().filter_map(|n| n.as_str().map(String::from)).collect(),
            refresh: r["refresh"].as_str().unwrap_or_default().into(),
            concurrency: r["concurrency"].as_u64().unwrap_or(4) as usize,
            created_at: from_secs(r["created_at"].as_f64().unwrap_or(0.0)),
            run_started_at: started,
            run_started: Instant::now(),
            finished_at: r["finished_at"].as_f64().map(from_secs),
            exit_code: r["exit_code"].as_i64().map(|c| c as i32),
            state,
            pid: None,
            runs: r["runs"].as_u64().unwrap_or(1) as u32,
            log: VecDeque::new(),
            last_activity: started,
            tiles: r["tiles"].as_u64().unwrap_or(0),
            task_tiles: 0,
            errors: r["errors"].as_u64().unwrap_or(0),
            progress: vec![],
            queue: None,
            last_persist: Instant::now(),
        })
    }
}

fn parse_iso(s: &str) -> Option<f64> {
    // tnt-mapproxy wrote `2026-10-26T12:40:38.867573+00:00`, this module `…T12:40:38Z`.
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    let frac = if b.get(19) == Some(&b'.') {
        let end = s[20..].find(|c: char| !c.is_ascii_digit()).map_or(s.len(), |i| 20 + i);
        format!("0.{}", &s[20..end]).parse::<f64>().unwrap_or(0.0)
    } else {
        0.0
    };
    // days_from_civil
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + hh * 3600 + mm * 60 + ss) as f64 + frac)
}

fn url_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~,:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Legends one below the other on white, each blended by its alpha (tnt-mapproxy's
/// `_stack_pngs`, made opaque).
fn stack_on_white(imgs: &[crate::image::Rgba]) -> crate::image::Rgba {
    let w = imgs.iter().map(|i| i.w).max().unwrap_or(1);
    let h: u32 = imgs.iter().map(|i| i.h).sum();
    let mut out = crate::image::Rgba::filled(w, h.max(1), [255, 255, 255, 255]);
    let mut y0 = 0usize;
    for img in imgs {
        for y in 0..img.h as usize {
            for x in 0..img.w as usize {
                let s = &img.px[(y * img.w as usize + x) * 4..][..4];
                let d = &mut out.px[((y0 + y) * w as usize + x) * 4..][..4];
                let a = s[3] as u32;
                for c in 0..3 {
                    d[c] = ((s[c] as u32 * a + d[c] as u32 * (255 - a) + 127) / 255) as u8;
                }
            }
        }
        y0 += img.h as usize;
    }
    out
}

/// Metatiles and tiles a task touches: the coverage extent's tile range per level, refined by
/// the coverage test for levels with few metatiles; deeper levels reuse the last refined level's
/// hit ratio. An estimate before refresh_before skips existing tiles.
fn estimate_task(t: &seed::SeedTask) -> Result<(u64, u64)> {
    let mg = &t.cg.meta;
    let g = &mg.grid;
    let bbox = match &t.coverage {
        Some(c) => c.extent_bbox_for(&g.srs)?,
        None => g.bbox,
    };
    let (mut metas, mut tiles) = (0u64, 0u64);
    let mut ratio: Option<f64> = None;
    for &z in &t.levels {
        let delta = g.res[z] / 10.0;
        let (x0, y0, _) = g.tile_coord_for_point(bbox[0] + delta, bbox[1] + delta, z);
        let (x1, y1, _) = g.tile_coord_for_point(bbox[2] - delta, bbox[3] - delta, z);
        let gs = g.grid_sizes[z];
        let (xa, xb) = (x0.min(x1).max(0), x0.max(x1).min(gs.0 - 1));
        let (ya, yb) = (y0.min(y1).max(0), y0.max(y1).min(gs.1 - 1));
        if xa > xb || ya > yb {
            continue;
        }
        let ms = mg.meta_size(z);
        let (mx0, mx1, my0, my1) = (xa.div_euclid(ms.0), xb.div_euclid(ms.0), ya.div_euclid(ms.1), yb.div_euclid(ms.1));
        let level_metas = ((mx1 - mx0 + 1) * (my1 - my0 + 1)) as u64;
        let level_tiles = ((xb - xa + 1) * (yb - ya + 1)) as u64;
        match &t.coverage {
            Some(c) if level_metas <= 20_000 => {
                let mut hit = 0u64;
                for my in my0..=my1 {
                    for mx in mx0..=mx1 {
                        if c.intersects(mg.meta_tile((mx * ms.0, my * ms.1, z)).bbox, &g.srs)? {
                            hit += 1;
                        }
                    }
                }
                metas += hit;
                tiles += level_tiles * hit / level_metas.max(1);
                ratio = Some(hit as f64 / level_metas.max(1) as f64);
            }
            Some(_) if ratio.is_some() => {
                let r = ratio.unwrap_or(1.0);
                metas += (level_metas as f64 * r).round() as u64;
                tiles += (level_tiles as f64 * r).round() as u64;
            }
            _ => {
                metas += level_metas;
                tiles += level_tiles;
            }
        }
    }
    Ok((metas, tiles))
}

#[derive(Default)]
struct Tally {
    count: u64,
    bytes: u64,
    mtime: i64,
    /// Hardlinked single-colour tiles: counted once per inode.
    links: HashMap<u64, u64>,
}

fn walk(dir: &Path, t: &mut Tally) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&e.path(), t);
        } else if let Ok(m) = e.metadata() {
            t.count += 1;
            if m.nlink() > 1 {
                t.links.insert(m.ino(), m.len());
            } else {
                t.bytes += m.len();
            }
            t.mtime = t.mtime.max(m.mtime());
        }
    }
}

/// Tiles per cache directory and level (`<base>/<cache>_<srs>/<zz>/…`), 8 directory walkers.
fn scan_cache(base: &Path) -> Json {
    let mut work: Vec<(usize, String, PathBuf)> = vec![];
    let mut caches: Vec<String> = vec![];
    let mut entries: Vec<_> = std::fs::read_dir(base).into_iter().flatten().flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let mut levels: Vec<(String, PathBuf)> = std::fs::read_dir(e.path())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|z| {
                let n = z.file_name().to_string_lossy().into_owned();
                (n.len() == 2 && n.bytes().all(|b| b.is_ascii_digit()) && z.file_type().is_ok_and(|t| t.is_dir())).then(|| (n, z.path()))
            })
            .collect();
        if levels.is_empty() {
            continue;
        }
        levels.sort();
        for (z, p) in levels {
            work.push((caches.len(), z, p));
        }
        caches.push(name);
    }
    let queue = Mutex::new(work.into_iter().enumerate().collect::<Vec<_>>());
    let results: Mutex<Vec<(usize, usize, String, Tally)>> = Mutex::new(vec![]);
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                loop {
                    let Some((i, (ci, z, p))) = queue.lock().pop() else { break };
                    let mut t = Tally::default();
                    walk(&p, &mut t);
                    results.lock().push((i, ci, z, t));
                }
            });
        }
    });
    let mut results = results.into_inner();
    results.sort_by_key(|r| r.0);
    let mut out = vec![];
    let (mut all_tiles, mut all_bytes) = (0u64, 0u64);
    for (ci, name) in caches.iter().enumerate() {
        let mut links: HashMap<u64, u64> = HashMap::new();
        let mut levels = vec![];
        let (mut count, mut bytes, mut mtime) = (0u64, 0u64, 0i64);
        for (_, _, z, t) in results.iter().filter(|r| r.1 == ci) {
            let link_bytes: u64 = t.links.iter().filter(|(ino, _)| !links.contains_key(ino)).map(|(_, b)| b).sum();
            links.extend(t.links.iter().map(|(a, b)| (*a, *b)));
            levels.push(json!({
                "zoom": z.parse::<u32>().unwrap_or(0), "count": t.count, "size_bytes": t.bytes + link_bytes,
                "last_modified": (t.mtime > 0).then(|| iso(UNIX_EPOCH + Duration::from_secs(t.mtime as u64))),
            }));
            count += t.count;
            bytes += t.bytes + link_bytes;
            mtime = mtime.max(t.mtime);
        }
        all_tiles += count;
        all_bytes += bytes;
        out.push(json!({
            "dir": name, "count": count, "size_bytes": bytes,
            "last_modified": (mtime > 0).then(|| iso(UNIX_EPOCH + Duration::from_secs(mtime as u64))),
            "zoom_levels": levels,
        }));
    }
    json!({ "caches": out, "tile_count": all_tiles, "cache_size_bytes": all_bytes })
}

fn disk_usage(p: &Path) -> Json {
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) else { return Json::Null };
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Json::Null;
    }
    let f = s.f_frsize as u64;
    json!({ "free_bytes": s.f_bavail as u64 * f, "total_bytes": s.f_blocks as u64 * f })
}

/// seed.yaml's tasks for the GUI: levels, caches, grids and the coverage extent in WGS84.
fn task_list(mgr: &TileManager, conf: &Yaml, dir: &Path) -> Json {
    let wgs = crate::srs::Srs::new("EPSG:4326").ok();
    let mut bboxes: HashMap<String, Option<[f64; 4]>> = HashMap::new();
    let abspath = |p: &str| {
        let p = PathBuf::from(p);
        if p.is_absolute() { p } else { dir.join(p) }
    };
    let mut out = vec![];
    for (name, s) in conf.get("seeds").and_then(|s| s.as_mapping()).into_iter().flatten() {
        let Some(name) = name.as_str() else { continue };
        let strs = |k: &str| -> Vec<String> {
            s.get(k).and_then(|v| v.as_sequence()).into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect()
        };
        let (caches, grids, covs) = (strs("caches"), strs("grids"), strs("coverages"));
        let max = grids.first().and_then(|g| mgr.cfg.grids.get(g)).map_or(19, |g| g.res.len()) as u64;
        let (lf, lt) = match s.get("levels") {
            Some(Yaml::Mapping(m)) => (
                m.get("from").and_then(|v| v.as_u64()).unwrap_or(0),
                m.get("to").and_then(|v| v.as_u64()).unwrap_or(max - 1),
            ),
            Some(Yaml::Sequence(l)) => {
                let l: Vec<u64> = l.iter().filter_map(|v| v.as_u64()).collect();
                (l.iter().copied().min().unwrap_or(0), l.iter().copied().max().unwrap_or(max - 1))
            }
            _ => (0, max - 1),
        };
        let bbox = covs.first().and_then(|c| {
            bboxes
                .entry(c.clone())
                .or_insert_with(|| {
                    let conf = conf.get("coverages")?.get(c.as_str())?;
                    let cov = crate::coverage::Coverage::from_conf(conf, &abspath).ok()?;
                    let b = cov.srs.transform_bbox(wgs.as_ref()?, cov.bbox).ok()?;
                    Some(b.map(|v| (v * 1e4).round() / 1e4))
                })
                .to_owned()
        });
        out.push(json!({
            "name": name, "levels_from": lf, "levels_to": lt, "caches": caches, "grids": grids,
            "bbox": bbox, "refresh_before": s.get("refresh_before").is_some(),
        }));
    }
    json!({ "tasks": out })
}

// ── HTTP ────────────────────────────────────────────────────────────────────

pub fn router(admin: Arc<Admin>) -> Router {
    {
        let a = admin.clone();
        tokio::spawn(a.stats_loop());
        let a = admin.clone();
        tokio::spawn(a.generate_legends(false));
    }
    Router::new()
        .route("/_admin", get(index))
        .route("/_admin/", get(index))
        .route("/_admin/status", get(|State(a): State<Arc<Admin>>| async move { resp(StatusCode::OK, a.status()) }))
        .route("/_admin/tasks", get(|State(a): State<Arc<Admin>>| async move { resp(StatusCode::OK, a.tasks_json.clone()) }))
        .route("/_admin/history", get(history))
        .route("/_admin/estimate", post(estimate))
        .route("/_admin/jobs", post(start))
        .route("/_admin/jobs/{id}", get(job))
        .route("/_admin/jobs/{id}/stop", post(|State(a): State<Arc<Admin>>, UrlPath(id): UrlPath<String>| async move { a.stop(&id) }))
        .route("/_admin/jobs/{id}/resume", post(|State(a): State<Arc<Admin>>, UrlPath(id): UrlPath<String>| async move { a.resume(&id) }))
        .route("/_admin/jobs/{id}/discard", post(|State(a): State<Arc<Admin>>, UrlPath(id): UrlPath<String>| async move { a.discard(&id) }))
        .route(
            "/_admin/external/{key}/discard",
            post(|State(a): State<Arc<Admin>>, UrlPath(k): UrlPath<String>| async move { a.discard_external(&k) }),
        )
        .route("/_admin/work/claim", post(work_claim))
        .route("/_admin/work/heartbeat", post(work_heartbeat))
        .route("/_admin/work/complete", post(work_complete))
        .route("/_admin/work/release", post(work_release))
        .route("/_admin/stats/refresh", post(stats_refresh))
        .route("/_admin/legends", post(legends))
        .route("/_admin/{file}", get(asset))
        .with_state(admin)
}

fn parse_body(body: &Bytes) -> Result<Json, Response> {
    serde_json::from_slice(body).map_err(|e| msg(StatusCode::BAD_REQUEST, format!("Ugyldig JSON: {e}")))
}

async fn index(State(a): State<Arc<Admin>>) -> Response {
    serve_asset(&a, "admin.html")
}

async fn asset(State(a): State<Arc<Admin>>, UrlPath(file): UrlPath<String>) -> Response {
    serve_asset(&a, &file)
}

fn serve_asset(a: &Admin, file: &str) -> Response {
    let ctype = match file.rsplit_once('.').map(|(_, e)| e) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        _ => return msg(StatusCode::NOT_FOUND, "Not found"),
    };
    if !valid_id(file) {
        return msg(StatusCode::NOT_FOUND, "Not found");
    }
    match std::fs::read(a.opts.assets.join(file)) {
        Ok(d) => ([(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "no-store")], d).into_response(),
        Err(_) => msg(StatusCode::NOT_FOUND, "Not found"),
    }
}

async fn history(State(a): State<Arc<Admin>>) -> Response {
    let mut h = a.read_history();
    h.reverse();
    resp(StatusCode::OK, Json::Array(h))
}

async fn estimate(State(a): State<Arc<Admin>>, body: Bytes) -> Response {
    let req = match parse_body(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    match tokio::task::spawn_blocking(move || a.estimate(&req)).await {
        Ok(Ok(v)) => resp(StatusCode::OK, v),
        Ok(Err(e)) => msg(StatusCode::BAD_REQUEST, format!("{e:#}")),
        Err(e) => msg(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn start(State(a): State<Arc<Admin>>, body: Bytes) -> Response {
    let req = match parse_body(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    if req["distributed"] != true && a.running_count() >= a.max_jobs {
        return msg(StatusCode::CONFLICT, format!("Maks {} samtidige jobber kjører allerede i admin-poden. Stopp en av dem, eller fordel jobben på arbeiderne.", a.max_jobs));
    }
    // Chunking a national job tests ~10⁵–10⁶ blocks against the coverage: off the runtime.
    match tokio::task::spawn_blocking(move || a.start(&req)).await {
        Ok(Ok(v)) => resp(StatusCode::OK, v),
        Ok(Err(e)) => msg(StatusCode::BAD_REQUEST, format!("Kunne ikke starte: {e:#}")),
        Err(e) => msg(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn job(State(a): State<Arc<Admin>>, UrlPath(id): UrlPath<String>) -> Response {
    let mut jobs = a.jobs.lock();
    match jobs.iter_mut().find(|j| j.id == id) {
        Some(j) => resp(StatusCode::OK, a.job_json(j, true)),
        None => msg(StatusCode::NOT_FOUND, "Ukjent jobb."),
    }
}

async fn work_claim(State(a): State<Arc<Admin>>, body: Bytes) -> Response {
    let req = match parse_body(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let Some(worker) = req["worker"].as_str().filter(|w| !w.is_empty() && w.len() <= 100) else {
        return msg(StatusCode::BAD_REQUEST, "worker mangler");
    };
    match a.claim(worker) {
        Some(c) => resp(StatusCode::OK, c),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn work_heartbeat(State(a): State<Arc<Admin>>, body: Bytes) -> Response {
    match parse_body(&body) {
        Ok(b) => resp(StatusCode::OK, a.heartbeat(&b)),
        Err(r) => r,
    }
}

async fn work_complete(State(a): State<Arc<Admin>>, body: Bytes) -> Response {
    match parse_body(&body) {
        Ok(b) => {
            a.complete(&b);
            resp(StatusCode::OK, json!({}))
        }
        Err(r) => r,
    }
}

async fn work_release(State(a): State<Arc<Admin>>, body: Bytes) -> Response {
    match parse_body(&body) {
        Ok(b) => {
            a.release(&b);
            resp(StatusCode::OK, json!({}))
        }
        Err(r) => r,
    }
}

async fn stats_refresh(State(a): State<Arc<Admin>>) -> Response {
    a.stats_kick.notify_one();
    msg(StatusCode::OK, "Ny opptelling startet.")
}

async fn legends(State(a): State<Arc<Admin>>) -> Response {
    if a.legends_running.load(Ordering::SeqCst) {
        return msg(StatusCode::CONFLICT, "Tegnforklaringer genereres allerede.");
    }
    tokio::spawn(a.generate_legends(true));
    msg(StatusCode::OK, "Tegnforklaringer genereres på nytt i bakgrunnen.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_roundtrip() {
        let t = UNIX_EPOCH + Duration::from_secs(1_793_096_130);
        assert_eq!(iso(t), "2026-10-27T10:15:30Z");
        assert_eq!(parse_iso("2026-10-27T10:15:30Z"), Some(1_793_096_130.0));
        assert_eq!(parse_iso("2026-10-26T12:40:38.5+00:00"), Some(1_793_018_438.5));
        assert_eq!(iso(UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn progress_ids_and_percent() {
        let dir = std::env::temp_dir().join(format!("rmp-admin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("j.progress");
        std::fs::write(sidecar(&p), r#"{"oslo_city|topo_cache|norway_25833|[10, 11, 12]": [5, 10], "bergen_city|topo_cache|norway_25833|[10]": [3, 3]}"#)
            .unwrap();
        let pr = read_progress(&p).unwrap();
        let oslo = pr.iter().find(|t| t.name == "oslo_city").unwrap();
        assert_eq!((oslo.levels.clone(), oslo.done, oslo.total), (vec![10, 11, 12], 5, 10));
        let names = vec!["oslo_city".to_string(), "bergen_city".to_string(), "tromsoe_city".to_string()];
        assert_eq!(percent(&names, &pr).map(|p| p.round()), Some(50.0));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ansi_and_ids() {
        assert_eq!(strip_ansi("\x1b[32m INFO\x1b[0m done\r"), " INFO done");
        assert!(valid_id("20261027-101530-seed"));
        assert!(!valid_id("../x") && !valid_id(".hidden") && !valid_id("a/b"));
        assert_eq!(url_escape("a b,c&d"), "a%20b,c%26d");
    }

    #[test]
    fn legend_stack_blends_on_white() {
        let a = crate::image::Rgba::filled(2, 1, [0, 0, 0, 255]);
        let b = crate::image::Rgba::filled(1, 2, [0, 0, 0, 0]);
        let s = stack_on_white(&[a, b]);
        assert_eq!((s.w, s.h), (2, 3));
        assert_eq!(&s.px[0..4], &[0, 0, 0, 255]);
        assert_eq!(&s.px[8..12], &[255, 255, 255, 255]);
    }
}
