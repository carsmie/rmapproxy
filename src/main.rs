//! rMapproxy: the MapProxy features tnt-mapproxy uses, in Rust (see SCOPE.md).

mod cache;
mod config;
mod coverage;
mod grid;
mod image;
mod pyfloat;
mod seed;
mod server;
mod source;
mod srs;
mod tile;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};

const USAGE: &str = "\
rmapproxy check -f mapproxy.yaml                       load the configuration and print a summary
rmapproxy serve -f mapproxy.yaml [--bind ADDR]           WMTS on ADDR (default 0.0.0.0:8080)
rmapproxy seed  -f mapproxy.yaml -s seed.yaml --seed a,b [-c N]   mapproxy-seed --seed subset
                                                       (--continue, --progress-file, --log-config accepted, ignored)
rmapproxy tile  -f mapproxy.yaml --cache C --grid G Z/X/Y [-o out.png] [--no-store]
                                                       render (or load) one tile
";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "rmapproxy=info".into()))
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
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
            rt.block_on(server::serve(mgr, &bind))
        }
        Some("seed") => {
            let cfg = Arc::new(config::Config::load(&cfg_path)?);
            let mgr = Arc::new(tile::TileManager::new(cfg)?);
            let seed_conf = PathBuf::from(flag("-s").context("-s seed.yaml")?);
            let names: Vec<String> = flag("--seed").context("--seed")?.split(',').map(String::from).collect();
            let conc: usize = flag("-c").map(|c| c.parse()).transpose()?.unwrap_or(2);
            let tasks = seed::load_tasks(&mgr, &seed_conf, &names)?;
            rt.block_on(seed::run(mgr, tasks, conc))
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
                    let tiles = mgr.create_meta(&cg, &meta, false).await?;
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
