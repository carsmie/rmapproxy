//! rMapproxy: the MapProxy features tnt-mapproxy uses, in Rust (see SCOPE.md).

mod admin;
mod cache;
mod config;
mod coverage;
#[cfg(test)]
mod e2e_tests;
mod grid;
mod image;
mod pickle;
mod pyfloat;
mod quantize;
mod seed;
mod server;
mod source;
mod srs;
mod tile;
mod transform;
mod wms;
mod work;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};

const USAGE: &str = "\
rmapproxy check -f mapproxy.yaml                       load the configuration and print a summary
rmapproxy serve -f mapproxy.yaml [--bind ADDR]           WMTS on ADDR (default 0.0.0.0:8080)
                [--admin [--seed-conf seed.yaml] [--admin-assets DIR]]
                                                       plus the /_admin API and GUI (DIR, default ./admin)
rmapproxy seed  -f mapproxy.yaml -s seed.yaml (--seed a,b | --cleanup a,b) [-c N]
                [--progress-file F [--continue]] [--log-config X (ignored)]
                                                       mapproxy-seed subset; also runs as `mapproxy-seed`
rmapproxy seed-worker -f mapproxy.yaml --admin URL [--name NAME] [--bind ADDR]
                                                       render chunks of distributed admin jobs until SIGTERM
                                                       (/healthz on ADDR, default 0.0.0.0:8080)
rmapproxy tile  -f mapproxy.yaml --cache C --grid G Z/X/Y [-o out.png] [--no-store]
                                                       render (or load) one tile
";

/// Per command: flags that take a value, and switches.
fn known_flags(cmd: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match cmd {
        "check" => (&["-f"], &[]),
        "serve" => (&["-f", "--bind", "--seed-conf", "--admin-assets"], &["--admin"]),
        "seed" => (&["-f", "-s", "--seed", "--cleanup", "-c", "--progress-file", "--log-config"], &["--continue"]),
        "seed-worker" => (&["-f", "--admin", "--name", "--bind"], &[]),
        "tile" => (&["-f", "--cache", "--grid", "-o"], &["--no-store"]),
        _ => (&[], &[]),
    }
}

/// Flags are parsed loosely (`mapproxy-seed` callers may pass more than this subset), so a
/// typo would otherwise go unnoticed.
fn warn_unknown_flags(args: &[String]) {
    let Some(cmd) = args.first() else { return };
    let (values, switches) = known_flags(cmd);
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if values.contains(&a.as_str()) {
            it.next();
        } else if a.starts_with('-') && !switches.contains(&a.as_str()) {
            tracing::warn!("{cmd}: unknown option {a} ignored");
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "rmapproxy=info".into()))
        .init();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Installed as `mapproxy-seed` (what tnt-mapproxy's admin app and seed jobs run and
    // look for in /proc/<pid>/cmdline): act as `rmapproxy seed`.
    let argv0 = std::env::args().next().unwrap_or_default();
    if std::path::Path::new(&argv0).file_name().is_some_and(|n| n == "mapproxy-seed") {
        args.insert(0, "seed".into());
    }
    warn_unknown_flags(&args);
    let flag = |name: &str| args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone());
    let has = |name: &str| args.iter().any(|a| a == name);
    let cfg_path = PathBuf::from(flag("-f").unwrap_or_else(|| "mapproxy.yaml".into()));
    let rt = tokio::runtime::Runtime::new()?;
    match args.first().map(String::as_str) {
        Some("check") => {
            let cfg = config::Config::load(&cfg_path)?;
            println!(
                "grids {}, caches {}, sources {}, layers {}",
                cfg.grids.len(),
                cfg.caches.len(),
                cfg.sources.len(),
                cfg.layers.len()
            );
            for l in &cfg.layers {
                for c in &l.sources {
                    if let Some(c) = cfg.caches.get(c) {
                        for g in &c.grids {
                            println!("  {} -> {} [{}] {}", l.name, c.name, g, cfg.cache_dir(&c.name, g).display());
                        }
                    }
                }
            }
            Ok(())
        }
        Some("serve") => {
            let cfg = Arc::new(config::Config::load(&cfg_path)?);
            let mgr = Arc::new(tile::TileManager::new(cfg)?);
            let bind = flag("--bind").unwrap_or_else(|| "0.0.0.0:8080".into());
            // Absolute: seed children run in the admin's state directory.
            let admin = has("--admin")
                .then(|| -> Result<admin::AdminOpts> {
                    let dir = cfg_path.parent().unwrap_or(std::path::Path::new("."));
                    let seed_conf = flag("--seed-conf").map(PathBuf::from).unwrap_or_else(|| dir.join("seed.yaml"));
                    let assets = flag("--admin-assets").map(PathBuf::from).unwrap_or_else(|| dir.join("admin"));
                    Ok(admin::AdminOpts {
                        mapproxy_conf: std::path::absolute(&cfg_path)?,
                        seed_conf: std::path::absolute(seed_conf)?,
                        assets: std::path::absolute(assets)?,
                    })
                })
                .transpose()?;
            rt.block_on(server::serve(mgr, &bind, admin))
        }
        Some("seed") => {
            let cfg = Arc::new(config::Config::load(&cfg_path)?);
            let mgr = Arc::new(tile::TileManager::new(cfg)?);
            let seed_conf = PathBuf::from(flag("-s").context("-s seed.yaml")?);
            let (mode, names) = match (flag("--seed"), flag("--cleanup")) {
                (Some(n), None) => (seed::Mode::Seed, n),
                (None, Some(n)) => (seed::Mode::Cleanup, n),
                _ => bail!("give exactly one of --seed or --cleanup"),
            };
            let names: Vec<String> = names.split(',').map(String::from).collect();
            let conc: usize = flag("-c").map(|c| c.parse()).transpose()?.unwrap_or(2);
            let progress = flag("--progress-file").map(|p| seed::Progress::new(PathBuf::from(p), has("--continue")));
            let tasks = seed::load_tasks(&mgr, &seed_conf, &names, mode)?;
            rt.block_on(seed::run(mgr, tasks, mode, conc, progress))
        }
        Some("seed-worker") => {
            let cfg = Arc::new(config::Config::load(&cfg_path)?);
            let mgr = Arc::new(tile::TileManager::new(cfg)?);
            let admin_url = flag("--admin").context("--admin URL")?;
            let name = flag("--name").or_else(|| std::env::var("HOSTNAME").ok()).unwrap_or_else(|| "worker".into());
            let bind = flag("--bind").unwrap_or_else(|| "0.0.0.0:8080".into());
            rt.block_on(work::run_worker(mgr, admin_url, name, bind))
        }
        Some("tile") => {
            let cfg = Arc::new(config::Config::load(&cfg_path)?);
            let mgr = tile::TileManager::new(cfg)?;
            let (cache, grid) = (flag("--cache").context("--cache")?, flag("--grid").context("--grid")?);
            let zxy = args.iter().rev().find(|a| a.matches('/').count() == 2).context("Z/X/Y")?;
            let v: Vec<i64> = zxy.split('/').map(|s| s.parse()).collect::<Result<_, _>>()?;
            let cg = mgr.cache_grid(&cache, &grid).ok_or_else(|| anyhow!("no cache {cache} in grid {grid}"))?;
            let coord = (v[1], v[2], v[0] as usize);
            let data = rt.block_on(async {
                if has("--no-store") {
                    let meta = cg.meta.meta_tile(coord);
                    let (tiles, _) = mgr.create_meta(&cg, &meta, false).await?;
                    tiles.into_iter().find(|(c, _)| *c == coord).map(|(_, d)| d).ok_or_else(|| anyhow!("tile not in metatile"))
                } else {
                    Ok(mgr.tile(&cg, coord).await?.data)
                }
            })?;
            let out = flag("-o").unwrap_or_else(|| "tile.png".into());
            std::fs::write(&out, &data)?;
            println!("{out}: {} bytes", data.len());
            Ok(())
        }
        _ => {
            eprint!("{USAGE}");
            bail!("unknown command")
        }
    }
}
