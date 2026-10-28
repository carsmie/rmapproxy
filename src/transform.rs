//! Source reprojection (`image/transform.py` `ImageTransformer._transform`): MapProxy's mesh
//! of quads (`transform_meshes`, max 1 px error) and Pillow 12.3's `Image.transform(MESH,
//! BILINEAR)` (libImaging/Geometry.c quad_transform + bilinear_filter32RGB), bit-exact.
//! RGBA goes through premultiplied RGBa like Pillow (Convert.c rgbA2rgba / rgba2rgbA).

use anyhow::Result;

use crate::image::Rgba;
use crate::srs::{BBox, Srs};

/// Destination box (x0, y0, x1, y1) and source quad NW, SW, SE, NE (x, y pairs).
pub type Mesh = ([i64; 4], [f64; 8]);

/// `make_lin_transf(src, dst)`: linear map between a cartesian bbox and image pixels
/// (y flipped), same float op order as Python.
fn lin(src: [f64; 4], dst: [f64; 4], p: (f64, f64)) -> (f64, f64) {
    (
        dst[0] + (p.0 - src[0]) * (dst[2] - dst[0]) / (src[2] - src[0]),
        dst[1] + (src[3] - p.1) * (dst[3] - dst[1]) / (src[3] - src[1]),
    )
}

/// `center_quad_transform`: where Pillow's affine quad map puts the quad's centre pixel.
fn center_quad(q: [i64; 4], s: &[f64; 8]) -> (f64, f64) {
    let (w, h) = ((q[2] - q[0]) as f64, (q[3] - q[1]) as f64);
    let (a, b) = quad_coefs(w, h, s);
    let (x, y) = (w / 2.0 - 0.5, h / 2.0 - 0.5);
    (a[0] + a[1] * x + a[2] * y + a[3] * x * y, b[0] + b[1] * x + b[2] * y + b[3] * x * y)
}

/// Pillow `__transformer` QUAD coefficients (x: a0..a3, y: a4..a7).
fn quad_coefs(w: f64, h: f64, s: &[f64; 8]) -> ([f64; 4], [f64; 4]) {
    let (nw, sw, se, ne) = ((s[0], s[1]), (s[2], s[3]), (s[4], s[5]), (s[6], s[7]));
    let (r#as, at) = (1.0 / w, 1.0 / h);
    (
        [nw.0, (ne.0 - nw.0) * r#as, (sw.0 - nw.0) * at, (se.0 - sw.0 - ne.0 + nw.0) * r#as * at],
        [nw.1, (ne.1 - nw.1) * r#as, (sw.1 - nw.1) * at, (se.1 - sw.1 - ne.1 + nw.1) * r#as * at],
    )
}

/// `divide_quad`: halves or quarters, splitting along the long side only if it's > 2x.
fn divide_quad(q: [i64; 4]) -> Vec<[i64; 4]> {
    let (w, h) = (q[2] - q[0], q[3] - q[1]);
    // int(q0 + w/2): float division, truncated (all positive).
    let xc = (q[0] as f64 + w as f64 / 2.0) as i64;
    let yc = (q[1] as f64 + h as f64 / 2.0) as i64;
    if w > 2 * h {
        return vec![[q[0], q[1], xc, q[3]], [xc, q[1], q[2], q[3]]];
    }
    if h > 2 * w {
        return vec![[q[0], q[1], q[2], yc], [q[0], yc, q[2], q[3]]];
    }
    vec![[q[0], q[1], xc, yc], [xc, q[1], q[2], yc], [q[0], yc, xc, q[3]], [xc, yc, q[2], q[3]]]
}

/// `transform_meshes(..., max_px_err=1, use_center_px=False)`.
pub fn transform_meshes(
    src_size: (u32, u32),
    src_bbox: BBox,
    src_srs: &Srs,
    dst_size: (u32, u32),
    dst_bbox: BBox,
    dst_srs: &Srs,
) -> Result<Vec<Mesh>> {
    let src_rect = [0.0, 0.0, src_size.0 as f64, src_size.1 as f64];
    let dst_rect = [0.0, 0.0, dst_size.0 as f64, dst_size.1 as f64];
    let res = (dst_bbox[2] - dst_bbox[0]) / dst_size.0 as f64;
    let max_err = res;
    let to_src = |q: [i64; 4]| -> Result<[f64; 8]> {
        let mut s = [0.0; 8];
        for (i, (x, y)) in [(q[0], q[1]), (q[0], q[3]), (q[2], q[3]), (q[2], q[1])].into_iter().enumerate() {
            let w = lin(dst_rect, dst_bbox, (x as f64, y as f64));
            let w = dst_srs.transform_point(src_srs, w.0, w.1)?;
            let p = lin(src_bbox, src_rect, w);
            (s[2 * i], s[2 * i + 1]) = p;
        }
        Ok(s)
    };
    let is_good = |q: [i64; 4], s: &[f64; 8]| -> Result<bool> {
        let (w, h) = (q[2] - q[0], q[3] - q[1]);
        if w < 50 || h < 50 {
            return Ok(true);
        }
        let xc = q[0] as f64 + w as f64 / 2.0 - 0.5;
        let yc = q[1] as f64 + h as f64 / 2.0 - 0.5;
        let dst_w = lin(dst_rect, dst_bbox, (xc, yc));
        let src_w = lin(src_rect, src_bbox, center_quad(q, s));
        let real = src_srs.transform_point(dst_srs, src_w.0, src_w.1)?;
        Ok((dst_w.0 - real.0).abs().max((dst_w.1 - real.1).abs()) < max_err)
    };
    // add_meshes: depth first, in divide_quad order.
    fn add(
        q: [i64; 4],
        out: &mut Vec<Mesh>,
        to_src: &QuadToSrc,
        is_good: &QuadIsGood,
    ) -> Result<()> {
        let s = to_src(q)?;
        if is_good(q, &s)? {
            out.push((q, s));
        } else {
            for sub in divide_quad(q) {
                add(sub, out, to_src, is_good)?;
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    add([0, 0, dst_size.0 as i64, dst_size.1 as i64], &mut out, &to_src, &is_good)?;
    Ok(out)
}

/// MULDIV255 (libImaging/ImagingUtils.h).
fn muldiv255(a: u32, b: u32) -> u8 {
    let t = a * b + 128;
    (((t >> 8) + t) >> 8) as u8
}

/// A destination quad's corners in source pixels (`add_meshes`).
type QuadToSrc<'a> = dyn Fn([i64; 4]) -> Result<[f64; 8]> + 'a;
/// Whether a quad and its source corners are fine enough to warp as one mesh.
type QuadIsGood<'a> = dyn Fn([i64; 4], &[f64; 8]) -> Result<bool> + 'a;

fn premultiply(img: &mut Rgba) {
    for p in img.px.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        for c in &mut p[..3] {
            *c = muldiv255(*c as u32, a);
        }
    }
}

fn unpremultiply(img: &mut Rgba) {
    for p in img.px.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((255 * *c as u32) / a).min(255) as u8;
            }
        }
    }
}

/// Pillow `img.transform(dst_size, MESH, meshes, BILINEAR)`. `alpha`: the image is RGBA
/// (warped premultiplied); otherwise it's opaque RGB/L (alpha stays 255, fill is black).
pub fn mesh_warp(src: &Rgba, alpha: bool, dst_size: (u32, u32), meshes: &[Mesh]) -> Rgba {
    let boxes: Vec<_> = meshes
        .iter()
        .map(|(b, s)| {
            let (a, c) = quad_coefs((b[2] - b[0]) as f64, (b[3] - b[1]) as f64, s);
            (*b, Map::Quad(a, c))
        })
        .collect();
    warp(src, alpha, dst_size, &boxes)
}

/// Pillow `img.transform(dst_size, EXTENT, (x0, y0, x1, y1), BILINEAR)` (`_transform_simple`):
/// the EXTENT -> AFFINE coefficients of `__transformer`.
pub fn extent_warp(src: &Rgba, alpha: bool, dst_size: (u32, u32), e: [f64; 4]) -> Rgba {
    let xs = (e[2] - e[0]) / dst_size.0 as f64;
    let ys = (e[3] - e[1]) / dst_size.1 as f64;
    warp(src, alpha, dst_size, &[([0, 0, dst_size.0 as i64, dst_size.1 as i64], Map::Affine([xs, 0.0, e[0], 0.0, ys, e[1]]))])
}

enum Map {
    /// quad_transform x (a0..a3) and y (a4..a7) coefficients.
    Quad([f64; 4], [f64; 4]),
    /// affine_transform a0..a5.
    Affine([f64; 6]),
}

/// ImagingGenericTransform with the bilinear filter over boxes of the output image.
fn warp(src: &Rgba, alpha: bool, dst_size: (u32, u32), boxes: &[([i64; 4], Map)]) -> Rgba {
    let mut src_p;
    let src = if alpha {
        src_p = src.clone();
        premultiply(&mut src_p);
        &src_p
    } else {
        src
    };
    let fill: [u8; 4] = if alpha { [0, 0, 0, 0] } else { [0, 0, 0, 255] };
    let mut out = Rgba::filled(dst_size.0, dst_size.1, fill);
    let (sw, sh) = (src.w as i64, src.h as i64);
    let row = |y: i64| &src.px[(y * sw * 4) as usize..((y + 1) * sw * 4) as usize];
    let xclip = |x: i64| x.clamp(0, sw - 1);
    for (b, m) in boxes {
        let (x0, y0) = (b[0].max(0), b[1].max(0));
        let (x1, y1) = (b[2].min(dst_size.0 as i64), b[3].min(dst_size.1 as i64));
        for y in y0..y1 {
            for x in x0..x1 {
                // Box-relative pixel centre.
                let xin = (x - x0) as f64 + 0.5;
                let yin = (y - y0) as f64 + 0.5;
                let (mut xs, mut ys) = match m {
                    Map::Quad(a, c) => (a[0] + a[1] * xin + a[2] * yin + a[3] * xin * yin, c[0] + c[1] * xin + c[2] * yin + c[3] * xin * yin),
                    Map::Affine(a) => (a[0] * xin + a[1] * yin + a[2], a[3] * xin + a[4] * yin + a[5]),
                };
                let o = ((y * dst_size.0 as i64 + x) * 4) as usize;
                // BILINEAR_HEAD: out of the source -> fill.
                if xs < 0.0 || xs >= sw as f64 || ys < 0.0 || ys >= sh as f64 {
                    out.px[o..o + 4].copy_from_slice(&fill);
                    continue;
                }
                xs -= 0.5;
                ys -= 0.5;
                // FLOOR: (int) truncation, floor() below zero.
                let xi = if xs < 0.0 { xs.floor() as i64 } else { xs as i64 };
                let yi = if ys < 0.0 { ys.floor() as i64 } else { ys as i64 };
                let (dx, dy) = (xs - xi as f64, ys - yi as f64);
                let r0 = row(yi.clamp(0, sh - 1));
                let r1 = (yi + 1 >= 0 && yi + 1 < sh).then(|| row(yi + 1));
                let (i0, i1) = ((xclip(xi) * 4) as usize, (xclip(xi + 1) * 4) as usize);
                for k in 0..4 {
                    let lerp = |r: &[u8]| r[i0 + k] as f64 + (r[i1 + k] as i32 - r[i0 + k] as i32) as f64 * dx;
                    let v1 = lerp(r0);
                    let v2 = r1.map_or(v1, lerp);
                    out.px[o + k] = (v1 + (v2 - v1) * dy) as u8;
                }
            }
        }
    }
    if alpha {
        unpremultiply(&mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divide_quad_matches_mapproxy_doctest() {
        assert_eq!(divide_quad([0, 0, 500, 500]), vec![[0, 0, 250, 250], [250, 0, 500, 250], [0, 250, 250, 500], [250, 250, 500, 500]]);
        assert_eq!(divide_quad([0, 0, 2000, 500]), vec![[0, 0, 1000, 500], [1000, 0, 2000, 500]]);
        assert_eq!(divide_quad([100, 200, 200, 500]), vec![[100, 200, 200, 350], [100, 350, 200, 500]]);
    }

    #[test]
    fn premultiply_roundtrip_like_pillow() {
        let mut i = Rgba { w: 1, h: 1, px: vec![200, 100, 7, 128] };
        premultiply(&mut i);
        assert_eq!(i.px, vec![100, 50, 4, 128]);
        unpremultiply(&mut i);
        assert_eq!(i.px, vec![199, 99, 7, 128]);
    }
}
