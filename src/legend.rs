//! Background-map legend (tegnforklaring) from WMS JSON legends (GetLegendGraphic
//! FORMAT=application/json): blank symbols dropped, pixel-identical symbols merged into one row,
//! technical rule names tidied, labels drawn with ab_glyph. Output: 1x/2x PNG + JSON for clients.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use md5::{Digest, Md5};
use serde_json::{Value as Json, json};

use crate::image::Rgba;

/// One legend row from a WMS: label and the same symbol at DPI ×1, ×2, ×4 (None when missing).
pub struct RawItem {
    pub label: String,
    pub icons: [Option<Rgba>; 3],
}

/// A merged row: cleaned labels (unique, in first-seen order), raw names, display icons ×1/×2.
pub struct Item {
    pub labels: Vec<String>,
    pub names: Vec<String>,
    pub icons: [Rgba; 2],
}

impl Item {
    pub fn label(&self) -> String {
        self.labels.join(", ")
    }
}

/// Rows of WMS JSON legend nodes (rGIS / QGIS Server): a single-symbol layer is one row titled by
/// the layer, other layers one row per symbol (untitled symbols take the layer title).
pub fn json_rows(v: &Json) -> Vec<(String, Option<Rgba>)> {
    fn icon(n: &Json) -> Option<Rgba> {
        let b = base64_decode(n.get("icon")?.as_str()?)?;
        crate::image::decode_png(&b).ok().map(|(i, _)| i)
    }
    let s = |n: &Json, k: &str| n.get(k).and_then(Json::as_str).unwrap_or("").to_string();
    let mut out = vec![];
    for n in v.get("nodes").and_then(Json::as_array).into_iter().flatten() {
        let title = s(n, "title");
        match n.get("symbols").and_then(Json::as_array) {
            Some(syms) => {
                for sy in syms {
                    let t = s(sy, "title");
                    out.push((if t.is_empty() { title.clone() } else { t }, icon(sy)));
                }
            }
            None => out.push((title, icon(n))),
        }
    }
    out
}

/// Visible extent (alpha > 8, not white) of an icon: (x0, y0, x1, y1) exclusive, None if blank.
fn ink_bbox(img: &Rgba) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..img.h {
        for x in 0..img.w {
            let p = &img.px[((y * img.w + x) * 4) as usize..][..4];
            if p[3] > 8 && !(p[0] > 250 && p[1] > 250 && p[2] > 250) {
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
            }
        }
    }
    (x1 > 0).then_some((x0, y0, x1, y1))
}

/// Centre `w`×`h` of `img` (for showing a tiny symbol from the next DPI up in the same patch).
fn centre_crop(img: &Rgba, w: u32, h: u32) -> Rgba {
    let (w, h) = (w.min(img.w), h.min(img.h));
    let (ox, oy) = ((img.w - w) / 2, (img.h - h) / 2);
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let s = (((oy + y) * img.w + ox) * 4) as usize;
        px.extend_from_slice(&img.px[s..s + (w * 4) as usize]);
    }
    Rgba { w, h, px }
}

/// A symbol this small (px at ×1) is drawn from the next DPI up so it can be seen.
const TINY: u32 = 5;

/// Blank rows out, pixel-identical ×1 symbols merged (labels joined), tiny symbols enlarged.
pub fn build(raw: Vec<RawItem>) -> Vec<Item> {
    let mut items: Vec<Item> = vec![];
    let mut by_key: std::collections::HashMap<[u8; 16], usize> = Default::default();
    for r in raw {
        let [Some(i1), i2, i4] = r.icons else { continue };
        let Some((x0, y0, x1, y1)) = ink_bbox(&i1) else { continue };
        let key: [u8; 16] = Md5::new().chain_update(i1.w.to_le_bytes()).chain_update(&i1.px).finalize().into();
        let label = clean_label(&r.label);
        if let Some(&k) = by_key.get(&key) {
            let it = &mut items[k];
            if !label.is_empty() && !it.labels.iter().any(|l| l.eq_ignore_ascii_case(&label)) {
                it.labels.push(label);
            }
            if !it.names.contains(&r.label) {
                it.names.push(r.label);
            }
            continue;
        }
        let tiny = x1 - x0 <= TINY && y1 - y0 <= TINY;
        let scale2 = |img: &Rgba| upscale2(img);
        let d1 = match (&i2, tiny) {
            (Some(b), true) => centre_crop(b, i1.w, i1.h),
            _ => i1.clone(),
        };
        let d2 = match (&i2, &i4, tiny) {
            (Some(b), Some(c), true) => centre_crop(c, b.w, b.h),
            (Some(b), _, _) => b.clone(),
            (None, _, _) => scale2(&d1),
        };
        by_key.insert(key, items.len());
        items.push(Item {
            labels: if label.is_empty() { vec![] } else { vec![label] },
            names: vec![r.label],
            icons: [d1, d2],
        });
    }
    items
}

/// Nearest-neighbour ×2 (fallback when the ×2 DPI fetch failed).
fn upscale2(img: &Rgba) -> Rgba {
    let (w, h) = (img.w * 2, img.h * 2);
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&img.px[(((y / 2) * img.w + x / 2) * 4) as usize..][..4]);
        }
    }
    Rgba { w, h, px }
}

/// Technical rule/layer name → readable text: dataset/geometry prefix (`n50_F_`, `eu_L_`,
/// `S100 P `, `j1000_P_`), scale-range tokens (`55k-12k`, `1,500k-0`), road codes (`rtt16`),
/// `Lag2`, bare numbers and `samferdsel_terreng_` dropped; CamelCase split; first letter upper.
pub fn clean_label(s: &str) -> String {
    let s = s.replace("samferdsel_terreng_", "");
    let mut toks: Vec<String> = s.split(['_', ' ']).filter(|t| !t.is_empty()).map(str::to_string).collect();
    let dataset = |t: &str| {
        let l = t.to_ascii_lowercase();
        l == "eu" || (l.len() >= 2 && matches!(l.as_bytes()[0], b'n' | b's' | b'j') && l[1..].bytes().all(|b| b.is_ascii_digit()))
    };
    if toks.len() > 2 && dataset(&toks[0]) && matches!(toks[1].as_str(), "F" | "L" | "P" | "f" | "l" | "p") {
        toks.drain(..2);
    }
    let noise = |t: &str| {
        let l = t.to_ascii_lowercase();
        let scale = l.contains('k')
            && l.split('-').all(|p| {
                let p = p.trim_end_matches('k');
                !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit() || b == b',')
            });
        let code = |pre: &str| l.strip_prefix(pre).is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()));
        scale || code("rtt") || code("lag") || l.bytes().all(|b| b.is_ascii_digit())
    };
    toks.retain(|t| !noise(t));
    let mut words = vec![];
    for t in toks {
        // CamelCase → words ("ÅpentOmråde" → "Åpent område"); all-caps tokens kept.
        let cs: Vec<char> = t.chars().collect();
        let mut w = String::new();
        for (i, &c) in cs.iter().enumerate() {
            if i > 0 && c.is_uppercase() && cs[i - 1].is_lowercase() {
                words.push(std::mem::take(&mut w));
            }
            w.push(c);
        }
        words.push(w);
    }
    let words: Vec<String> = words
        .into_iter()
        .enumerate()
        .map(|(i, w)| {
            let upper = w.chars().filter(|c| c.is_alphabetic()).all(char::is_uppercase) && w.chars().count() > 1;
            if i > 0 && !upper { w.to_lowercase() } else { w }
        })
        .collect();
    let mut out = words.join(" ");
    if let Some(c) = out.chars().next() {
        out = c.to_uppercase().chain(out.chars().skip(1)).collect();
    }
    out
}

/// Text drawing with one font at ×1 sizes scaled by `k`.
pub struct Text {
    font: FontVec,
}

impl Text {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Text> {
        let b = std::fs::read(path)?;
        Ok(Text { font: FontVec::try_from_vec(b).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))? })
    }

    fn width(&self, px: f32, s: &str) -> f32 {
        let f = self.font.as_scaled(PxScale::from(px));
        let mut w = 0.0;
        let mut prev = None;
        for c in s.chars() {
            let id = f.glyph_id(c);
            if let Some(p) = prev {
                w += f.kern(p, id);
            }
            w += f.h_advance(id);
            prev = Some(id);
        }
        w
    }

    /// Greedy word wrap at `max` px; a single word longer than `max` keeps its own line.
    fn wrap(&self, px: f32, s: &str, max: f32) -> Vec<String> {
        let mut lines: Vec<String> = vec![];
        let mut cur = String::new();
        for word in s.split(' ') {
            let cand = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
            if !cur.is_empty() && self.width(px, &cand) > max {
                lines.push(std::mem::replace(&mut cur, word.to_string()));
            } else {
                cur = cand;
            }
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        lines
    }

    /// Fill `s` in `rgb` with its baseline at (x, y), alpha-blended into an opaque image.
    fn draw(&self, img: &mut Rgba, px: f32, x: f32, y: f32, s: &str, rgb: [u8; 3]) {
        let f = self.font.as_scaled(PxScale::from(px));
        let mut pen = x;
        let mut prev = None;
        for c in s.chars() {
            let id = f.glyph_id(c);
            if let Some(p) = prev {
                pen += f.kern(p, id);
            }
            let g = id.with_scale_and_position(px, point(pen, y));
            pen += f.h_advance(id);
            prev = Some(id);
            let Some(og) = self.font.outline_glyph(g) else { continue };
            let b = og.px_bounds();
            og.draw(|gx, gy, cov| {
                let (ix, iy) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                if ix < 0 || iy < 0 || ix >= img.w as i32 || iy >= img.h as i32 {
                    return;
                }
                let d = &mut img.px[((iy as u32 * img.w + ix as u32) * 4) as usize..][..4];
                let a = cov.clamp(0.0, 1.0);
                for ch in 0..3 {
                    d[ch] = (rgb[ch] as f32 * a + d[ch] as f32 * (1.0 - a)).round() as u8;
                }
            });
        }
    }
}

/// Rows top to bottom on white at ×`k` (1 or 2): symbol, then its wrapped label (Noto 12 px at ×1).
pub fn render(items: &[Item], k: u32, text: Option<&Text>) -> Rgba {
    let kf = k as f32;
    let (pad, gap, px, max_w) = (2 * k, 6.0 * kf, 12.0 * kf, 280.0 * kf);
    let idx = (k as usize).min(2) - 1;
    let line_h = text.map_or(0.0, |t| {
        let f = t.font.as_scaled(PxScale::from(px));
        f.ascent() - f.descent() + f.line_gap()
    });
    let rows: Vec<(&Rgba, Vec<String>)> = items
        .iter()
        .map(|it| (&it.icons[idx], text.map(|t| t.wrap(px, &it.label(), max_w)).unwrap_or_default()))
        .collect();
    let icon_w = rows.iter().map(|(i, _)| i.w).max().unwrap_or(1);
    let text_w = text.map_or(0.0, |t| rows.iter().flat_map(|(_, ls)| ls.iter().map(|l| t.width(px, l))).fold(0.0, f32::max));
    let row_h = |(i, ls): &(&Rgba, Vec<String>)| (i.h as f32).max(ls.len() as f32 * line_h).ceil() as u32;
    let h = rows.iter().map(|r| row_h(r) + pad).sum::<u32>() + pad;
    let w = pad * 2 + icon_w + if text_w > 0.0 { (gap + text_w).ceil() as u32 } else { 0 };
    let mut out = Rgba::filled(w, h.max(1), [255, 255, 255, 255]);
    let mut y = pad;
    for r in &rows {
        let rh = row_h(r);
        let (icon, lines) = r;
        blend(&mut out, icon, pad, y + (rh - icon.h) / 2);
        if let Some(t) = text {
            let f = t.font.as_scaled(PxScale::from(px));
            // Text block centred on the row; each line's baseline at ascent within its slot.
            let top = y as f32 + (rh as f32 - lines.len() as f32 * line_h) / 2.0;
            for (n, l) in lines.iter().enumerate() {
                let base = (top + n as f32 * line_h + f.ascent()).round();
                t.draw(&mut out, px, (pad + icon_w) as f32 + gap, base, l, [0, 0, 0]);
            }
        }
        y += rh + pad;
    }
    out
}

fn blend(dst: &mut Rgba, src: &Rgba, x0: u32, y0: u32) {
    for y in 0..src.h.min(dst.h.saturating_sub(y0)) {
        for x in 0..src.w.min(dst.w.saturating_sub(x0)) {
            let s = &src.px[((y * src.w + x) * 4) as usize..][..4];
            let d = &mut dst.px[(((y0 + y) * dst.w + x0 + x) * 4) as usize..][..4];
            let a = s[3] as u32;
            for c in 0..3 {
                d[c] = ((s[c] as u32 * a + d[c] as u32 * (255 - a) + 127) / 255) as u8;
            }
        }
    }
}

/// Client form: label, raw rule names, ×2 symbol as a data URL with its ×1 (CSS px) size.
pub fn to_json(items: &[Item], scale: u32) -> anyhow::Result<Json> {
    let mut rows = vec![];
    for it in items {
        let i2 = &it.icons[1];
        let png = crate::image::encode_png_rgba(i2, png::Compression::Balanced)?;
        rows.push(json!({
            "label": it.label(),
            "names": it.names,
            "icon": format!("data:image/png;base64,{}", base64_encode(&png)),
            "width": it.icons[0].w,
            "height": it.icons[0].h,
        }));
    }
    Ok(json!({"scale": scale, "items": rows}))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() { B64[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for b in s.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=') {
        acc = acc << 6 | B64.iter().position(|&c| c == b)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_technical_names() {
        for (a, b) in [
            ("n50_L_samferdsel_terreng_motorveg", "Motorveg"),
            ("eu_L_roadl_rtt16_motorway_tunnel_1000k-12k", "Roadl motorway tunnel"),
            ("n50_F_ÅpentOmråde", "Åpent område"),
            ("n50_F_SportIdrettPlass", "Sport idrett plass"),
            ("eu_P_builtupp_4_1,500k-0", "Builtupp"),
            ("S100 F morene", "Morene"),
            ("s100_L_veg_Lag2", "Veg"),
            ("j1000_P_lufthavn", "Lufthavn"),
            ("n50_L_ElvBekk_2", "Elv bekk"),
            ("Sykehus m akuttmottak 105k-3k", "Sykehus m akuttmottak"),
            ("Maritime grenser — avtaltavgrensningslinje", "Maritime grenser — avtaltavgrensningslinje"),
            ("Hav", "Hav"),
            ("n50_P_MastTele", "Mast tele"),
        ] {
            assert_eq!(clean_label(a), b, "{a}");
        }
    }

    #[test]
    fn drops_blank_merges_identical_enlarges_tiny() {
        let dot = |w: u32, h: u32, r: u32| {
            let mut i = Rgba::filled(w, h, [0, 0, 0, 0]);
            for y in h / 2 - r..h / 2 + r {
                for x in w / 2 - r..w / 2 + r {
                    i.px[((y * w + x) * 4) as usize..][..4].copy_from_slice(&[200, 0, 0, 255]);
                }
            }
            i
        };
        let blank = Rgba::filled(26, 15, [0, 0, 0, 0]);
        let raw = vec![
            RawItem { label: "n50_P_tank".into(), icons: [Some(dot(26, 15, 1)), Some(dot(52, 30, 2)), Some(dot(104, 60, 4))] },
            RawItem { label: "Eu P gnamep".into(), icons: [Some(blank), None, None] },
            RawItem { label: "n50_P_bolig".into(), icons: [Some(dot(26, 15, 1)), Some(dot(52, 30, 2)), Some(dot(104, 60, 4))] },
            RawItem { label: "s100_P_tank".into(), icons: [Some(dot(26, 15, 1)), None, None] },
        ];
        let items = build(raw);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label(), "Tank, Bolig");
        assert_eq!(items[0].names.len(), 3);
        // Tiny 2×2 dot shown from the ×2 fetch: 4×4 in the ×1 patch, 8×8 in ×2.
        assert_eq!(ink_bbox(&items[0].icons[0]).map(|(a, b, c, d)| (c - a, d - b)), Some((4, 4)));
        assert_eq!(ink_bbox(&items[0].icons[1]).map(|(a, b, c, d)| (c - a, d - b)), Some((8, 8)));
        assert_eq!((items[0].icons[0].w, items[0].icons[1].w), (26, 52));
    }

    #[test]
    fn base64_roundtrip() {
        for d in [&b""[..], b"a", b"ab", b"abc", b"\x00\xff\x10legend"] {
            assert_eq!(base64_decode(&base64_encode(d)).unwrap(), d);
        }
    }

    #[test]
    fn json_rows_flatten_nodes() {
        let v = json!({"nodes": [
            {"type": "layer", "title": "Hav", "icon": "x"},
            {"type": "layer", "title": "Veg", "symbols": [{"title": "", "icon": "x"}, {"title": "Sti", "icon": "x"}]},
        ]});
        let labels: Vec<String> = json_rows(&v).into_iter().map(|(l, _)| l).collect();
        assert_eq!(labels, ["Hav", "Veg", "Sti"]);
    }

    /// Real legends from a running rGIS: LEGEND_E2E_BASE (`http://localhost:18000/?MAP=/p/`),
    /// LEGEND_E2E_PROJECTS (comma list, layer = project name), LEGEND_E2E_SCALE, LEGEND_E2E_OUT,
    /// RMAPPROXY_LEGEND_FONT. `cargo test legend_e2e -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn legend_e2e() {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k}"));
        let (base, out) = (env("LEGEND_E2E_BASE"), std::path::PathBuf::from(env("LEGEND_E2E_OUT")));
        let scale: u32 = env("LEGEND_E2E_SCALE").parse().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut raw = vec![];
        for p in env("LEGEND_E2E_PROJECTS").split(',') {
            let get = |k: u32| {
                let u = format!(
                    "{base}{p}.qgs&SERVICE=WMS&REQUEST=GetLegendGraphic&VERSION=1.3.0&FORMAT=application%2Fjson&LAYERTITLE=false&DPI={}&LAYER={p}&SCALE={scale}",
                    96 * k
                );
                rt.block_on(async { reqwest::get(&u).await.ok()?.bytes().await.ok() })
                    .and_then(|b| serde_json::from_slice::<Json>(&b).ok())
                    .map(|v| json_rows(&v))
            };
            let (r1, r2, r4) = (get(1), get(2), get(4));
            let Some(r1) = r1 else { eprintln!("{p}: no legend"); continue };
            let pick = |r: &Option<Vec<(String, Option<Rgba>)>>, i: usize| r.as_ref().filter(|r| r.len() == r1.len()).and_then(|r| r[i].1.clone());
            for (i, (l, ic)) in r1.iter().enumerate() {
                raw.push(RawItem { label: l.clone(), icons: [ic.clone(), pick(&r2, i), pick(&r4, i)] });
            }
        }
        let n_raw = raw.len();
        let text = Text::load(std::path::Path::new(&env("RMAPPROXY_LEGEND_FONT"))).unwrap();
        let items = build(raw);
        eprintln!("raw rows {n_raw} → legend rows {}", items.len());
        for (k, f) in [(1, format!("topo-{scale}.png")), (2, format!("topo-{scale}@2x.png"))] {
            let img = render(&items, k, Some(&text));
            eprintln!("{f}: {}x{}", img.w, img.h);
            std::fs::write(out.join(f), crate::image::encode_png_rgba(&img, png::Compression::Balanced).unwrap()).unwrap();
        }
        std::fs::write(out.join(format!("topo-{scale}.json")), serde_json::to_vec_pretty(&to_json(&items, scale).unwrap()).unwrap()).unwrap();
    }
}
