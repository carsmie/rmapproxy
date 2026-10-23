//! `mapproxy-seed --seed` (`seed/seeder.py`, `seed/config.py`): depth-first TileWalker over
//! metatiles intersecting the task coverage, refresh_before / overwrite-all, N concurrent
//! metatiles, progress lines in mapproxy-seed's format (parsed by tnt-mapproxy's admin app).

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use serde_yaml::Value;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config::load_yaml_with_bases;
use crate::coverage::Coverage;
use crate::grid::TileCoord;
use crate::srs::BBox;
use crate::tile::{CacheGrid, TileManager};

pub struct SeedTask {
    pub name: String,
    pub cg: Arc<CacheGrid>,
    pub levels: Vec<usize>,
    pub coverage: Option<Arc<Coverage>>,
    /// Tiles older than this are re-rendered; `None` = refresh all.
    pub refresh_before: Option<SystemTime>,
}

#[derive(PartialEq, Clone, Copy)]
enum Intersection {
    None,
    Intersects,
    Contains,
}

fn levels_of(v: Option<&Value>, max: usize) -> Vec<usize> {
    match v {
        Some(Value::Sequence(s)) => s.iter().filter_map(|l| l.as_u64().map(|l| l as usize)).collect(),
        Some(Value::Mapping(m)) => {
            let from = m.get("from").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let to = m.get("to").and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(max - 1);
            (from..=to.min(max - 1)).collect()
        }
        _ => (0..max).collect(),
    }
}

fn refresh_before(v: Option<&Value>) -> Result<Option<SystemTime>> {
    let Some(v) = v else { return Ok(None) };
    let secs = |k: &str, f: f64| v.get(k).and_then(|x| x.as_f64().or(x.as_i64().map(|i| i as f64))).unwrap_or(0.0) * f;
    if v.get("time").is_some() || v.get("mtime").is_some() {
        bail!("refresh_before time/mtime not supported");
    }
    let delta = secs("weeks", 604800.0) + secs("days", 86400.0) + secs("hours", 3600.0) + secs("minutes", 60.0) + secs("seconds", 1.0);
    Ok(Some(SystemTime::now() - Duration::from_secs_f64(delta)))
}

/// Seed tasks named in `names` from a seed.yaml (one per cache × grid).
pub fn load_tasks(mgr: &TileManager, seed_conf: &Path, names: &[String]) -> Result<Vec<SeedTask>> {
    let v = load_yaml_with_bases(seed_conf)?;
    let dir = seed_conf.parent().unwrap_or(Path::new("."));
    let abspath = |p: &str| {
        let p = std::path::PathBuf::from(p);
        if p.is_absolute() { p } else { dir.join(p) }
    };
    let mut tasks = vec![];
    for name in names {
        let s = v
            .get("seeds")
            .and_then(|s| s.get(name.as_str()))
            .ok_or_else(|| anyhow!("unknown seed task {name}"))?;
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
                .ok_or_else(|| anyhow!("seed {name}: unknown coverage {cname}"))?;
            coverages.push(Arc::new(Coverage::from_conf(conf, &abspath).with_context(|| format!("coverage {cname}"))?));
        }
        if coverages.len() > 1 {
            bail!("seed {name}: multiple coverages are not supported");
        }
        let refresh = refresh_before(s.get("refresh_before"))?;
        for cache in &caches {
            let c = mgr.cfg.caches.get(cache).ok_or_else(|| anyhow!("seed {name}: unknown cache {cache}"))?;
            let grids: Vec<String> = match s.get("grids").and_then(|g| g.as_sequence()) {
                Some(g) => g.iter().filter_map(|g| g.as_str().map(String::from)).collect(),
                None => c.grids.clone(),
            };
            for g in grids {
                let cg = mgr.cache_grid(cache, &g).ok_or_else(|| anyhow!("seed {name}: cache {cache} has no grid {g}"))?;
                let levels = levels_of(s.get("levels"), cg.meta.grid.res.len());
                tasks.push(SeedTask {
                    name: name.clone(),
                    cg,
                    levels,
                    coverage: coverages.first().cloned(),
                    refresh_before: refresh,
                });
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

fn needs_work(task: &SeedTask, main: TileCoord) -> bool {
    let Some(before) = task.refresh_before else { return true };
    // handle_uncached with a refresh timestamp: any tile of the metatile missing or older.
    task.cg
        .meta
        .meta_tile(main)
        .tiles
        .iter()
        .filter_map(|t| t.0)
        .any(|t| task.cg.file.mtime(t).is_none_or(|m| m < before))
}

pub async fn run(mgr: Arc<TileManager>, tasks: Vec<SeedTask>, concurrency: usize) -> Result<()> {
    println!("========== Seeding tasks ==========");
    println!("Start seeding process ({} task{})", tasks.len(), if tasks.len() == 1 { "" } else { "s" });
    for task in tasks {
        let srs = &task.cg.meta.grid.srs;
        println!("  {}:", task.name);
        println!(
            "    Seeding cache '{}' with grid '{}' in {}",
            task.cg.cache, task.cg.grid, srs.code
        );
        if let Some(c) = &task.coverage {
            let wgs = crate::srs::Srs::new("EPSG:4326")?;
            let b = c.srs.transform_bbox(&wgs, c.bbox)?;
            println!("    Limited to coverage in: {:.5}, {:.5}, {:.5}, {:.5} (EPSG:4326)", b[0], b[1], b[2], b[3]);
        }
        println!("    Levels: {:?}", task.levels);
        match task.refresh_before {
            Some(t) => println!("    Overwriting: tiles older than {}", httpdate::fmt_http_date(t)),
            None => println!("    Overwriting: all tiles"),
        }
        let metas = plan(&task)?;
        let total = metas.len();
        let task = Arc::new(task);
        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut set = JoinSet::new();
        let (t0, mut last) = (Instant::now(), Instant::now());
        let (mut done, mut tiles, mut errors) = (0usize, 0usize, 0usize);
        let report = |z: usize, main: TileCoord, done: usize, tiles: usize, t: &SeedTask| {
            let b = t.cg.meta.meta_tile(main).bbox;
            println!(
                "[{}] {:2} {:6.2}% {:.5}, {:.5}, {:.5}, {:.5} ({} tiles)",
                clock(),
                z,
                if total == 0 { 100.0 } else { 100.0 * done as f64 / total as f64 },
                b[0],
                b[1],
                b[2],
                b[3],
                tiles
            );
        };
        for (z, main) in metas {
            let permit = sem.clone().acquire_owned().await?;
            let (m, t) = (mgr.clone(), task.clone());
            set.spawn(async move {
                let _p = permit;
                if !needs_work(&t, main) {
                    return (z, main, Ok(0));
                }
                let meta = t.cg.meta.meta_tile(main);
                let lock_cg = t.cg.clone();
                let r = async {
                    let _l = tokio::task::spawn_blocking(move || lock_cg.file.lock(main)).await??;
                    m.create_meta(&t.cg, &meta, true).await.map(|t| t.len())
                }
                .await;
                (z, main, r)
            });
            while let Some(r) = set.try_join_next() {
                let (z, main, r) = r?;
                done += 1;
                match r {
                    Ok(n) => tiles += n,
                    Err(e) => {
                        errors += 1;
                        eprintln!("error in metatile {main:?}: {e:#}");
                    }
                }
                if last.elapsed() > Duration::from_secs(30) {
                    report(z, main, done, tiles, &task);
                    last = Instant::now();
                }
            }
        }
        while let Some(r) = set.join_next().await {
            let (z, main, r) = r?;
            done += 1;
            match r {
                Ok(n) => tiles += n,
                Err(e) => {
                    errors += 1;
                    eprintln!("error in metatile {main:?}: {e:#}");
                }
            }
            if last.elapsed() > Duration::from_secs(30) || done == total {
                report(z, main, done, tiles, &task);
                last = Instant::now();
            }
        }
        println!(
            "    {} metatiles, {} tiles, {} errors in {:.1} s",
            total,
            tiles,
            errors,
            t0.elapsed().as_secs_f64()
        );
        if errors > 0 {
            bail!("{errors} metatiles failed");
        }
    }
    Ok(())
}

fn clock() -> String {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
    let s = now % 86400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}
