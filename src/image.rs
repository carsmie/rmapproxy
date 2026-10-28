//! RGBA images and the Pillow operations MapProxy uses on them: decoding, `alpha_composite`
//! (libImaging/AlphaComposite.c, integer-exact), paste, crop, single-colour test, PNG
//! encoding.

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
    // Pillow mode before expansion: RGBA and P (with or without tRNS) are composited,
    // RGB/L with a tRNS colour key are converted to RGBA first ('transparency' in info),
    // plain RGB/L/LA are pasted.
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
    let rgb: Vec<[u8; 3]> = img.px.as_chunks::<4>().0.iter().map(|p| [p[0], p[1], p[2]]).collect();
    let (pal, mut idx) = crate::quantize::mediancut(&rgb, colors - 1);
    for (i, p) in img.px.as_chunks::<4>().0.iter().enumerate() {
        if p[3] <= 128 {
            idx[i] = 255;
        }
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
