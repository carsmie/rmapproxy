//! Tile and metatile geometry, as `grid/tile_grid.py` and `grid/meta_grid.py`. Every
//! rounding step here decides the BBOX/size of source requests, so it follows MapProxy
//! literally.

use crate::pyfloat::{floordiv, int, round_int, round_nd};
use crate::srs::{BBox, Srs};

pub type TileCoord = (i64, i64, usize);

#[derive(Debug, Clone)]
pub struct TileGrid {
    pub name: String,
    pub srs: Srs,
    pub bbox: BBox,
    pub tile_size: (u32, u32),
    pub res: Vec<f64>,
    pub grid_sizes: Vec<(i64, i64)>,
    /// origin nw/ul: y counts down from the top (MapProxy `flipped_y_axis`).
    pub flipped: bool,
}

impl TileGrid {
    pub fn new(name: &str, srs: Srs, bbox: BBox, tile_size: (u32, u32), res: Vec<f64>, flipped: bool) -> Self {
        let (w, h) = (bbox[2] - bbox[0], bbox[3] - bbox[1]);
        let grid_sizes = res
            .iter()
            .map(|&r| {
                let x = (floordiv(w, r) / tile_size.0 as f64).ceil().max(1.0) as i64;
                let y = (floordiv(h, r) / tile_size.1 as f64).ceil().max(1.0) as i64;
                (x, y)
            })
            .collect();
        TileGrid { name: name.into(), srs, bbox, tile_size, res, grid_sizes, flipped }
    }

    pub fn tile_bbox(&self, (x, y, z): TileCoord) -> BBox {
        let res = self.res[z];
        let (tw, th) = (self.tile_size.0 as f64, self.tile_size.1 as f64);
        let x0 = self.bbox[0] + round_nd(x as f64 * res * tw, 12);
        let x1 = x0 + round_nd(res * tw, 12);
        if self.flipped {
            let y1 = self.bbox[3] - round_nd(y as f64 * res * th, 12);
            [x0, y1 - round_nd(res * th, 12), x1, y1]
        } else {
            let y0 = self.bbox[1] + round_nd(y as f64 * res * th, 12);
            [x0, y0, x1, y0 + round_nd(res * th, 12)]
        }
    }

    /// `_tiles_bbox([first, last])` = merge_bbox of both tile bboxes.
    pub fn tiles_bbox(&self, a: TileCoord, b: TileCoord) -> BBox {
        let (p, q) = (self.tile_bbox(a), self.tile_bbox(b));
        [p[0].min(q[0]), p[1].min(q[1]), p[2].max(q[2]), p[3].max(q[3])]
    }

    pub fn contains(&self, (x, y, z): TileCoord) -> bool {
        z < self.res.len() && x >= 0 && y >= 0 && x < self.grid_sizes[z].0 && y < self.grid_sizes[z].1
    }

    /// `tile_coord_for_point` (only used for seeding ranges).
    pub fn tile_coord_for_point(&self, px: f64, py: f64, z: usize) -> TileCoord {
        let res = self.res[z];
        let x = px - self.bbox[0];
        let y = if self.flipped { self.bbox[3] - py } else { py - self.bbox[1] };
        let tx = x / (res * self.tile_size.0 as f64);
        let ty = y / (res * self.tile_size.1 as f64);
        (tx.floor() as i64, ty.floor() as i64, z)
    }
}

#[derive(Debug, Clone)]
pub struct MetaTile {
    pub bbox: BBox,
    pub size: (u32, u32),
    /// Tiles in row-major order with their top-left pixel in the metatile image; `None`
    /// for positions outside the grid.
    pub tiles: Vec<(Option<TileCoord>, (u32, u32))>,
}

impl MetaTile {
    /// The tile used for locking (`main_tile_coord`).
    pub fn main_tile(&self) -> Option<TileCoord> {
        self.tiles.iter().find_map(|t| t.0)
    }
}

#[derive(Debug, Clone)]
pub struct MetaGrid {
    pub grid: TileGrid,
    pub meta_size: (i64, i64),
    pub meta_buffer: i64,
}

impl MetaGrid {
    pub fn meta_size(&self, z: usize) -> (i64, i64) {
        let gs = self.grid.grid_sizes[z];
        (self.meta_size.0.min(gs.0), self.meta_size.1.min(gs.1))
    }

    pub fn main_tile(&self, (x, y, z): TileCoord) -> TileCoord {
        let ms = self.meta_size(z);
        (x.div_euclid(ms.0) * ms.0, y.div_euclid(ms.1) * ms.1, z)
    }

    fn buffered_bbox(&self, bbox: BBox, z: usize) -> (BBox, [i64; 4]) {
        let [mut minx, mut miny, mut maxx, mut maxy] = bbox;
        let mut buffers = [0i64; 4];
        if self.meta_buffer > 0 {
            let res = self.grid.res[z];
            let b = self.meta_buffer as f64 * res;
            minx -= b;
            miny -= b;
            maxx += b;
            maxy += b;
            buffers = [self.meta_buffer; 4];
            let g = self.grid.bbox;
            if g[0] > minx {
                buffers[0] -= int(round_nd((g[0] - minx) / res, 5));
                minx = g[0];
            }
            if g[1] > miny {
                buffers[1] -= int(round_nd((g[1] - miny) / res, 5));
                miny = g[1];
            }
            if g[2] < maxx {
                buffers[2] -= int(round_nd((maxx - g[2]) / res, 5));
                maxx = g[2];
            }
            if g[3] < maxy {
                buffers[3] -= int(round_nd((maxy - g[3]) / res, 5));
                maxy = g[3];
            }
        }
        ([minx, miny, maxx, maxy], buffers)
    }

    /// `get_affected_level_tiles(bbox, level)`: metatile main coords touching `bbox`
    /// (shrunk by res/10), row-wise from the top for flipped grids, plus the grid shape.
    pub fn affected_level_tiles(&self, bbox: BBox, z: usize) -> ((i64, i64), Vec<Option<TileCoord>>) {
        let delta = self.grid.res[z] / 10.0;
        let (x0, y0, _) = self.grid.tile_coord_for_point(bbox[0] + delta, bbox[1] + delta, z);
        let (x1, y1, _) = self.grid.tile_coord_for_point(bbox[2] - delta, bbox[3] - delta, z);
        let ms = self.meta_size(z);
        let (x0, x1) = (x0.div_euclid(ms.0) * ms.0, x1.div_euclid(ms.0) * ms.0);
        let (y0, y1) = (y0.div_euclid(ms.1) * ms.1, y1.div_euclid(ms.1) * ms.1);
        let xs: Vec<i64> = (x0..=x1).step_by(ms.0 as usize).collect();
        let ys: Vec<i64> = if self.grid.flipped {
            (y1.min(y0)..=y1.max(y0)).step_by(ms.1 as usize).collect()
        } else {
            let mut v: Vec<i64> = (y0..=y1).step_by(ms.1 as usize).collect();
            v.reverse();
            v
        };
        let gs = self.grid.grid_sizes[z];
        let mut tiles = Vec::with_capacity(xs.len() * ys.len());
        for &y in &ys {
            for &x in &xs {
                tiles.push((x >= 0 && y >= 0 && x < gs.0 && y < gs.1).then_some((x, y, z)));
            }
        }
        ((xs.len() as i64, ys.len() as i64), tiles)
    }

    /// `MetaGrid.meta_tile(tile_coord)`.
    pub fn meta_tile(&self, coord: TileCoord) -> MetaTile {
        let (x0, y0, z) = self.main_tile(coord);
        let ms = self.meta_size(z);
        let unbuffered = self.grid.tiles_bbox((x0, y0, z), (x0 + ms.0 - 1, y0 + ms.1 - 1, z));
        let (bbox, buffers) = self.buffered_bbox(unbuffered, z);
        let res = self.grid.res[z];
        let size = (
            round_int((bbox[2] - bbox[0]) / res) as u32,
            round_int((bbox[3] - bbox[1]) / res) as u32,
        );
        let ys: Vec<i64> = if self.grid.flipped {
            (y0..y0 + ms.1).collect()
        } else {
            (y0..y0 + ms.1).rev().collect()
        };
        let gs = self.grid.grid_sizes[z];
        let (tw, th) = (self.grid.tile_size.0 as i64, self.grid.tile_size.1 as i64);
        let mut tiles = Vec::with_capacity((ms.0 * ms.1) as usize);
        for (i, &y) in ys.iter().enumerate() {
            for j in 0..ms.0 {
                let x = x0 + j;
                let c = (x >= 0 && y >= 0 && x < gs.0 && y < gs.1).then_some((x, y, z));
                let off = (j * tw + buffers[0], i as i64 * th + buffers[3]);
                tiles.push((c, (off.0 as u32, off.1 as u32)));
            }
        }
        MetaTile { bbox, size, tiles }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn norway() -> TileGrid {
        let res = (0..19).map(|i| 21664.0 / 2f64.powi(i)).collect();
        TileGrid::new(
            "norway_25833",
            Srs::new("EPSG:25833").unwrap(),
            [-2500000.0, 3500000.0, 3045984.0, 9045984.0],
            (256, 256),
            res,
            true,
        )
    }

    #[test]
    fn grid_sizes_and_bbox() {
        let g = norway();
        assert_eq!(g.grid_sizes[0], (1, 1));
        assert_eq!(g.grid_sizes[4], (16, 16));
        // z04 r4 c6 from the GEBCO check: x0 = -2.5e6 + 6 * 346624, y1 = top - 4 * 346624.
        let b = g.tile_bbox((6, 4, 4));
        assert_eq!(b, [-420256.0, 7312864.0, -73632.0, 7659488.0]);
    }

    #[test]
    fn metatile_buffer_clamped_at_grid_edge() {
        let mg = MetaGrid { grid: norway(), meta_size: (4, 4), meta_buffer: 100 };
        let m = mg.meta_tile((1, 2, 3));
        // main tile (0,0,3): left/top buffers fall outside the grid bbox -> 0.
        assert_eq!(m.tiles[0], (Some((0, 0, 3)), (0, 0)));
        assert_eq!(m.size, (4 * 256 + 100, 4 * 256 + 100));
        assert_eq!(m.bbox[0], -2500000.0);
        assert_eq!(m.bbox[3], 9045984.0);
    }
}
