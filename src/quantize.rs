//! Pillow's median-cut quantizer (libImaging/Quant.c `quantize`, QuantHash.c, QuantHeap.c,
//! Pillow 12.3.0), ported line by line: the hash table's bucket/chain order decides the
//! order of equal colours in the sorted lists and therefore where boxes split, so it is
//! reproduced exactly (including `_findPrime`'s `!start % t` quirk). Plus MapProxy's use of
//! it (`image/__init__.py::quantize`, quantizer=mediancut, alpha=True).

use std::collections::HashMap;

const MAX_HASH_ENTRIES: u32 = 65536;
const MIN_LENGTH: u32 = 11;
const RESIZE_FACTOR: u32 = 3;

type Px = [u8; 3];

#[inline]
fn pixel_hash(p: Px, scale: u32) -> u32 {
    let (r, g, b) = ((p[0] as u32) >> scale, (p[1] as u32) >> scale, (p[2] as u32) >> scale);
    r.wrapping_mul(463) ^ (g << 8).wrapping_mul(10069) ^ (b << 16).wrapping_mul(64997)
}

#[inline]
fn dist_sqr(a: Px, b: Px) -> u32 {
    let d = |i: usize| a[i] as i32 - b[i] as i32;
    (d(0) * d(0) + d(1) * d(1) + d(2) * d(2)) as u32
}

/// `_findPrime` as written (its primality test never rejects, so it returns the first
/// value whose low nibble is in `unit`).
fn find_prime(mut start: u32, dir: i32) -> u32 {
    const UNIT: [u8; 16] = [0, 1, 0, 1, 0, 0, 0, 1, 0, 1, 0, 1, 0, 1, 0, 0];
    while start > 1 {
        if UNIT[(start & 0x0f) as usize] == 0 {
            start = start.wrapping_add_signed(dir);
            continue;
        }
        // for (t = 2; t < sqrt(start); t++) if (!start % t) break;  -- never breaks
        let mut t: u32 = 2;
        while (t as f64) < (start as f64).sqrt() {
            t += 1;
        }
        if (t as f64) >= (start as f64).sqrt() {
            break;
        }
        start = start.wrapping_add_signed(dir);
    }
    start
}

struct Node {
    next: Option<u32>,
    key: Px,
    value: u32,
}

/// QuantHash with `pixel_hash` / `pixel_cmp` (keys equal when their scaled hashes are).
struct PixelHash {
    table: Vec<Option<u32>>,
    nodes: Vec<Node>,
    count: u32,
    scale: u32,
}

impl PixelHash {
    fn new() -> Self {
        PixelHash { table: vec![None; MIN_LENGTH as usize], nodes: vec![], count: 0, scale: 0 }
    }

    fn len(&self) -> u32 {
        self.table.len() as u32
    }

    #[inline]
    fn h(&self, k: Px) -> u32 {
        pixel_hash(k, self.scale)
    }

    /// Walk a bucket: Ok(node) if a key compares equal, Err(link) = insertion point (the
    /// link to rewrite: None = bucket head, Some(n) = n.next).
    fn find(&self, key: Px) -> (u32, Result<u32, Option<u32>>) {
        let kh = self.h(key);
        let bucket = kh % self.len();
        let mut prev: Option<u32> = None;
        let mut cur = self.table[bucket as usize];
        while let Some(n) = cur {
            let nh = self.h(self.nodes[n as usize].key);
            if nh == kh {
                return (bucket, Ok(n));
            }
            if nh > kh {
                break;
            }
            prev = Some(n);
            cur = self.nodes[n as usize].next;
        }
        (bucket, Err(prev))
    }

    /// `node->next = *n; *n = node;`
    fn link(&mut self, bucket: u32, prev: Option<u32>, node: u32) {
        let old = match prev {
            None => self.table[bucket as usize],
            Some(p) => self.nodes[p as usize].next,
        };
        self.nodes[node as usize].next = old;
        match prev {
            None => self.table[bucket as usize] = Some(node),
            Some(p) => self.nodes[p as usize].next = Some(node),
        }
    }

    /// `_hashtable_insert_node(h, node, 0, 0, cf)` during a rehash.
    fn insert_node(&mut self, node: u32, collide: bool) {
        let key = self.nodes[node as usize].key;
        match self.find(key) {
            (_, Ok(n)) => {
                let v = self.nodes[node as usize].value;
                let nv = &mut self.nodes[n as usize];
                nv.key = key;
                if collide { nv.value += v } else { nv.value = v }
            }
            (bucket, Err(prev)) => {
                self.link(bucket, prev, node);
                self.count += 1;
            }
        }
    }

    fn rehash(&mut self, new_size: u32, collide: bool) {
        let old = std::mem::replace(&mut self.table, vec![None; new_size as usize]);
        self.count = 0;
        for head in old {
            let mut cur = head;
            while let Some(n) = cur {
                cur = self.nodes[n as usize].next;
                self.insert_node(n, collide);
            }
        }
    }

    fn resize(&mut self) {
        let old = self.len();
        let mut new = old;
        if self.count * RESIZE_FACTOR < old {
            new = find_prime(old / 2 - 1, -1);
        } else if old * RESIZE_FACTOR < self.count {
            new = find_prime(old * 2 + 1, 1);
        }
        if new < MIN_LENGTH {
            new = old;
        }
        if new != old {
            self.rehash(new, false);
        }
    }

    /// `hashtable_insert_or_update_computed(h, key, new_count_func, exists_count_func)`.
    fn count_pixel(&mut self, key: Px) {
        match self.find(key) {
            (_, Ok(n)) => self.nodes[n as usize].value += 1,
            (bucket, Err(prev)) => {
                self.nodes.push(Node { next: None, key, value: 1 });
                let n = (self.nodes.len() - 1) as u32;
                self.link(bucket, prev, n);
                self.count += 1;
                self.resize();
            }
        }
    }

    /// `hashtable_insert(h, key, val)`.
    fn insert(&mut self, key: Px, value: u32) {
        match self.find(key) {
            (_, Ok(n)) => self.nodes[n as usize].value = value,
            (bucket, Err(prev)) => {
                self.nodes.push(Node { next: None, key, value });
                let n = (self.nodes.len() - 1) as u32;
                self.link(bucket, prev, n);
                self.count += 1;
                self.resize();
            }
        }
    }

    fn lookup(&self, key: Px) -> Option<u32> {
        self.find(key).1.ok().map(|n| self.nodes[n as usize].value)
    }

    /// `hashtable_foreach` order.
    fn entries(&self) -> Vec<(Px, u32)> {
        let mut v = Vec::with_capacity(self.count as usize);
        for head in &self.table {
            let mut cur = *head;
            while let Some(n) = cur {
                let node = &self.nodes[n as usize];
                v.push((node.key, node.value));
                cur = node.next;
            }
        }
        v
    }
}

/// `create_pixel_hash`.
fn create_pixel_hash(pixels: &[Px]) -> PixelHash {
    let mut h = PixelHash::new();
    for &p in pixels {
        h.count_pixel(p);
        while h.count > MAX_HASH_ENTRIES {
            h.scale += 1;
            let len = h.len();
            h.rehash(len, true);
        }
    }
    h
}

const NIL: u32 = u32::MAX;

struct PixelList {
    next: [u32; 3],
    prev: [u32; 3],
    p: Px,
    flag: bool,
    count: u32,
}

struct Lists {
    n: Vec<PixelList>,
}

impl Lists {
    /// `mergesort_pixels(head, i)`: descending by channel i; ties take from the second half.
    fn mergesort(&mut self, head: u32, i: usize) -> u32 {
        if head == NIL || self.n[head as usize].next[i] == NIL {
            if head != NIL {
                self.n[head as usize].next[i] = NIL;
                self.n[head as usize].prev[i] = NIL;
            }
            return head;
        }
        let (mut c, mut t) = (head, head);
        while c != NIL && t != NIL {
            c = self.n[c as usize].next[i];
            let tn = self.n[t as usize].next[i];
            t = if tn != NIL { self.n[tn as usize].next[i] } else { NIL };
        }
        if c != NIL {
            let cp = self.n[c as usize].prev[i];
            if cp != NIL {
                self.n[cp as usize].next[i] = NIL;
            }
            self.n[c as usize].prev[i] = NIL;
        }
        let mut a = self.mergesort(head, i);
        let mut b = self.mergesort(c, i);
        let (mut head, mut p) = (NIL, NIL);
        let mut c = NIL;
        while a != NIL && b != NIL {
            if self.n[a as usize].p[i] > self.n[b as usize].p[i] {
                c = a;
                a = self.n[a as usize].next[i];
            } else {
                c = b;
                b = self.n[b as usize].next[i];
            }
            self.n[c as usize].prev[i] = p;
            self.n[c as usize].next[i] = NIL;
            if p != NIL {
                self.n[p as usize].next[i] = c;
            }
            p = c;
            if head == NIL {
                head = c;
            }
        }
        if a != NIL {
            self.n[c as usize].next[i] = a;
            self.n[a as usize].prev[i] = c;
        } else if b != NIL {
            self.n[c as usize].next[i] = b;
            self.n[b as usize].prev[i] = c;
        }
        head
    }
}

struct BoxNode {
    l: usize,
    r: usize,
    head: [u32; 3],
    tail: [u32; 3],
    volume: i32,
    pixel_count: u32,
}

const NOBOX: usize = usize::MAX;

struct Cut {
    lists: Lists,
    boxes: Vec<BoxNode>,
}

impl Cut {
    fn ch(&self, n: u32, i: usize) -> i32 {
        self.lists.n[n as usize].p[i] as i32
    }

    fn volume(&mut self, b: usize) -> i32 {
        if self.boxes[b].volume >= 0 {
            return self.boxes[b].volume;
        }
        let bx = &self.boxes[b];
        let v = if bx.head[0] == NIL {
            0
        } else {
            let (h, t) = (bx.head, bx.tail);
            (self.ch(h[0], 0) - self.ch(t[0], 0) + 1)
                * (self.ch(h[1], 1) - self.ch(t[1], 1) + 1)
                * (self.ch(h[2], 2) - self.ch(t[2], 2) + 1)
        };
        self.boxes[b].volume = v;
        v
    }

    /// `splitlists`: returns (heads, tails, counts) of the left/right halves.
    fn splitlists(&mut self, h: [u32; 3], t: [u32; 3], axis: usize, pixel_count: u32) -> ([[u32; 3]; 2], [[u32; 3]; 2], [u32; 2]) {
        let mut ncount = [0u32; 2];
        let n = &mut self.lists.n;
        let mut left: u32 = 0;
        let mut c = h[axis];
        while c != NIL {
            left = left.wrapping_add(n[c as usize].count);
            ncount[0] += n[c as usize].count;
            n[c as usize].flag = false;
            c = n[c as usize].next[axis];
            if left.wrapping_mul(2) > pixel_count {
                break;
            }
        }
        if c != NIL {
            let split_val = n[n[c as usize].prev[axis] as usize].p[axis];
            while c != NIL {
                if split_val != n[c as usize].p[axis] {
                    break;
                }
                n[c as usize].flag = false;
                ncount[0] += n[c as usize].count;
                c = n[c as usize].next[axis];
            }
        }
        let mut n_right = 0;
        while c != NIL {
            n[c as usize].flag = true;
            n_right += 1;
            ncount[1] += n[c as usize].count;
            c = n[c as usize].next[axis];
        }
        if n_right == 0 {
            let split_val = n[t[axis] as usize].p[axis];
            let mut c = t[axis];
            while c != NIL {
                if split_val != n[c as usize].p[axis] {
                    break;
                }
                n[c as usize].flag = true;
                ncount[0] -= n[c as usize].count;
                ncount[1] += n[c as usize].count;
                c = n[c as usize].prev[axis];
            }
        }
        let mut nh = [[NIL; 3]; 2];
        let mut nt = [[NIL; 3]; 2];
        for i in 0..3 {
            let (mut l, mut r) = (NIL, NIL);
            let mut c = h[i];
            while c != NIL {
                let next = n[c as usize].next[i];
                if n[c as usize].flag {
                    if r != NIL { n[r as usize].next[i] = c } else { nh[1][i] = c }
                    n[c as usize].prev[i] = r;
                    r = c;
                } else {
                    if l != NIL { n[l as usize].next[i] = c } else { nh[0][i] = c }
                    n[c as usize].prev[i] = l;
                    l = c;
                }
                c = next;
            }
            if l != NIL {
                n[l as usize].next[i] = NIL;
            }
            if r != NIL {
                n[r as usize].next[i] = NIL;
            }
            nt[0][i] = l;
            nt[1][i] = r;
        }
        (nh, nt, ncount)
    }

    /// `split(node)`.
    fn split(&mut self, b: usize) {
        let (h, t) = (self.boxes[b].head, self.boxes[b].tail);
        let f = [
            (self.ch(h[0], 0) - self.ch(t[0], 0)) * 77,
            (self.ch(h[1], 1) - self.ch(t[1], 1)) * 150,
            (self.ch(h[2], 2) - self.ch(t[2], 2)) * 29,
        ];
        let (mut best, mut axis) = (f[0], 0);
        for (i, &v) in f.iter().enumerate().skip(1) {
            if best < v {
                best = v;
                axis = i;
            }
        }
        let pc = self.boxes[b].pixel_count;
        let (heads, tails, counts) = self.splitlists(h, t, axis, pc);
        let mk = |k: usize| BoxNode { l: NOBOX, r: NOBOX, head: heads[k], tail: tails[k], volume: -1, pixel_count: counts[k] };
        self.boxes.push(mk(0));
        self.boxes.push(mk(1));
        let n = self.boxes.len();
        let node = &mut self.boxes[b];
        node.head = [NIL; 3];
        node.tail = [NIL; 3];
        node.l = n - 2;
        node.r = n - 1;
    }

    /// `median_cut(hl, imPixelCount, nPixels)` with QuantHeap (max-heap on pixelCount).
    fn median_cut(&mut self, hl: [u32; 3], im_pixel_count: u32, n_colors: u32) -> usize {
        let mut tl = [NIL; 3];
        for i in 0..3 {
            let mut c = hl[i];
            while c != NIL && self.lists.n[c as usize].next[i] != NIL {
                c = self.lists.n[c as usize].next[i];
            }
            tl[i] = c;
        }
        self.boxes.push(BoxNode { l: NOBOX, r: NOBOX, head: hl, tail: tl, volume: -1, pixel_count: im_pixel_count });
        let root = 0;
        let mut heap: Vec<usize> = vec![NOBOX]; // 1-indexed
        let cmp = |boxes: &Vec<BoxNode>, a: usize, b: usize| boxes[a].pixel_count as i32 - boxes[b].pixel_count as i32;
        let add = |heap: &mut Vec<usize>, boxes: &Vec<BoxNode>, val: usize| {
            heap.push(val);
            let mut k = heap.len() - 1;
            while k != 1 {
                if cmp(boxes, val, heap[k / 2]) <= 0 {
                    break;
                }
                heap[k] = heap[k / 2];
                k >>= 1;
            }
            heap[k] = val;
        };
        let remove = |heap: &mut Vec<usize>, boxes: &Vec<BoxNode>| -> Option<usize> {
            if heap.len() <= 1 {
                return None;
            }
            let r = heap[1];
            let v = heap.pop().unwrap();
            let count = heap.len() - 1;
            if count == 0 {
                return Some(r);
            }
            let mut k = 1;
            while k * 2 <= count {
                let mut l = k * 2;
                if l < count && cmp(boxes, heap[l], heap[l + 1]) < 0 {
                    l += 1;
                }
                if cmp(boxes, v, heap[l]) > 0 {
                    break;
                }
                heap[k] = heap[l];
                k = l;
            }
            heap[k] = v;
            Some(r)
        };
        add(&mut heap, &self.boxes, root);
        let mut n = n_colors;
        loop {
            n -= 1;
            if n == 0 {
                break;
            }
            let this = loop {
                match remove(&mut heap, &self.boxes) {
                    None => return root,
                    Some(b) if self.volume(b) == 1 => continue,
                    Some(b) => break b,
                }
            };
            self.split(this);
            let (l, r) = (self.boxes[this].l, self.boxes[this].r);
            add(&mut heap, &self.boxes, l);
            add(&mut heap, &self.boxes, r);
        }
        root
    }

    /// `annotate_hash_table`: leaf boxes left to right get palette indices.
    fn annotate(&self, b: usize, h: &mut PixelHash, next_box: &mut u32) {
        let node = &self.boxes[b];
        if node.l != NOBOX && node.r != NOBOX {
            self.annotate(node.l, h, next_box);
            self.annotate(node.r, h, next_box);
            return;
        }
        let mut p = node.head[0];
        while p != NIL {
            let s = h.scale;
            let q = self.lists.n[p as usize].p.map(|v| ((v as u32) << s) as u8);
            h.insert(q, *next_box);
            p = self.lists.n[p as usize].next[0];
        }
        if node.head[0] != NIL {
            *next_box += 1;
        }
    }
}

/// `quantize(pixelData, nPixels, nQuantPixels, ...)` with kmeans = 0: palette and indices.
pub fn mediancut(pixels: &[Px], colors: u32) -> (Vec<Px>, Vec<u8>) {
    let mut h = create_pixel_hash(pixels);
    // hash_to_list: prepend in foreach order (all three lists identical before sorting).
    let entries = h.entries();
    let scale = h.scale;
    let mut lists = Lists { n: Vec::with_capacity(entries.len()) };
    let mut head = NIL;
    for (key, count) in entries {
        let idx = lists.n.len() as u32;
        lists.n.push(PixelList { next: [head; 3], prev: [NIL; 3], p: key.map(|v| v >> scale), flag: false, count });
        if head != NIL {
            for i in 0..3 {
                lists.n[head as usize].prev[i] = idx;
            }
        }
        head = idx;
    }
    let mut hl = [head; 3];
    for (i, h) in hl.iter_mut().enumerate() {
        *h = lists.mergesort(*h, i);
    }
    let mut cut = Cut { lists, boxes: vec![] };
    let root = cut.median_cut(hl, pixels.len() as u32, colors);
    let mut n_entries = 0;
    cut.annotate(root, &mut h, &mut n_entries);

    // compute_palette_from_median_cut
    let n = n_entries as usize;
    let (mut avg, mut count) = (vec![[0u32; 3]; n], vec![0u32; n]);
    let mut box_of: HashMap<Px, u32> = HashMap::new();
    for &p in pixels {
        let e = *box_of.entry(p).or_insert_with(|| h.lookup(p).expect("pixel in median-cut hash"));
        for c in 0..3 {
            avg[e as usize][c] += p[c] as u32;
        }
        count[e as usize] += 1;
    }
    let palette: Vec<Px> = (0..n)
        .map(|i| [0, 1, 2].map(|c| (0.5 + avg[i][c] as f64 / count[i] as f64) as i32 as u8))
        .collect();

    // build_distance_tables: per row, entries sorted by (distance, index).
    let mut dist = vec![0u32; n * n];
    for i in 0..n {
        for j in 0..i {
            let d = dist_sqr(palette[i], palette[j]);
            dist[i * n + j] = d;
            dist[j * n + i] = d;
        }
    }
    let mut order = vec![0u32; n * n];
    for i in 0..n {
        let row = &mut order[i * n..(i + 1) * n];
        for (j, o) in row.iter_mut().enumerate() {
            *o = j as u32;
        }
        row.sort_by_key(|&j| (dist[i * n + j as usize], j));
    }

    // map_image_pixels_from_median_box
    let mut cache: HashMap<Px, u8> = HashMap::new();
    let indices = pixels
        .iter()
        .map(|&p| {
            *cache.entry(p).or_insert_with(|| {
                let pv = *box_of.get(&p).unwrap() as usize;
                let mut best = dist_sqr(palette[pv], p);
                let mut bestmatch = pv;
                let initial = best << 2;
                for &idx in &order[pv * n..(pv + 1) * n] {
                    if dist[pv * n + idx as usize] <= initial {
                        let d = dist_sqr(palette[idx as usize], p);
                        if d < best {
                            best = d;
                            bestmatch = idx as usize;
                        }
                    } else {
                        break;
                    }
                }
                bestmatch as u8
            })
        })
        .collect();
    (palette, indices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_prime_quirk() {
        // Next value up from 2*11+1 whose low nibble is in {1,3,7,9,11,13}.
        assert_eq!(find_prime(23, 1), 23);
        assert_eq!(find_prime(24, 1), 25);
        assert_eq!(find_prime(4, -1), 3);
    }

    /// Fixture from Pillow 12.3: tests/fixtures/quant_*.png (see README there).
    #[test]
    fn mediancut_matches_pillow() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        for name in ["quant_gradient", "quant_tile"] {
            let src = crate::image::decode_png(&std::fs::read(format!("{dir}/{name}_src.png")).unwrap()).unwrap().0;
            let want = std::fs::read_to_string(format!("{dir}/{name}_pillow.txt")).unwrap();
            let px: Vec<Px> = src.px.as_chunks::<4>().0.iter().map(|c| [c[0], c[1], c[2]]).collect();
            let (pal, idx) = mediancut(&px, 255);
            let mut got = String::new();
            for p in &pal {
                got.push_str(&format!("{} {} {}\n", p[0], p[1], p[2]));
            }
            got.push_str("--\n");
            for chunk in idx.chunks(src.w as usize) {
                got.push_str(&chunk.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(" "));
                got.push('\n');
            }
            assert!(got == want, "{name}: palette/indices differ from Pillow");
        }
    }
}
