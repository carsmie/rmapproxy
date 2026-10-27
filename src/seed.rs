//! `mapproxy-seed` (`seed/seeder.py`, `seed/cleanup.py`, `seed/config.py`): depth-first
//! TileWalker over metatiles intersecting the task coverage, refresh_before / overwrite-all,
//! cleanups, N concurrent metatiles, progress lines in mapproxy-seed's format and the
//! `--progress-file` pickle that tnt-mapproxy's admin app reads.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use futures::FutureExt;
use serde_yaml::Value;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::cache::write_atomic;
use crate::config::load_yaml_with_bases;
use crate::coverage::Coverage;
use crate::grid::TileCoord;
use crate::pickle::{Py, dumps_dict};
use crate::srs::BBox;
use crate::tile::{CacheGrid, TileManager};

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Seed,
    Cleanup,
}

pub struct SeedTask {
    pub name: String,
    pub cg: Arc<CacheGrid>,
    pub levels: Vec<usize>,
    pub coverage: Option<Arc<Coverage>>,
    /// Seed: re-render tiles older than this (`None` = all). Cleanup: remove tiles older
    /// than this (`None` = all).
    pub before: Option<SystemTime>,
}

impl SeedTask {
    /// `task.id`: (name, cache, grid, tuple(levels)) — the progress-file key.
    fn id(&self) -> String {
        format!("{}|{}|{}|{:?}", self.name, self.cg.cache, self.cg.grid, self.levels)
    }

}

#[derive(PartialEq, Clone, Copy)]
enum Intersection {
    None,
    Intersects,
    Contains,
}

fn levels_of(v: Option<&Value>, max: usize) -> Vec<usize> {
    match v {
        Some(Value::Sequence(s)) => s.iter().filter_map(|l| l.as_u64().map(|l| l as usize)).filter(|&l| l < max).collect(),
        Some(Value::Mapping(m)) => {
            let from = m.get("from").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let to = m.get("to").and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(max - 1);
            (from..=to.min(max - 1)).collect()
        }
        _ => (0..max).collect(),
    }
}

/// `before_timestamp_from_options` (weeks/days/hours/minutes/seconds).
fn before_timestamp(v: Option<&Value>) -> Result<Option<SystemTime>> {
    let Some(v) = v else { return Ok(None) };
    if v.get("time").is_some() || v.get("mtime").is_some() {
        bail!("refresh_before/remove_before time/mtime not supported");
    }
    let secs = |k: &str, f: f64| v.get(k).and_then(|x| x.as_f64().or(x.as_i64().map(|i| i as f64))).unwrap_or(0.0) * f;
    let delta = secs("weeks", 604800.0) + secs("days", 86400.0) + secs("hours", 3600.0) + secs("minutes", 60.0) + secs("seconds", 1.0);
    let delta = Duration::try_from_secs_f64(delta).map_err(|_| anyhow!("invalid refresh_before/remove_before {v:?}"))?;
    Ok(Some(SystemTime::now().checked_sub(delta).unwrap_or(SystemTime::UNIX_EPOCH)))
}

/// Tasks named in `names` from the `seeds:` or `cleanups:` section (one per cache × grid).
pub fn load_tasks(mgr: &TileManager, seed_conf: &Path, names: &[String], mode: Mode) -> Result<Vec<SeedTask>> {
    let v = load_yaml_with_bases(seed_conf)?;
    load_tasks_from(mgr, &v, seed_conf.parent().unwrap_or(Path::new(".")), names, mode)
}

/// `load_tasks` from an already loaded seed configuration; relative datasources resolve
/// against `dir` (the admin estimates its job configurations without writing them first).
pub fn load_tasks_from(mgr: &TileManager, v: &Value, dir: &Path, names: &[String], mode: Mode) -> Result<Vec<SeedTask>> {
    let abspath = |p: &str| {
        let p = PathBuf::from(p);
        if p.is_absolute() { p } else { dir.join(p) }
    };
    let (section, before_key) = match mode {
        Mode::Seed => ("seeds", "refresh_before"),
        Mode::Cleanup => ("cleanups", "remove_before"),
    };
    let mut tasks = vec![];
    for name in names {
        let s = v
            .get(section)
            .and_then(|s| s.get(name.as_str()))
            .ok_or_else(|| anyhow!("unknown {section} task {name}"))?;
        let caches: Vec<String> = s
            .get("caches")
            .and_then(|c| c.as_sequence())
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str().map(String::from))
            .collect();
        let mut coverages = vec![];
        for c in s.get("coverages").and_then(|c| c.as_sequence()).into_iter().flatten() {
            let cname = c.as_str().unwrap_or_default();
            let conf = v
                .get("coverages")
                .and_then(|cs| cs.get(cname))
                .ok_or_else(|| anyhow!("{name}: unknown coverage {cname}"))?;
            coverages.push(Arc::new(Coverage::from_conf(conf, &abspath).with_context(|| format!("coverage {cname}"))?));
        }
        if coverages.len() > 1 {
            bail!("{name}: multiple coverages are not supported");
        }
        let before = before_timestamp(s.get(before_key))?;
        for cache in &caches {
            let c = mgr.cfg.caches.get(cache).ok_or_else(|| anyhow!("{name}: unknown cache {cache}"))?;
            let grids: Vec<String> = match s.get("grids").and_then(|g| g.as_sequence()) {
                Some(g) => g.iter().filter_map(|g| g.as_str().map(String::from)).collect(),
                None => c.grids.clone(),
            };
            for g in grids {
                let cg = mgr.cache_grid(cache, &g).ok_or_else(|| anyhow!("{name}: cache {cache} has no grid {g}"))?;
                let levels = levels_of(s.get("levels"), cg.meta.grid.res.len());
                // SeedTask: coverage.transform_to(grid.srs) once, then tested in grid coords.
                let coverage = match coverages.first() {
                    Some(c) => Some(Arc::new(c.transform_to(&cg.meta.grid.srs).with_context(|| format!("{name}: coverage"))?)),
                    None => None,
                };
                tasks.push(SeedTask { name: name.clone(), cg, levels, coverage, before });
            }
        }
    }
    Ok(tasks)
}

struct Walker<'a> {
    task: &'a SeedTask,
    seeded: Vec<HashSet<TileCoord>>,
    out: Vec<(usize, TileCoord)>,
}

impl Walker<'_> {
    /// `SeedTask.intersects` / `CleanupTask.intersects`.
    fn intersects(&self, bbox: BBox) -> Result<Intersection> {
        let Some(cov) = &self.task.coverage else { return Ok(Intersection::Contains) };
        let srs = &self.task.cg.meta.grid.srs;
        Ok(if cov.contains(bbox, srs)? {
            Intersection::Contains
        } else if cov.intersects(bbox, srs)? {
            Intersection::Intersects
        } else {
            Intersection::None
        })
    }

    /// `TileWalker._walk`.
    fn walk(&mut self, cur: BBox, levels: &[usize], z: usize, all_subtiles: bool) -> Result<()> {
        let mg = &self.task.cg.meta;
        if z >= mg.grid.res.len() {
            return Ok(());
        }
        let (_, subtiles) = mg.affected_level_tiles(cur, z);
        let (process, levels) = match levels.first() {
            Some(&l) if l == z => (true, &levels[1..]),
            _ => (false, levels),
        };
        for st in subtiles.into_iter().flatten() {
            let sub_bbox = mg.meta_tile(st).bbox;
            let inter = if all_subtiles { Intersection::Contains } else { self.intersects(sub_bbox)? };
            if inter == Intersection::None {
                continue;
            }
            if !levels.is_empty() {
                let limited = [cur[0].max(sub_bbox[0]), cur[1].max(sub_bbox[1]), cur[2].min(sub_bbox[2]), cur[3].min(sub_bbox[3])];
                self.walk(limited, levels, z + 1, inter == Intersection::Contains)?;
            }
            if process && self.seeded[z].insert(st) {
                self.out.push((z, st));
            }
        }
        Ok(())
    }
}

/// Metatiles of a task in mapproxy-seed's processing order.
pub fn plan(task: &SeedTask) -> Result<Vec<(usize, TileCoord)>> {
    let srs = &task.cg.meta.grid.srs;
    let bbox = match &task.coverage {
        Some(c) => c.extent_bbox_for(srs)?,
        None => task.cg.meta.grid.bbox,
    };
    let mut w = Walker { task, seeded: vec![HashSet::new(); task.cg.meta.grid.res.len()], out: vec![] };
    w.walk(bbox, &task.levels, 0, false)?;
    Ok(w.out)
}

/// Seed with refresh_before: any tile of the metatile missing or older than the cutoff.
fn needs_work(task: &SeedTask, main: TileCoord) -> bool {
    let Some(before) = task.before else { return true };
    task.cg
        .meta
        .meta_tile(main)
        .tiles
        .iter()
        .filter_map(|t| t.0)
        .any(|t| task.cg.file.mtime(t).is_none_or(|m| m < before))
}

/// Cleanup of one metatile: remove its tiles (all, or those older than the cutoff).
fn cleanup_meta(task: &SeedTask, main: TileCoord) -> Result<usize> {
    let mut n = 0;
    for t in task.cg.meta.meta_tile(main).tiles.iter().filter_map(|t| t.0) {
        let p = task.cg.file.tile_path(t);
        let old = match task.before {
            None => p.symlink_metadata().is_ok(),
            Some(b) => p.symlink_metadata().and_then(|m| m.modified()).is_ok_and(|m| m < b),
        };
        if old && std::fs::remove_file(&p).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

/// One metatile of a task: seed (if `needs_work`) under the metatile lock, or clean up.
/// Shared by `run` and the admin's seed workers.
pub async fn process_meta(mgr: &Arc<TileManager>, t: &Arc<SeedTask>, main: TileCoord, seeding: bool) -> Result<usize> {
    if !seeding {
        let t2 = t.clone();
        return match tokio::task::spawn_blocking(move || cleanup_meta(&t2, main)).await {
            Ok(r) => r,
            Err(e) => Err(anyhow!("{e}")),
        };
    }
    if !needs_work(t, main) {
        return Ok(0);
    }
    let meta = t.cg.meta.meta_tile(main);
    let lock_cg = t.cg.clone();
    let _l = tokio::task::spawn_blocking(move || lock_cg.file.lock(main)).await??;
    // Another seeder (or `serve`) may have rendered it while we waited for the lock.
    if !needs_work(t, main) {
        return Ok(0);
    }
    // A source error (on_error: transparent) leaves the metatile unstored: MapProxy's seeder
    // counts that as done; here it is an error, so it shows in the summary/exit code and the
    // resume point stays before it.
    match mgr.create_meta(&t.cg, &meta, true).await? {
        (tiles, true) => Ok(tiles.len()),
        (_, false) => Err(anyhow!("not stored: a source failed (see the warnings above)")),
    }
}

/// Metatile index range `(mx0, mx1, my0, my1)` (main tile = `(mx * ms.0, my * ms.1, z)`) of the
/// task's coverage extent at level `z`, clipped to the grid: what `affected_level_tiles`
/// visits, without materialising it (the admin's chunks and estimates).
pub fn level_meta_range(task: &SeedTask, z: usize) -> Result<Option<(i64, i64, i64, i64)>> {
    let mg = &task.cg.meta;
    let g = &mg.grid;
    let bbox = match &task.coverage {
        Some(c) => c.extent_bbox_for(&g.srs)?,
        None => g.bbox,
    };
    let delta = g.res[z] / 10.0;
    let (x0, y0, _) = g.tile_coord_for_point(bbox[0] + delta, bbox[1] + delta, z);
    let (x1, y1, _) = g.tile_coord_for_point(bbox[2] - delta, bbox[3] - delta, z);
    let gs = g.grid_sizes[z];
    let (xa, xb) = (x0.min(x1).max(0), x0.max(x1).min(gs.0 - 1));
    let (ya, yb) = (y0.min(y1).max(0), y0.max(y1).min(gs.1 - 1));
    if xa > xb || ya > yb {
        return Ok(None);
    }
    let ms = mg.meta_size(z);
    Ok(Some((xa.div_euclid(ms.0), xb.div_euclid(ms.0), ya.div_euclid(ms.1), yb.div_euclid(ms.1))))
}

/// Does metatile `main` touch the task's coverage (the TileWalker's per-level test)?
pub fn meta_in_coverage(task: &SeedTask, main: TileCoord) -> Result<bool> {
    match &task.coverage {
        Some(c) => c.intersects(task.cg.meta.meta_tile(main).bbox, &task.cg.meta.grid.srs),
        None => Ok(true),
    }
}

/// Cleanup without coverage (`complete_extent`): whole level directories.
fn cleanup_levels(task: &SeedTask) -> Result<usize> {
    let mut n = 0;
    for &z in &task.levels {
        let dir = task.cg.file.dir.join(format!("{z:02}"));
        if !dir.exists() {
            continue;
        }
        match task.before {
            None => {
                n += count_files(&dir);
                std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
            }
            Some(b) => n += remove_older(&dir, b)?,
        }
    }
    Ok(n)
}

fn count_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| if e.path().is_dir() { count_files(&e.path()) } else { 1 })
        .sum()
}

fn remove_older(dir: &Path, before: SystemTime) -> Result<usize> {
    let mut n = 0;
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        if p.is_dir() {
            n += remove_older(&p, before)?;
        } else if e.metadata().and_then(|m| m.modified()).is_ok_and(|m| m < before) && std::fs::remove_file(&p).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

/// `--progress-file`: the pickle tnt-mapproxy's admin reads (task-id keys) plus a JSON
/// sidecar with the finished-prefix length per task, which `--continue` resumes from.
pub struct Progress {
    path: PathBuf,
    done: BTreeMap<String, (usize, usize)>,
    keys: BTreeMap<String, SeedKey>,
    last_write: Instant,
}

/// Owned copy of a task's pickle key parts.
struct SeedKey {
    name: String,
    cache: String,
    grid: String,
    levels: Vec<usize>,
}

impl SeedKey {
    fn py(&self) -> Py {
        Py::Tuple(vec![
            Py::Str(self.name.clone()),
            Py::Str(self.cache.clone()),
            Py::Str(self.grid.clone()),
            Py::Tuple(self.levels.iter().map(|&l| Py::Int(l as i64)).collect()),
        ])
    }
}

impl Progress {
    pub fn new(path: PathBuf, resume: bool) -> Progress {
        let mut done = BTreeMap::new();
        if resume && let Ok(s) = std::fs::read_to_string(Self::sidecar(&path)) && let Ok(serde_json::Value::Object(m)) = serde_json::from_str::<serde_json::Value>(&s) {
            for (k, v) in m {
                if let (Some(d), Some(t)) = (v[0].as_u64(), v[1].as_u64()) {
                    done.insert(k, (d as usize, t as usize));
                }
            }
        }
        Progress { path, done, keys: BTreeMap::new(), last_write: Instant::now() - Duration::from_secs(3600) }
    }

    fn sidecar(path: &Path) -> PathBuf {
        PathBuf::from(format!("{}.rmapproxy.json", path.display()))
    }

    /// Metatiles of this task already done in an earlier run with the same plan.
    fn resume_point(&self, task: &SeedTask, total: usize) -> usize {
        match self.done.get(&task.id()) {
            Some(&(d, t)) if t == total => d,
            _ => 0,
        }
    }

    fn update(&mut self, task: &SeedTask, done: usize, total: usize, force: bool) {
        self.keys.entry(task.id()).or_insert_with(|| SeedKey {
            name: task.name.clone(),
            cache: task.cg.cache.clone(),
            grid: task.cg.grid.clone(),
            levels: task.levels.clone(),
        });
        self.done.insert(task.id(), (done, total));
        if force || self.last_write.elapsed() > Duration::from_secs(5) {
            self.write();
        }
    }

    fn write(&mut self) {
        self.last_write = Instant::now();
        let items: Vec<(Py, Py)> = self
            .keys
            .iter()
            .map(|(id, k)| {
                let (d, t) = self.done[id];
                (k.py(), Py::Tuple(vec![Py::Str("rmapproxy".into()), Py::Int(d as i64), Py::Int(t as i64)]))
            })
            .collect();
        let json: serde_json::Map<String, serde_json::Value> =
            self.done.iter().map(|(k, (d, t))| (k.clone(), serde_json::json!([d, t]))).collect();
        if let Err(e) = write_atomic(&self.path, &dumps_dict(&items))
            .and_then(|_| write_atomic(&Self::sidecar(&self.path), serde_json::Value::Object(json).to_string().as_bytes()))
        {
            eprintln!("unable to write seed progress: {e:#}");
        }
    }

    /// mapproxy-seed removes its progress file when everything finished.
    fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(Self::sidecar(&self.path));
    }
}

type Finished = (usize, usize, TileCoord, Result<usize>);

struct Counters {
    finished: BTreeSet<usize>,
    prefix: usize,
    tiles: usize,
    errors: usize,
}

impl Counters {
    fn record(&mut self, r: Finished, task: &SeedTask, total: usize, progress: &mut Option<Progress>) -> (usize, TileCoord) {
        let (i, z, main, r) = r;
        match r {
            Ok(n) => {
                self.tiles += n;
                // Only a contiguous prefix of successful metatiles can be resumed safely;
                // a failed one keeps the resume point before it.
                self.finished.insert(i);
            }
            Err(e) => {
                self.errors += 1;
                eprintln!("error in metatile {main:?}: {e:#}");
            }
        }
        while self.finished.remove(&self.prefix) {
            self.prefix += 1;
        }
        if let Some(p) = progress.as_mut() {
            p.update(task, self.prefix, total, false);
        }
        (z, main)
    }
}

pub async fn run(mgr: Arc<TileManager>, tasks: Vec<SeedTask>, mode: Mode, concurrency: usize, mut progress: Option<Progress>) -> Result<()> {
    let seeding = mode == Mode::Seed;
    let verb = if seeding { "Seeding" } else { "Cleaning up" };
    println!("========== {verb} tasks ==========");
    println!(
        "Start {} process ({} task{})",
        if seeding { "seeding" } else { "cleanup" },
        tasks.len(),
        if tasks.len() == 1 { "" } else { "s" }
    );
    let mut failed = 0usize;
    for task in tasks {
        let srs = &task.cg.meta.grid.srs;
        println!("  {}:", task.name);
        println!("    {verb} cache '{}' with grid '{}' in {}", task.cg.cache, task.cg.grid, srs.code);
        if let Some(c) = &task.coverage {
            let wgs = crate::srs::Srs::new("EPSG:4326")?;
            let b = c.srs.transform_bbox(&wgs, c.bbox)?;
            println!("    Limited to coverage in: {:.5}, {:.5}, {:.5}, {:.5} (EPSG:4326)", b[0], b[1], b[2], b[3]);
        } else {
            println!("    Complete extent");
        }
        println!("    Levels: {:?}", task.levels);
        match (seeding, task.before) {
            // fmt_http_date panics before 1970 (refresh_before of over ~56 years).
            (true, Some(t)) => println!("    Overwriting: tiles older than {}", httpdate::fmt_http_date(t.max(SystemTime::UNIX_EPOCH))),
            (true, None) => println!("    Overwriting: all tiles"),
            (false, Some(t)) => println!("    Remove: tiles older than {}", httpdate::fmt_http_date(t.max(SystemTime::UNIX_EPOCH))),
            (false, None) => println!("    Remove: all tiles"),
        }

        if !seeding && task.coverage.is_none() {
            let n = cleanup_levels(&task)?;
            println!("    removed {n} tiles");
            continue;
        }

        let metas = plan(&task)?;
        let total = metas.len();
        let start = progress.as_ref().map_or(0, |p| p.resume_point(&task, total));
        if start > 0 {
            println!("    continuing after {start} of {total} metatiles");
        }
        let task = Arc::new(task);
        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut set: JoinSet<Finished> = JoinSet::new();
        let (t0, mut last) = (Instant::now(), Instant::now());
        let mut c = Counters { finished: BTreeSet::new(), prefix: start, tiles: 0, errors: 0 };
        let report = |z: usize, main: TileCoord, c: &Counters| {
            let b = task.cg.meta.meta_tile(main).bbox;
            println!(
                "[{}] {:2} {:6.2}% {:.5}, {:.5}, {:.5}, {:.5} ({} tiles)",
                clock(),
                z,
                if total == 0 { 100.0 } else { 100.0 * c.prefix as f64 / total as f64 },
                b[0],
                b[1],
                b[2],
                b[3],
                c.tiles
            );
        };
        for (i, (z, main)) in metas.into_iter().enumerate().skip(start) {
            let permit = sem.clone().acquire_owned().await?;
            let (m, t) = (mgr.clone(), task.clone());
            set.spawn(async move {
                let _p = permit;
                // A panic is one failed metatile, not the end of the run (and its progress).
                let r = std::panic::AssertUnwindSafe(process_meta(&m, &t, main, seeding))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|p| Err(anyhow!("panic: {}", panic_message(&*p))));
                (i, z, main, r)
            });
            while let Some(r) = set.try_join_next() {
                let (z, main) = c.record(r?, &task, total, &mut progress);
                if last.elapsed() > Duration::from_secs(30) {
                    report(z, main, &c);
                    last = Instant::now();
                }
            }
        }
        let mut last_seen = None;
        while let Some(r) = set.join_next().await {
            let (z, main) = c.record(r?, &task, total, &mut progress);
            last_seen = Some((z, main));
            if last.elapsed() > Duration::from_secs(30) {
                report(z, main, &c);
                last = Instant::now();
            }
        }
        if let Some((z, main)) = last_seen {
            report(z, main, &c);
        }
        if let Some(p) = progress.as_mut() {
            p.update(&task, c.prefix, total, true);
        }
        println!(
            "    {} metatiles, {} tiles {}, {} errors in {:.1} s",
            total - start,
            c.tiles,
            if seeding { "rendered" } else { "removed" },
            c.errors,
            t0.elapsed().as_secs_f64()
        );
        failed += c.errors;
    }
    if failed > 0 {
        bail!("{failed} metatiles failed");
    }
    if let Some(p) = &progress {
        p.remove();
    }
    Ok(())
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> &str {
    p.downcast_ref::<&str>().copied().or_else(|| p.downcast_ref::<String>().map(String::as_str)).unwrap_or("?")
}

fn clock() -> String {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
    let s = now % 86400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}
