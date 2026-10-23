//! MapProxy's file cache (`cache/file.py`, `cache/path.py`): `tc` layout, atomic writes,
//! single-colour hardlinks and per-metatile flock files named like MapProxy's, so both can
//! share one cache directory (and nginx serves either's tiles).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};

use crate::grid::TileCoord;

pub struct FileCache {
    pub dir: PathBuf,
    lock_dir: PathBuf,
    /// md5 of the cache dir string (`FileCache.lock_cache_id`).
    lock_id: String,
    lock_timeout: Duration,
    link_single_color: bool,
}

impl FileCache {
    pub fn new(dir: PathBuf, lock_dir: PathBuf, lock_timeout: f64, link_single_color: bool) -> FileCache {
        let lock_id = format!("{:x}", Md5::digest(dir.to_string_lossy().as_bytes()));
        FileCache { dir, lock_dir, lock_id, lock_timeout: Duration::from_secs_f64(lock_timeout), link_single_color }
    }

    /// `tile_location_tc`: `<dir>/zz/xxx/xxx/xxx/yyy/yyy/yyy.png`.
    pub fn tile_path(&self, (x, y, z): TileCoord) -> PathBuf {
        self.dir.join(format!(
            "{:02}/{:03}/{:03}/{:03}/{:03}/{:03}/{:03}.png",
            z,
            x / 1_000_000,
            (x / 1000) % 1000,
            x % 1000,
            y / 1_000_000,
            (y / 1000) % 1000,
            y % 1000
        ))
    }

    pub fn load(&self, coord: TileCoord) -> Option<Vec<u8>> {
        fs::read(self.tile_path(coord)).ok()
    }

    pub fn mtime(&self, coord: TileCoord) -> Option<std::time::SystemTime> {
        fs::metadata(self.tile_path(coord)).and_then(|m| m.modified()).ok()
    }

    /// `store_tile`: write the encoded tile, or hardlink it to
    /// `single_color_tiles/<rrggbbaa>.png` when single-colour linking is on.
    pub fn store(&self, coord: TileCoord, data: &[u8], single_color: Option<[u8; 4]>) -> Result<()> {
        let loc = self.tile_path(coord);
        ensure_parent(&loc)?;
        match single_color.filter(|_| self.link_single_color) {
            Some(c) => {
                let hex: String = c.iter().map(|v| format!("{v:02x}")).collect();
                let real = self.dir.join("single_color_tiles").join(format!("{hex}.png"));
                if !real.exists() {
                    ensure_parent(&real)?;
                    write_atomic(&real, data)?;
                }
                if loc.symlink_metadata().is_ok() {
                    let _ = fs::remove_file(&loc);
                }
                match fs::hard_link(&real, &loc) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                    Err(e) => Err(e).with_context(|| format!("linking {}", loc.display())),
                }
            }
            None => {
                if loc.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
                    let _ = fs::remove_file(&loc);
                }
                write_atomic(&loc, data)
            }
        }
    }

    /// Exclusive lock for a metatile (`TileCacheBase.lock`), blocking up to the lock timeout.
    pub fn lock(&self, main_tile: TileCoord) -> Result<TileLock> {
        let (x, y, z) = main_tile;
        let path = self.lock_dir.join(format!("{}-{x}-{y}-{z}.lck", self.lock_id));
        fs::create_dir_all(&self.lock_dir).ok();
        let deadline = Instant::now() + self.lock_timeout;
        loop {
            let f = OpenOptions::new().create(true).append(true).read(true).open(&path)
                .with_context(|| format!("opening lock {}", path.display()))?;
            // zc.lockfile: flock(LOCK_EX | LOCK_NB), pid written for debugging.
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                let _ = (&f).write_all(format!(" {}\n", std::process::id()).as_bytes());
                return Ok(TileLock { path, _file: f });
            }
            if Instant::now() >= deadline {
                bail!("lock timeout {}", path.display());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Held metatile lock; removed on drop (`REMOVE_ON_UNLOCK` on POSIX).
pub struct TileLock {
    path: PathBuf,
    _file: File,
}

impl Drop for TileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn ensure_parent(p: &Path) -> Result<()> {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    Ok(())
}

/// `util/fs.py::write_atomic`: random temp name in the same dir, then rename.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = PathBuf::from(format!("{}.tmp-{}", path.display(), rand::random_range(0..=99_999_999u32)));
    let res = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(data)?;
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res.with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tc_layout_and_lock_name() {
        let c = FileCache::new("/tmp/cache".into(), "/tmp/locks".into(), 1.0, true);
        assert_eq!(c.tile_path((3, 4, 2)), PathBuf::from("/tmp/cache/02/000/000/003/000/000/004.png"));
        assert_eq!(
            c.tile_path((1_234_567, 89, 17)),
            PathBuf::from("/tmp/cache/17/001/234/567/000/000/089.png")
        );
        // md5("/tmp/cache"), as hashlib computes it.
        assert_eq!(c.lock_id, "0b6b5180c13288c5671bddf969c8f8c6");
    }
}
