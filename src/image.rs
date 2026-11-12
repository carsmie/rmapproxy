//! RGBA images and MapProxy's Pillow operations: decoding, `alpha_composite`
//! (libImaging/AlphaComposite.c, integer-exact), paste, crop, single-colour test, PNG encoding.

use anyhow::{Result, anyhow, bail};

/// Straight (non-premultiplied) RGBA8, like Pillow's "RGBA" mode.
#[derive(Clone, Debug, PartialEq)]
pub struct Rgba {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

/// `create_image` for a transparent image: bgcolor #ffffff with alpha 0.
pub const BLANK: [u8; 4] = [255, 255, 255, 0];

impl Rgba {
    pub fn filled(w: u32, h: u32, c: [u8; 4]) -> Rgba {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            px.extend_from_slice(&c);
        }
        Rgba { w, h, px }
    }

    pub fn blank(w: u32, h: u32) -> Rgba {
        Rgba::filled(w, h, BLANK)
    }

    /// Pillow `Image.alpha_composite(self, src)` in place.
    pub fn alpha_composite(&mut self, src: &Rgba) {
        debug_assert_eq!((self.w, self.h), (src.w, src.h));
        for (d, s) in self.px.as_chunks_mut::<4>().0.iter_mut().zip(src.px.as_chunks::<4>().0) {
            composite_px(d, s);
        }
    }

    /// Pillow `paste(img, (x, y))` without mask: replaces pixels, clipped to the canvas.
    pub fn paste(&mut self, src: &Rgba, x: i64, y: i64) {
        for sy in 0..src.h as i64 {
            let dy = y + sy;
            if dy < 0 || dy >= self.h as i64 {
                continue;
            }
            let x0 = x.max(0);
            let x1 = (x + src.w as i64).min(self.w as i64);
            if x0 >= x1 {
                continue;
            }
            let n = ((x1 - x0) * 4) as usize;
            let s = ((sy * src.w as i64 + (x0 - x)) * 4) as usize;
            let d = ((dy * self.w as i64 + x0) * 4) as usize;
            self.px[d..d + n].copy_from_slice(&src.px[s..s + n]);
        }
    }

    /// Pillow `crop((x, y, x + w, y + h))`; areas outside the image are (0, 0, 0, 0).
    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Rgba {
        let mut out = Rgba::filled(w, h, [0, 0, 0, 0]);
        out.paste(self, -(x as i64), -(y as i64));
        out
    }

    /// `is_single_color_image`: the colour if every pixel is identical.
    pub fn single_color(&self) -> Option<[u8; 4]> {
        let first: [u8; 4] = self.px.get(0..4)?.try_into().ok()?;
        self.px.as_chunks::<4>().0.iter().all(|p| *p == first).then_some(first)
    }
}

#[inline]
fn shiftfordiv255(a: u32) -> u32 {
    ((a >> 8) + a) >> 8
}

/// `coef1` of AlphaComposite.c for every (src_a, dst_a): the per-pixel division as a table.
static COEF1: std::sync::LazyLock<Box<[u16; 65536]>> = std::sync::LazyLock::new(|| {
    let mut t = Box::new([0u16; 65536]);
    for sa in 1..256u32 {
        for da in 0..256u32 {
            let outa255 = sa * 255 + da * (255 - sa);
            t[((sa << 8) | da) as usize] = (sa * 255 * 255 * (1 << 7) / outa255) as u16;
        }
    }
    t
});

/// One pixel of libImaging/AlphaComposite.c (PRECISION_BITS = 7). An opaque source pixel
/// yields itself exactly (coef1 = 255 << 7, coef2 = 0), a transparent one leaves `d`.
#[inline]
fn composite_px(d: &mut [u8], s: &[u8]) {
    let sa = s[3] as u32;
    if sa == 0 {
        return;
    }
    if sa == 255 {
        d.copy_from_slice(s);
        return;
    }
    let da = d[3] as u32;
    let outa255 = sa * 255 + da * (255 - sa);
    let coef1 = COEF1[((sa << 8) | da) as usize] as u32;
    let coef2 = 255 * (1 << 7) - coef1;
    for c in 0..3 {
        let tmp = s[c] as u32 * coef1 + d[c] as u32 * coef2;
        d[c] = (shiftfordiv255(tmp + (0x80 << 7)) >> 7) as u8;
    }
    d[3] = shiftfordiv255(outa255 + 0x80) as u8;
}

/// How Pillow would merge this decoded image in `LayerMerger.merge`: RGBA/P images are
/// alpha-composited, other modes (RGB, L, LA) are pasted over the canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MergeMode {
    Composite,
    Paste,
}

pub fn decode_png(data: &[u8]) -> Result<(Rgba, MergeMode)> {
    decode_png_typed(data).map(|(img, mode, _)| (img, mode))
}

/// `decode_png` plus the PNG's colour type (Pillow mode before expansion).
pub fn decode_png_typed(data: &[u8]) -> Result<(Rgba, MergeMode, png::ColorType)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| anyhow!("png: {e}"))?;
    let info = reader.info();
    let (w, h) = (info.width, info.height);
    // Pillow mode before expansion: RGBA and P (with or without tRNS) composite, RGB/L with a tRNS
    // colour key convert to RGBA first ('transparency' in info), plain RGB/L/LA paste.
    let ctype = info.color_type;
    let mode = match info.color_type {
        png::ColorType::Rgba | png::ColorType::Indexed => MergeMode::Composite,
        _ if info.trns.is_some() => MergeMode::Composite,
        _ => MergeMode::Paste,
    };
    let mut buf = vec![0; reader.output_buffer_size().ok_or_else(|| anyhow!("png too large"))?];
    let frame = reader.next_frame(&mut buf).map_err(|e| anyhow!("png: {e}"))?;
    buf.truncate(frame.buffer_size());
    let n = (w * h) as usize;
    let mut px = Vec::with_capacity(n * 4);
    match frame.color_type {
        png::ColorType::Rgba => px = buf,
        png::ColorType::Rgb => {
            for p in buf.as_chunks::<3>().0 {
                px.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for &g in &buf {
                px.extend_from_slice(&[g, g, g, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for p in buf.as_chunks::<2>().0 {
                px.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
            }
        }
        png::ColorType::Indexed => bail!("png: palette not expanded"),
    }
    if px.len() != n * 4 {
        bail!("png: unexpected buffer size");
    }
    Ok((Rgba { w, h, px }, mode, ctype))
}

/// RGBA PNG (MapProxy's default for transparent png caches without `colors`).
pub fn encode_png_rgba(img: &Rgba, compression: png::Compression) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(img.px.len() / 4);
    {
        let mut enc = png::Encoder::new(&mut out, img.w, img.h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(compression);
        let mut w = enc.write_header()?;
        w.write_image_data(&img.px)?;
    }
    Ok(out)
}

/// Paletted PNG as Pillow's PngImagePlugin writes a P image with `transparency=255`:
/// bit depth from the palette size, PLTE = palette, tRNS = (ff * 255 + 00)[:colors],
/// indices packed to the bit depth (so index 255 keeps only its low bits).
pub fn encode_png_paletted(w: u32, h: u32, palette: &[[u8; 3]], idx: &[u8], transparency: bool, compression: png::Compression) -> Result<Vec<u8>> {
    let colors = palette.len().clamp(1, 256);
    let bits: u8 = match colors {
        0..=2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        _ => 8,
    };
    let mut plte: Vec<u8> = palette.iter().flatten().copied().collect();
    plte.resize(colors * 3, 0);
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(match bits {
            1 => png::BitDepth::One,
            2 => png::BitDepth::Two,
            4 => png::BitDepth::Four,
            _ => png::BitDepth::Eight,
        });
        enc.set_palette(plte);
        if transparency {
            let mut trns = vec![0xffu8; 255];
            trns.push(0);
            trns.truncate(colors);
            enc.set_trns(trns);
        }
        enc.set_compression(compression);
        let mut wr = enc.write_header()?;
        if bits == 8 {
            wr.write_image_data(idx)?;
        } else {
            let mask = (1u8 << bits) - 1;
            let per = 8 / bits as usize;
            let row_bytes = (w as usize).div_ceil(per);
            let mut packed = vec![0u8; row_bytes * h as usize];
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let v = idx[y * w as usize + x] & mask;
                    let shift = 8 - bits as usize * (x % per + 1);
                    packed[y * row_bytes + x / per] |= v << shift;
                }
            }
            wr.write_image_data(&packed)?;
        }
    }
    Ok(out)
}

/// MapProxy `quantize(img, colors, alpha=True, quantizer='mediancut')`: median cut of the
/// RGB channels to colors-1 entries, then index 255 where alpha <= 128.
pub fn quantize_mediancut_alpha(img: &Rgba, colors: u32) -> (Vec<[u8; 3]>, Vec<u8>) {
    let rgb = rgb_of(img);
    let (pal, idx) = crate::quantize::mediancut(&rgb, colors - 1);
    apply_alpha(img, pal, idx)
}

/// Spot colours: bins (8 levels/channel) with at least this many opaque pixels...
const SPOT_MIN_PX: u32 = 24;
/// ...whose pixels sit on average further than this (RGB distance) from their entry...
const SPOT_MAX_ERR: u32 = 16;
/// ...get an entry for the bin's most common exact colour. Slots taken from median cut at most:
const SPOT_MAX: usize = 32;

/// `quantizer: mediancut_spot` (rMapproxy only): median cut plus reserved entries for small flat
/// symbol colours. Median cut splits by population, so a shield or road core of a few hundred px
/// in a 512 px tile drops out of the palette and maps far off (E6 shield 84,140,56 → 95,90,81 grey,
/// motorway core 255,255,0 → 248,243,134; Svinesund 2x tiles 2026-11-11). Bins, not exact colours:
/// thin lines are mostly anti-aliased shades. No such bins → output identical to `mediancut`.
pub fn quantize_mediancut_spot_alpha(img: &Rgba, colors: u32) -> (Vec<[u8; 3]>, Vec<u8>) {
    use std::collections::HashMap;
    let rgb = rgb_of(img);
    let (pal, idx) = crate::quantize::mediancut(&rgb, colors - 1);
    let opaque = |i: usize| img.px[i * 4 + 3] > 128;
    // Exact-colour histogram of opaque pixels, with median cut's entry for that colour.
    let mut hist: HashMap<[u8; 3], (u32, u8)> = HashMap::new();
    for (i, &c) in rgb.iter().enumerate() {
        if opaque(i) {
            hist.entry(c).or_insert((0, idx[i])).0 += 1;
        }
    }
    // Per bin: pixels, summed squared error, most common exact colour (count, colour).
    type Bin = (u32, u64, (u32, [u8; 3]));
    let mut bins: HashMap<[u8; 3], Bin> = HashMap::new();
    for (&c, &(n, e)) in &hist {
        let b = bins.entry([c[0] >> 5, c[1] >> 5, c[2] >> 5]).or_insert((0, 0, (0, c)));
        b.0 += n;
        b.1 += n as u64 * dist2(c, pal[e as usize]) as u64;
        if (n, c) > b.2 {
            b.2 = (n, c);
        }
    }
    let lim = (SPOT_MAX_ERR * SPOT_MAX_ERR) as u64;
    let mut cand: Vec<([u8; 3], u64)> =
        bins.values().filter(|(n, se, _)| *n >= SPOT_MIN_PX && *se > lim * *n as u64).map(|(_, se, (_, c))| (*c, *se)).collect();
    if cand.is_empty() {
        return apply_alpha(img, pal, idx);
    }
    // Most total error first; a near-duplicate of a chosen spot is covered by it.
    cand.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let far = |a: [u8; 3], b: [u8; 3]| dist2(a, b) > SPOT_MAX_ERR * SPOT_MAX_ERR;
    let mut spots: Vec<[u8; 3]> = Vec::new();
    for (c, _) in cand {
        if spots.len() == SPOT_MAX {
            break;
        }
        if spots.iter().all(|s| far(*s, c)) {
            spots.push(c);
        }
    }
    // Rerun with the slots left; a pixel moves to a spot only if it is closer than its entry.
    let (mut pal, mut idx) = crate::quantize::mediancut(&rgb, colors - 1 - spots.len() as u32);
    let base = pal.len();
    pal.extend_from_slice(&spots);
    let mut memo: HashMap<([u8; 3], u8), u8> = HashMap::new();
    for (i, &c) in rgb.iter().enumerate() {
        let cur = idx[i];
        idx[i] = *memo.entry((c, cur)).or_insert_with(|| {
            let (mut best, mut bd) = (cur, dist2(c, pal[cur as usize]));
            for (j, s) in spots.iter().enumerate() {
                let d = dist2(c, *s);
                if d < bd {
                    (best, bd) = ((base + j) as u8, d);
                }
            }
            best
        });
    }
    apply_alpha(img, pal, idx)
}

fn rgb_of(img: &Rgba) -> Vec<[u8; 3]> {
    img.px.as_chunks::<4>().0.iter().map(|p| [p[0], p[1], p[2]]).collect()
}

#[inline]
fn dist2(a: [u8; 3], b: [u8; 3]) -> u32 {
    let d = |i: usize| a[i] as i32 - b[i] as i32;
    (d(0) * d(0) + d(1) * d(1) + d(2) * d(2)) as u32
}

/// MapProxy's alpha=True step: index 255 where alpha <= 128.
fn apply_alpha(img: &Rgba, mut pal: Vec<[u8; 3]>, mut idx: Vec<u8>) -> (Vec<[u8; 3]>, Vec<u8>) {
    let mut any_transparent = false;
    for (i, p) in img.px.as_chunks::<4>().0.iter().enumerate() {
        if p[3] <= 128 {
            idx[i] = 255;
            any_transparent = true;
        }
    }
    // Index 255 must exist in PLTE/tRNS: mediancut yields ≤255 entries, so without padding the
    // transparent index fell outside the palette and decoded as opaque black (2x tiles, 2026-11-10).
    if any_transparent {
        pal.resize(256, [BLANK[0], BLANK[1], BLANK[2]]);
    }
    (pal, idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture from Pillow: src = every (src_a, dst_a) pair with varying colours,
    /// expected = Image.alpha_composite(dst, src) (see tests/fixtures/README).
    #[test]
    fn alpha_composite_matches_pillow() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let load = |n: &str| decode_png(&std::fs::read(format!("{dir}/{n}")).unwrap()).unwrap().0;
        let (mut dst, src, want) = (load("composite_dst.png"), load("composite_src.png"), load("composite_out.png"));
        dst.alpha_composite(&src);
        let bad = dst.px.chunks(4).zip(want.px.chunks(4)).filter(|(a, b)| a != b).count();
        assert_eq!(bad, 0, "{bad} pixels differ from Pillow");
    }

    /// Transparent pixels (index 255) must decode transparent, not as out-of-palette black.
    #[test]
    fn mediancut_alpha_transparent_roundtrips() {
        let mut img = Rgba::filled(4, 4, [0, 0, 0, 0]);
        for (i, p) in img.px.chunks_mut(4).enumerate().take(8) {
            p.copy_from_slice(&[10 * i as u8, 100, 200, 255]);
        }
        let (pal, idx) = quantize_mediancut_alpha(&img, 256);
        assert_eq!(pal.len(), 256);
        let png = encode_png_paletted(4, 4, &pal, &idx, true, png::Compression::Fast).unwrap();
        let (out, _) = decode_png(&png).unwrap();
        assert!(out.px.chunks(4).skip(8).all(|p| p[3] == 0), "transparent half not transparent");
        assert!(out.px.chunks(4).take(8).all(|p| p[3] == 255));
    }

    /// 512² of smooth 3-channel noise-free gradient (fills median cut's boxes) + a 24×20 shield.
    fn gradient_with_shield(shield: [u8; 3]) -> Rgba {
        let mut img = Rgba::filled(512, 512, [0, 0, 0, 255]);
        for (i, p) in img.px.chunks_mut(4).enumerate() {
            let (x, y) = ((i % 512) as u32, (i / 512) as u32);
            p[..3].copy_from_slice(&[(150 + x / 8) as u8, (180 + (x + y) / 32) as u8, (90 + y / 6) as u8]);
            if (200..224).contains(&x) && (300..320).contains(&y) {
                p[..3].copy_from_slice(&shield);
            }
        }
        img
    }

    fn decoded_px(pal: &[[u8; 3]], idx: &[u8], i: usize) -> [u8; 3] {
        pal[idx[i] as usize]
    }

    #[test]
    fn spot_keeps_small_flat_symbol_colour() {
        let green = [84, 140, 56];
        let img = gradient_with_shield(green);
        let at = 310 * 512 + 210;
        let (pal, idx) = quantize_mediancut_alpha(&img, 256);
        assert!(dist2(decoded_px(&pal, &idx, at), green) > 16 * 16, "fixture must reproduce the bug");
        let (pal, idx) = quantize_mediancut_spot_alpha(&img, 256);
        assert_eq!(decoded_px(&pal, &idx, at), green);
        assert!(pal.len() <= 255 && idx.iter().all(|&i| (i as usize) < pal.len()));
    }

    #[test]
    fn spot_without_spots_equals_mediancut() {
        let mut img = Rgba::filled(64, 64, [0, 0, 0, 255]);
        for (i, p) in img.px.chunks_mut(4).enumerate() {
            p[..3].copy_from_slice(&[(i % 64 * 4) as u8, (i / 64 * 4) as u8, 128]);
        }
        assert_eq!(quantize_mediancut_spot_alpha(&img, 256), quantize_mediancut_alpha(&img, 256));
    }

    /// Spot pass on real renders: `QUANT_EVAL_DIR=<dir of RGBA PNGs> cargo test --release
    /// quant_eval -- --ignored --nocapture`. Prints PSNR and mean error over pixels within 40 of a style colour.
    #[test]
    #[ignore]
    fn quant_eval() {
        let dir = std::env::var("QUANT_EVAL_DIR").expect("QUANT_EVAL_DIR");
        let style: [(&str, [u8; 3]); 4] =
            [("shield", [84, 140, 56]), ("mwy-core", [255, 255, 0]), ("mwy-fill", [230, 104, 104]), ("riksveg", [255, 255, 254])];
        let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
        files.sort();
        for f in files.iter().filter(|f| f.extension().is_some_and(|e| e == "png")) {
            let (img, _) = decode_png(&std::fs::read(f).unwrap()).unwrap();
            type Quant = fn(&Rgba, u32) -> (Vec<[u8; 3]>, Vec<u8>);
        let qs: [(&str, Quant); 2] = [("mediancut", quantize_mediancut_alpha), ("spot", quantize_mediancut_spot_alpha)];
        for (name, q) in qs {
                let t = std::time::Instant::now();
                let (pal, idx) = q(&img, 256);
                let ms = t.elapsed().as_secs_f64() * 1e3;
                let (mut se, mut n) = (0f64, 0f64);
                let mut near = [(0f64, 0u32); 4];
                for (i, p) in img.px.as_chunks::<4>().0.iter().enumerate() {
                    if p[3] <= 128 {
                        continue;
                    }
                    let c = [p[0], p[1], p[2]];
                    let d = dist2(c, decoded_px(&pal, &idx, i));
                    se += d as f64;
                    n += 3.0;
                    for (k, (_, s)) in style.iter().enumerate() {
                        if dist2(c, *s) <= 40 * 40 {
                            near[k].0 += (d as f64).sqrt();
                            near[k].1 += 1;
                        }
                    }
                }
                let kb = encode_png_paletted(img.w, img.h, &pal, &idx, true, png::Compression::Balanced).unwrap().len() / 1024;
                let psnr = 10.0 * (255.0f64 * 255.0 / (se / n)).log10();
                let w: Vec<String> = style.iter().zip(near).map(|((s, _), (e, n))| format!("{s}={:.1}/{n}", e / n.max(1) as f64)).collect();
                println!("{} {name:9} psnr {psnr:.2} dB {ms:.0} ms {kb} KiB pal {} mean-err near {}", f.file_name().unwrap().to_string_lossy(), pal.len(), w.join(" "));
            }
        }
    }

    #[test]
    fn crop_outside_is_zero_and_single_color() {
        let img = Rgba::filled(2, 2, [1, 2, 3, 4]);
        assert_eq!(img.single_color(), Some([1, 2, 3, 4]));
        let c = img.crop(1, 1, 2, 2);
        assert_eq!(&c.px[0..4], &[1, 2, 3, 4]);
        assert_eq!(&c.px[4..8], &[0, 0, 0, 0]);
        assert_eq!(c.single_color(), None);
    }
}
