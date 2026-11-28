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

/// zlib level 6, what png::Compression::Balanced maps to (flate2's default level).
pub const PNG_BALANCED: png::DeflateCompression = png::DeflateCompression::Level(6);
/// fdeflate, png::Compression::Fast.
pub const PNG_FAST: png::DeflateCompression = png::DeflateCompression::FdeflateUltraFast;
/// Cached tiles: zlib 4 is as small as 6 (RGBA +0.2 %, paletted ±0) at 40-60 % less time (Oslo
/// z12/z15: 16 RGBA tiles 81 → 49 ms, 4 paletted 512² 55 → 22 ms). Level 2 halves it again but
/// adds 2-3 % bytes; fdeflate adds ~35 % (cache volumes are tight, so size stays).
pub const PNG_TILE: png::DeflateCompression = png::DeflateCompression::Level(4);

/// `decode_png` of a `size` image, then merged into `canvas` as `Merger` would: placed at
/// `place` (a coverage sub-image: pasted into a blank canvas, then composited = composite at the
/// offset), else by its MergeMode at 0,0. Row by row, so no image-sized buffer per source (6 MB
/// at 1224²; the alloc/free churn was 25-40 % of seed CPU as system time, musl worst).
pub fn merge_png_into(canvas: &mut Rgba, data: &[u8], size: (u32, u32), place: Option<(i64, i64)>) -> Result<()> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| anyhow!("png: {e}"))?;
    let info = reader.info();
    if (info.width, info.height) != size {
        bail!("got {}x{}, asked {}x{}", info.width, info.height, size.0, size.1);
    }
    let interlaced = info.interlaced;
    let mode = match (place, info.color_type) {
        (Some(_), _) | (None, png::ColorType::Rgba | png::ColorType::Indexed) => MergeMode::Composite,
        _ if info.trns.is_some() => MergeMode::Composite,
        _ => MergeMode::Paste,
    };
    let (x, y) = place.unwrap_or((0, 0));
    if interlaced {
        // Adam7 rows arrive per pass: decode whole (rGIS/QGIS don't interlace).
        let (img, _) = decode_png(data)?;
        for (sy, row) in img.px.chunks_exact(img.w as usize * 4).enumerate() {
            merge_row(canvas, row, x, y + sy as i64, mode);
        }
        return Ok(());
    }
    let ctype = reader.output_color_type().0;
    let mut rgba = Vec::with_capacity(size.0 as usize * 4);
    let mut sy = 0i64;
    while let Some(row) = reader.next_row().map_err(|e| anyhow!("png: {e}"))? {
        let src = row.data();
        let px: &[u8] = match ctype {
            png::ColorType::Rgba => src,
            png::ColorType::Rgb => {
                rgba.clear();
                rgba.extend(src.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]));
                &rgba
            }
            png::ColorType::Grayscale => {
                rgba.clear();
                rgba.extend(src.iter().flat_map(|&g| [g, g, g, 255]));
                &rgba
            }
            png::ColorType::GrayscaleAlpha => {
                rgba.clear();
                rgba.extend(src.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]]));
                &rgba
            }
            png::ColorType::Indexed => bail!("png: palette not expanded"),
        };
        if px.len() != size.0 as usize * 4 {
            bail!("png: unexpected row size");
        }
        merge_row(canvas, px, x, y + sy, mode);
        sy += 1;
    }
    if sy != size.1 as i64 {
        bail!("png: {sy} of {} rows", size.1);
    }
    Ok(())
}

/// One source row (RGBA) at canvas (x, y): composited or pasted, clipped like `paste`.
fn merge_row(canvas: &mut Rgba, row: &[u8], x: i64, y: i64, mode: MergeMode) {
    if y < 0 || y >= canvas.h as i64 {
        return;
    }
    let w = (row.len() / 4) as i64;
    let (x0, x1) = (x.max(0), (x + w).min(canvas.w as i64));
    if x0 >= x1 {
        return;
    }
    let s = &row[((x0 - x) * 4) as usize..((x1 - x) * 4) as usize];
    let d0 = ((y * canvas.w as i64 + x0) * 4) as usize;
    let d = &mut canvas.px[d0..d0 + s.len()];
    match mode {
        MergeMode::Paste => d.copy_from_slice(s),
        MergeMode::Composite => {
            for (d, s) in d.as_chunks_mut::<4>().0.iter_mut().zip(s.as_chunks::<4>().0) {
                composite_px(d, s);
            }
        }
    }
}

/// RGBA PNG (MapProxy's default for transparent png caches without `colors`).
pub fn encode_png_rgba(img: &Rgba, compression: png::DeflateCompression) -> Result<Vec<u8>> {
    encode_png_rgba_filtered(img, compression, png::Filter::Adaptive)
}

fn encode_png_rgba_filtered(img: &Rgba, compression: png::DeflateCompression, filter: png::Filter) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(img.px.len() / 4);
    {
        let mut enc = png::Encoder::new(&mut out, img.w, img.h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_filter(filter);
        enc.set_deflate_compression(compression);
        let mut w = enc.write_header()?;
        w.write_image_data(&img.px)?;
    }
    Ok(out)
}

/// Paletted PNG as Pillow's PngImagePlugin writes a P image with `transparency=255`:
/// bit depth from the palette size, PLTE = palette, tRNS = (ff * 255 + 00)[:colors],
/// indices packed to the bit depth (so index 255 keeps only its low bits).
pub fn encode_png_paletted(w: u32, h: u32, palette: &[[u8; 3]], idx: &[u8], transparency: bool, compression: png::DeflateCompression) -> Result<Vec<u8>> {
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
        enc.set_deflate_compression(compression);
        let mut wr = enc.write_header()?;
        write_indices(&mut wr, w, h, bits, idx)?;
    }
    Ok(out)
}

/// Palette indices as PNG rows of `bits` per pixel (8: as is; less: packed, high bits first,
/// keeping only each index's low bits).
fn write_indices<W: std::io::Write>(wr: &mut png::Writer<W>, w: u32, h: u32, bits: u8, idx: &[u8]) -> Result<()> {
    if bits == 8 {
        wr.write_image_data(idx)?;
        return Ok(());
    }
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
    Ok(())
}

/// `encoding_options.lossless: true` (rMapproxy only): the same pixels in fewer bytes. ≤256
/// distinct RGBA values (sea, sparse land, empty margins) → palette PNG whose tRNS holds each
/// entry's exact alpha; else opaque → RGB; else RGBA. Decodes to exactly `img` (each RGBA value
/// incl. RGB under alpha 0). Not MapProxy's bytes, so off by default.
pub fn encode_png_lossless(img: &Rgba, compression: png::DeflateCompression) -> Result<Vec<u8>> {
    if let Some((pal, idx)) = exact_palette(img) {
        return encode_png_exact_palette(img.w, img.h, &pal, &idx, compression);
    }
    if img.px.as_chunks::<4>().0.iter().all(|p| p[3] == 255) {
        let rgb: Vec<u8> = img.px.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        let mut out = Vec::with_capacity(rgb.len() / 4);
        {
            let mut enc = png::Encoder::new(&mut out, img.w, img.h);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            // No row filter: flat areas, hatching and repeated symbols deflate better unfiltered
            // (topo samples: 76-81 % of the Adaptive size, and faster).
            enc.set_filter(png::Filter::NoFilter);
            enc.set_deflate_compression(compression);
            enc.write_header()?.write_image_data(&rgb)?;
        }
        return Ok(out);
    }
    encode_png_rgba_filtered(img, compression, png::Filter::NoFilter)
}

/// Palette + indices if `img` has ≤256 distinct RGBA values, else None (stops at the 257th).
/// Non-opaque entries first, so tRNS ends at the last of them (opaque ones need none).
fn exact_palette(img: &Rgba) -> Option<(Vec<[u8; 4]>, Vec<u8>)> {
    let px = img.px.as_chunks::<4>().0;
    let mut map: std::collections::HashMap<[u8; 4], u8> = std::collections::HashMap::with_capacity(512);
    let mut pal: Vec<[u8; 4]> = Vec::new();
    let mut idx = Vec::with_capacity(px.len());
    let mut last: Option<([u8; 4], u8)> = None;
    for p in px {
        // Runs of one colour are the norm in tiles: skip the hash lookup for them.
        let i = match last {
            Some((c, i)) if c == *p => i,
            _ => {
                let i = match map.get(p) {
                    Some(&i) => i,
                    None => {
                        if pal.len() == 256 {
                            return None;
                        }
                        let i = pal.len() as u8;
                        map.insert(*p, i);
                        pal.push(*p);
                        i
                    }
                };
                last = Some((*p, i));
                i
            }
        };
        idx.push(i);
    }
    // Reorder: translucent entries first (stable), then remap the indices.
    let mut order: Vec<usize> = (0..pal.len()).collect();
    order.sort_by_key(|&i| pal[i][3] == 255);
    let mut remap = [0u8; 256];
    for (new, &old) in order.iter().enumerate() {
        remap[old] = new as u8;
    }
    let pal = order.iter().map(|&i| pal[i]).collect();
    for i in &mut idx {
        *i = remap[*i as usize];
    }
    Some((pal, idx))
}

/// Indexed PNG for `exact_palette`: smallest bit depth for the entry count, tRNS only up to the
/// last translucent entry, no row filter (palette rows rarely gain from one, and it costs time).
fn encode_png_exact_palette(w: u32, h: u32, pal: &[[u8; 4]], idx: &[u8], compression: png::DeflateCompression) -> Result<Vec<u8>> {
    let bits: u8 = match pal.len() {
        0..=2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        _ => 8,
    };
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
        enc.set_palette(pal.iter().flat_map(|p| [p[0], p[1], p[2]]).collect::<Vec<u8>>());
        let translucent = pal.iter().take_while(|p| p[3] != 255).count();
        if translucent > 0 {
            enc.set_trns(pal[..translucent].iter().map(|p| p[3]).collect::<Vec<u8>>());
        }
        enc.set_filter(png::Filter::NoFilter);
        enc.set_deflate_compression(compression);
        let mut wr = enc.write_header()?;
        write_indices(&mut wr, w, h, bits, idx)?;
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
    use rustc_hash::FxHashMap as HashMap; // order-independent: sums, max, fully tie-broken sort
    let rgb = rgb_of(img);
    let (pal, idx) = crate::quantize::mediancut(&rgb, colors - 1);
    let opaque = |i: usize| img.px[i * 4 + 3] > 128;
    // Exact-colour histogram of opaque pixels, with median cut's entry for that colour.
    let mut hist: HashMap<[u8; 3], (u32, u8)> = HashMap::default();
    for (i, &c) in rgb.iter().enumerate() {
        if opaque(i) {
            hist.entry(c).or_insert((0, idx[i])).0 += 1;
        }
    }
    // Per bin: pixels, summed squared error, most common exact colour (count, colour).
    type Bin = (u32, u64, (u32, [u8; 3]));
    let mut bins: HashMap<[u8; 3], Bin> = HashMap::default();
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
    let mut memo: HashMap<([u8; 3], u8), u8> = HashMap::default();
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

    /// `encode_png_lossless` decodes to exactly its input, in the expected PNG form: palette for
    /// ≤256 RGBA values (any alpha, RGB under alpha 0 kept), RGB when opaque, RGBA otherwise.
    #[test]
    fn lossless_roundtrips_exactly() {
        let ctype = |png: &[u8]| png::Decoder::new(std::io::Cursor::new(png)).read_info().unwrap().info().color_type;
        let depth = |png: &[u8]| png::Decoder::new(std::io::Cursor::new(png)).read_info().unwrap().info().bit_depth as u8;
        let (w, h) = (61, 17);
        let with = |n: usize, alpha: fn(usize) -> u8| {
            let mut img = Rgba::filled(w, h, [0, 0, 0, 0]);
            for (k, p) in img.px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let c = (k * 7919) % n;
                *p = [c as u8, (c >> 8) as u8 ^ 0x5a, (c * 3) as u8, alpha(c)];
            }
            img
        };
        // (distinct colours, alpha per colour, expected colour type, expected bit depth)
        type Case = (usize, fn(usize) -> u8, png::ColorType, u8);
        let cases: [Case; 7] = [
            (1, |_| 255, png::ColorType::Indexed, 1),
            (2, |c| if c == 0 { 0 } else { 255 }, png::ColorType::Indexed, 1),
            (4, |c| (c * 60) as u8, png::ColorType::Indexed, 2),
            (16, |c| if c % 3 == 0 { 0 } else { 255 }, png::ColorType::Indexed, 4),
            (256, |c| (c % 7 * 40) as u8, png::ColorType::Indexed, 8),
            (257, |_| 255, png::ColorType::Rgb, 8),
            (900, |c| (c % 5 * 60) as u8, png::ColorType::Rgba, 8),
        ];
        for (n, alpha, want, bits) in cases {
            let img = with(n, alpha);
            let png = encode_png_lossless(&img, PNG_BALANCED).unwrap();
            assert_eq!(decode_png(&png).unwrap().0, img, "{n} colours");
            assert_eq!(ctype(&png), want, "{n} colours");
            assert_eq!(depth(&png), bits, "{n} colours");
        }
        // Distinct RGB under alpha 0 are distinct entries, and survive.
        let mut img = Rgba::filled(4, 1, [0, 0, 0, 0]);
        img.px[4..8].copy_from_slice(&[9, 8, 7, 0]);
        img.px[8..12].copy_from_slice(&[1, 2, 3, 128]);
        assert_eq!(decode_png(&encode_png_lossless(&img, PNG_TILE).unwrap()).unwrap().0, img);
    }

    /// Sizes/time of `encode_png_lossless` vs the RGBA encoding on real tiles, every result checked
    /// to decode to the same pixels. `RMAPPROXY_TILE_SAMPLES=dir cargo test --release lossless_on_samples -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn lossless_on_samples() {
        let Ok(dir) = std::env::var("RMAPPROXY_TILE_SAMPLES") else { return };
        type Enc = fn(&Rgba) -> Result<Vec<u8>>;
        let encs: [(&str, Enc); 6] = [
            ("rgba L4 (now)", |i| encode_png_rgba(i, PNG_TILE)),
            ("rgba None L4", |i| encode_png_rgba_filtered(i, PNG_TILE, png::Filter::NoFilter)),
            ("lossless L4", |i| encode_png_lossless(i, PNG_TILE)),
            ("lossless L6", |i| encode_png_lossless(i, PNG_BALANCED)),
            ("lossless L9", |i| encode_png_lossless(i, png::DeflateCompression::Level(9))),
            ("rgba L9", |i| encode_png_rgba(i, png::DeflateCompression::Level(9))),
        ];
        let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|x| x == "png")).collect();
        files.sort();
        // group (z <= 12 | z >= 13) -> per encoder (bytes, micros); plus kinds
        let mut tot = [[(0u64, 0u128); 6]; 2];
        let mut orig = [0u64; 2];
        let (mut n, mut kinds) = ([0u32; 2], [[0u32; 3]; 2]);
        for f in files {
            let data = std::fs::read(&f).unwrap();
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            let z: u32 = name.split('_').nth(1).unwrap().parse().unwrap();
            let g = usize::from(z >= 13);
            let (img, _) = decode_png(&data).unwrap();
            orig[g] += data.len() as u64;
            n[g] += 1;
            let k = if exact_palette(&img).is_some() { 0 } else if img.px.as_chunks::<4>().0.iter().all(|p| p[3] == 255) { 1 } else { 2 };
            kinds[g][k] += 1;
            for (e, (label, enc)) in encs.iter().enumerate() {
                let t = std::time::Instant::now();
                let mut out = Vec::new();
                for _ in 0..3 {
                    out = enc(&img).unwrap();
                }
                tot[g][e].1 += t.elapsed().as_micros() / 3;
                tot[g][e].0 += out.len() as u64;
                let (back, _) = decode_png(&out).unwrap();
                assert!(back == img, "{label}: {name} decodes to different pixels");
            }
        }
        for g in 0..2 {
            if n[g] == 0 {
                continue;
            }
            println!("{} : {} tiles, served files {} KB avg; palette {} / RGB {} / RGBA {}", ["z<=12", "z>=13"][g], n[g], orig[g] / n[g] as u64 / 1024, kinds[g][0], kinds[g][1], kinds[g][2]);
            for (e, (label, _)) in encs.iter().enumerate() {
                let (b, us) = tot[g][e];
                println!("  {label:15} avg {:6.1} KB  {:5.1} % of now  {:6.2} ms/tile  (pixel-identical)", b as f64 / n[g] as f64 / 1024.0, 100.0 * b as f64 / tot[g][0].0 as f64, us as f64 / n[g] as f64 / 1000.0);
            }
        }
    }
    use super::*;

    /// Streaming merge = decode_png + Merger's paste/composite, for every colour type and offset.
    #[test]
    fn merge_png_into_matches_decode_then_merge() {
        let (w, h) = (37u32, 23u32);
        let px = |i: usize, c: usize| ((i * 31 + c * 77) % 256) as u8;
        let enc = |ct: png::ColorType, ch: usize, trns: Option<Vec<u8>>| {
            let data: Vec<u8> = (0..(w * h) as usize).flat_map(|i| (0..ch).map(move |c| if ch == 4 && c == 3 && i % 5 == 0 { 0 } else { px(i, c) })).collect();
            let mut out = Vec::new();
            {
                let mut e = png::Encoder::new(&mut out, w, h);
                e.set_color(ct);
                e.set_depth(png::BitDepth::Eight);
                if let Some(t) = trns {
                    e.set_trns(t);
                }
                let mut wr = e.write_header().unwrap();
                wr.write_image_data(&data).unwrap();
            }
            out
        };
        let pngs = [
            enc(png::ColorType::Rgba, 4, None),
            enc(png::ColorType::Rgb, 3, None),
            enc(png::ColorType::Rgb, 3, Some(vec![0, px(0, 1), 0, px(0, 2), 0, 0])),
            enc(png::ColorType::GrayscaleAlpha, 2, None),
            enc(png::ColorType::Grayscale, 1, None),
        ];
        let base = || {
            let mut c = Rgba::blank(40, 30);
            for (i, p) in c.px.chunks_mut(4).enumerate() {
                p.copy_from_slice(&[px(i, 2), px(i, 0), px(i, 1), (i * 13 % 256) as u8]);
            }
            c
        };
        for data in &pngs {
            for place in [None, Some((5, 4)), Some((-3, -2)), Some((20, 15))] {
                let mut want = base();
                let (img, mode) = decode_png(data).unwrap();
                match place {
                    // SourceImage::decode: pasted into a blank metatile canvas, then composited.
                    Some((x, y)) => {
                        let mut sub = Rgba::blank(40, 30);
                        sub.paste(&img, x, y);
                        want.alpha_composite(&sub);
                    }
                    None if mode == MergeMode::Paste => want.paste(&img, 0, 0),
                    None => {
                        let mut full = Rgba::blank(40, 30);
                        full.paste(&img, 0, 0);
                        want.alpha_composite(&full);
                    }
                }
                let mut got = base();
                merge_png_into(&mut got, data, (w, h), place).unwrap();
                assert!(got.px == want.px, "{mode:?} {place:?}");
            }
        }
        assert!(merge_png_into(&mut base(), &pngs[0], (w + 1, h), None).is_err());
    }

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
        let png = encode_png_paletted(4, 4, &pal, &idx, true, PNG_FAST).unwrap();
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
        let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|x| x == "png")).collect();
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
                let kb = encode_png_paletted(img.w, img.h, &pal, &idx, true, PNG_BALANCED).unwrap().len() / 1024;
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
