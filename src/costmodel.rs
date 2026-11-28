//! Seed duration estimate learned from finished distributed jobs (`.seed_history.json` entries
//! with `levels` + `wall_seconds`). A single tiles/s average mixes products (a 2x metatile has 4
//! tiles, a 1x one 16), zoom levels (a low-zoom metatile covers far more ground and can cost
//! several times more) and jobs that hit few backends; this learns wall time per metatile for each
//! (cache, level) as it was actually achieved, including the parallelism that work reached.

use std::collections::HashMap;

use serde_json::Value as Json;

/// History jobs used (newest first).
const MAX_JOBS: usize = 30;
/// A (cache, level) needs at least this many metatiles of history to count as known.
const MIN_METATILES: u64 = 100;

#[derive(Default, Debug)]
pub struct Model {
    /// (cache, level) → (wall seconds attributed, metatiles).
    per: HashMap<(String, u32), (f64, u64)>,
    pub jobs: usize,
    ramp_secs: f64,
}

#[derive(Debug, PartialEq)]
pub struct Prediction {
    pub seconds: f64,
    pub low: f64,
    pub high: f64,
    /// Metatiles priced from their own (cache, level) history / from a neighbour or average.
    pub known_metatiles: u64,
    pub assumed_metatiles: u64,
}

impl Model {
    /// `ramp_secs`: time before a job reaches full speed (backend/worker autoscaling); subtracted
    /// from history wall times when learning and added once to each prediction.
    pub fn learn(history: &[Json], ramp_secs: f64) -> Model {
        let mut m = Model { ramp_secs, ..Default::default() };
        let jobs = history.iter().rev().filter(|e| e["mode"] == "seed" && e["exit_code"] == 0 && e["levels"].is_array());
        for e in jobs.take(MAX_JOBS) {
            let wall = e["wall_seconds"].as_f64().unwrap_or(0.0);
            let levels: Vec<(String, u32, u64, f64)> = e["levels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| Some((l["cache"].as_str()?.to_string(), l["z"].as_u64()? as u32, l["metatiles"].as_u64()?, l["worker_seconds"].as_f64()?)))
                .filter(|l| l.2 > 0 && l.3 > 0.0)
                .collect();
            let worker_secs: f64 = levels.iter().map(|l| l.3).sum();
            if wall <= 0.0 || worker_secs <= 0.0 {
                continue;
            }
            // Parallelism the job reached at full speed; ramp never takes more than half a job.
            let parallel = worker_secs / (wall - ramp_secs).max(wall / 2.0);
            for (cache, z, metas, secs) in levels {
                let e = m.per.entry((cache, z)).or_default();
                e.0 += secs / parallel;
                e.1 += metas;
            }
            m.jobs += 1;
        }
        m
    }

    /// Wall seconds per metatile for (cache, z) and whether it is the level's own history.
    fn cost(&self, cache: &str, z: u32) -> Option<(f64, bool)> {
        let known = |k: &(String, u32)| self.per.get(k).filter(|v| v.1 >= MIN_METATILES).map(|v| v.0 / v.1 as f64);
        if let Some(c) = known(&(cache.to_string(), z)) {
            return Some((c, true));
        }
        // Same cache, nearest level with history; then any cache at this level; then everything.
        let nearest = self
            .per
            .iter()
            .filter(|(k, v)| k.0 == cache && v.1 >= MIN_METATILES)
            .min_by_key(|(k, _)| k.1.abs_diff(z))
            .map(|(_, v)| v.0 / v.1 as f64);
        let same_level = || {
            let (s, n) = self.per.iter().filter(|(k, _)| k.1 == z).fold((0.0, 0), |a, (_, v)| (a.0 + v.0, a.1 + v.1));
            (n >= MIN_METATILES).then(|| s / n as f64)
        };
        let all = || {
            let (s, n) = self.per.values().fold((0.0, 0), |a, v| (a.0 + v.0, a.1 + v.1));
            (n >= MIN_METATILES).then(|| s / n as f64)
        };
        nearest.or_else(same_level).or_else(all).map(|c| (c, false))
    }

    /// Duration for `levels` = (cache, z, metatiles); None without any usable history. The range
    /// is wider the more metatiles are priced from other levels.
    pub fn predict(&self, levels: &[(String, u32, u64)]) -> Option<Prediction> {
        let (mut secs, mut known, mut assumed) = (0.0, 0u64, 0u64);
        for (cache, z, metas) in levels {
            let (c, own) = self.cost(cache, *z)?;
            secs += c * *metas as f64;
            if own { known += metas } else { assumed += metas }
        }
        let assumed_share = assumed as f64 / (known + assumed).max(1) as f64;
        let ramp = if known + assumed > 0 { self.ramp_secs } else { 0.0 };
        Some(Prediction {
            seconds: ramp + secs,
            low: ramp + secs * 0.8,
            high: ramp + secs * (1.4 + 1.1 * assumed_share),
            known_metatiles: known,
            assumed_metatiles: assumed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn job(wall: f64, levels: &[(&str, u32, u64, f64)]) -> Json {
        json!({ "mode": "seed", "exit_code": 0, "wall_seconds": wall,
            "levels": levels.iter().map(|(c, z, m, s)| json!({"cache": c, "grid": "g", "z": z, "metatiles": m, "worker_seconds": s})).collect::<Vec<_>>() })
    }

    /// The 2026-11-27 dev runs, shaped like history: a z13 2x job (179,795 metatiles in 55 min at
    /// 12 workers) and a slower low-zoom 2x job (~9.5 metatiles/s at ~5 workers).
    fn dev_history() -> Vec<Json> {
        vec![
            job(3300.0, &[("topo_cache_2x", 13, 179_795, 12.0 * (3300.0 - 600.0))]),
            job(1200.0, &[("topo_cache_2x", 9, 5_700, 5.0 * 600.0)]),
        ]
    }

    #[test]
    fn learns_per_level_wall_time_and_adds_ramp() {
        let m = Model::learn(&dev_history(), 600.0);
        assert_eq!(m.jobs, 2);
        // z13: 2700 s of full-speed wall for 179,795 metatiles; the same job again ≈ 55 min.
        let p = m.predict(&[("topo_cache_2x".into(), 13, 179_795)]).unwrap();
        assert!((p.seconds - 3300.0).abs() < 1.0, "{p:?}");
        assert_eq!((p.known_metatiles, p.assumed_metatiles), (179_795, 0));
        assert!(p.low < p.seconds && p.seconds < p.high);
        // Low zoom costs ~7x more wall time per metatile than z13 here (fewer workers, heavier).
        let z9 = m.cost("topo_cache_2x", 9).unwrap().0;
        let z13 = m.cost("topo_cache_2x", 13).unwrap().0;
        assert!(z9 / z13 > 5.0, "{z9} vs {z13}");
    }

    #[test]
    fn unknown_level_uses_nearest_and_widens_the_range() {
        let m = Model::learn(&dev_history(), 600.0);
        let p = m.predict(&[("topo_cache_2x".into(), 10, 10_000)]).unwrap();
        assert_eq!((p.known_metatiles, p.assumed_metatiles), (0, 10_000));
        let z9 = m.cost("topo_cache_2x", 9).unwrap().0;
        assert!((p.seconds - (600.0 + z9 * 10_000.0)).abs() < 1e-6);
        assert!(p.high - 600.0 > 2.4 * (p.seconds - 600.0));
        // Other cache, no history of its own: the level average, else everything.
        assert!(m.predict(&[("graa".into(), 13, 1000)]).unwrap().assumed_metatiles == 1000);
    }

    #[test]
    fn no_usable_history_gives_none() {
        assert!(Model::learn(&[], 0.0).predict(&[("c".into(), 1, 10)]).is_none());
        // Old entries without levels and failed jobs are ignored.
        let h = vec![json!({"mode": "seed", "exit_code": 0, "tiles": 1000}), { let mut j = job(100.0, &[("c", 1, 500, 50.0)]); j["exit_code"] = 1.into(); j }];
        assert_eq!(Model::learn(&h, 0.0).jobs, 0);
    }
}
