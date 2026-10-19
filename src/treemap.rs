//! Squarified, cushion-shaded treemap rendering.
//!
//! Layout follows Bruls, Huizing & van Wijk ("Squarified Treemaps"), shading
//! follows van Wijk & van de Wetering ("Cushion Treemaps"), the technique
//! used by SequoiaView, WinDirStat and WizTree. Rendering produces an RGB
//! image plus a per-pixel index of the item drawn there, so hit testing is a
//! single array lookup. It runs on a worker thread and never touches GTK.

use crate::settings::TreemapStyle;
use crate::tree::{ExtId, ExtStat, NodeId, SizeMode, Tree};
use std::sync::Arc;

/// Initial cushion height and how it shrinks per nesting level.
const HEIGHT: f64 = 0.38;
const HEIGHT_SCALE: f64 = 0.91;
const AMBIENT: f64 = 0.26;
const DIFFUSE: f64 = 0.82;
// Light from the top left, mostly from the front.
const LX: f64 = -0.098_533_9;
const LY: f64 = -0.098_533_9;
const LZ: f64 = 0.985_339;

/// Distinct colours handed out to the extensions using the most space.
pub const PALETTE: [[u8; 3]; 20] = [
    [0x35, 0x84, 0xe4], // blue
    [0xe6, 0x61, 0x00], // orange
    [0x2e, 0xc2, 0x7e], // green
    [0xe0, 0x1b, 0x24], // red
    [0x91, 0x41, 0xac], // purple
    [0xf5, 0xc2, 0x11], // yellow
    [0x1c, 0xb5, 0xa9], // teal
    [0xc0, 0x61, 0xcb], // pink
    [0x98, 0x6a, 0x44], // brown
    [0x62, 0xa0, 0xea], // light blue
    [0xff, 0xa3, 0x48], // light orange
    [0x57, 0xe3, 0x89], // light green
    [0xf6, 0x61, 0x51], // salmon
    [0xdc, 0x8a, 0xdd], // lilac
    [0xf8, 0xe4, 0x5c], // light yellow
    [0x26, 0xa2, 0x69], // dark green
    [0x1a, 0x5f, 0xb4], // dark blue
    [0xa5, 0x1d, 0x2d], // dark red
    [0x61, 0x35, 0x83], // dark purple
    [0x86, 0x5e, 0x3c], // dark brown
];

/// Colour of every extension outside the top group.
pub const OTHER_COLOR: [u8; 3] = [0x9a, 0x99, 0x96];
/// Colour of empty folders and unreadable areas.
pub const DIR_COLOR: [u8; 3] = [0x77, 0x76, 0x7b];

/// Assigns palette colours to the largest extensions; returns one colour per
/// extension id.
pub fn ext_colors(stats: &[ExtStat], ext_count: usize) -> Vec<[u8; 3]> {
    let mut colors = vec![OTHER_COLOR; ext_count.max(1)];
    for (stat, color) in stats.iter().zip(PALETTE) {
        if let Some(c) = colors.get_mut(stat.ext as usize) {
            *c = color;
        }
    }
    colors
}

#[derive(Clone)]
pub struct Params {
    pub root: NodeId,
    pub width: u32,
    pub height: u32,
    pub mode: SizeMode,
    pub colors: Arc<Vec<[u8; 3]>>,
    /// Draw only files of this extension in colour.
    pub highlight: Option<ExtId>,
    pub style: TreemapStyle,
    pub dark: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Leaf {
    pub node: NodeId,
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

pub struct Rendered {
    pub root: NodeId,
    pub width: u32,
    pub height: u32,
    /// Packed RGB, `width * 3` bytes per row.
    pub pixels: Vec<u8>,
    /// Index into `leaves` for every pixel (`u32::MAX` for none).
    index: Vec<u32>,
    pub leaves: Vec<Leaf>,
    /// Every laid out node with its exact rectangle, sorted by node id.
    rects: Vec<(NodeId, [f32; 4])>,
}

impl Rendered {
    /// Index into `leaves` of the item drawn at a pixel.
    pub fn leaf_index_at(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = self.index[y as usize * self.width as usize + x as usize];
        (i != u32::MAX).then_some(i as usize)
    }

    pub fn leaf_at(&self, x: u32, y: u32) -> Option<&Leaf> {
        self.leaf_index_at(x, y).and_then(|i| self.leaves.get(i))
    }

    /// Rectangle (x0, y0, x1, y1) of a node in image pixels, if visible.
    pub fn rect_of(&self, node: NodeId) -> Option<[f32; 4]> {
        self.rects.binary_search_by_key(&node, |r| r.0).ok().map(|i| self.rects[i].1)
    }
}

#[derive(Clone, Copy)]
struct Rect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

struct Renderer<'a> {
    tree: &'a Tree,
    p: &'a Params,
    out: Rendered,
    visited: usize,
    stale: &'a dyn Fn() -> bool,
    aborted: bool,
}

/// Renders the treemap of `p.root`. Returns `None` if `stale()` reported
/// that the result is no longer needed.
pub fn render(tree: &Tree, p: &Params, stale: &dyn Fn() -> bool) -> Option<Rendered> {
    let (w, h) = (p.width.max(1), p.height.max(1));
    let n = (w as usize) * (h as usize);
    let bg = if p.dark { 0x24 } else { 0xfa };
    let mut r = Renderer {
        tree,
        p,
        out: Rendered {
            root: p.root,
            width: w,
            height: h,
            pixels: vec![bg; n * 3],
            index: vec![u32::MAX; n],
            leaves: Vec::new(),
            rects: Vec::new(),
        },
        visited: 0,
        stale,
        aborted: false,
    };
    let full = Rect { x0: 0.0, y0: 0.0, x1: w as f64, y1: h as f64 };
    if tree.size(p.root, p.mode) > 0 || !tree.is_dir(p.root) {
        r.node(p.root, full, [0.0; 4], HEIGHT, true);
    }
    if r.aborted {
        return None;
    }
    r.out.rects.sort_unstable_by_key(|r| r.0);
    Some(r.out)
}

fn add_ridge(r: &Rect, s: &mut [f64; 4], h: f64) {
    let w = r.x1 - r.x0;
    let hh = r.y1 - r.y0;
    if w > 0.0 {
        let f = 4.0 * h / w;
        s[2] += f * (r.x1 + r.x0);
        s[0] -= f;
    }
    if hh > 0.0 {
        let f = 4.0 * h / hh;
        s[3] += f * (r.y1 + r.y0);
        s[1] -= f;
    }
}

fn px(v: f64, max: u32) -> u32 {
    (v.round().max(0.0) as u32).min(max)
}

impl Renderer<'_> {
    fn node(&mut self, id: NodeId, r: Rect, surface: [f64; 4], h: f64, is_root: bool) {
        let (w, hgt) = (self.out.width, self.out.height);
        let (x0, y0, x1, y1) = (px(r.x0, w), px(r.y0, hgt), px(r.x1, w), px(r.y1, hgt));
        if x1 <= x0 || y1 <= y0 || self.aborted {
            return;
        }
        self.visited += 1;
        if self.visited % 4096 == 0 && (self.stale)() {
            self.aborted = true;
            return;
        }
        self.out.rects.push((id, [r.x0 as f32, r.y0 as f32, r.x1 as f32, r.y1 as f32]));

        let mut s = surface;
        if !is_root {
            add_ridge(&r, &mut s, h);
        }
        let tree = self.tree;
        let tiny = x1 - x0 < 3 || y1 - y0 < 3;
        if tree.is_dir(id) && !tiny {
            let mode = self.p.mode;
            let mut kids: Vec<(u64, NodeId)> = tree
                .live_children(id)
                .filter_map(|c| {
                    let size = tree.counted_size(c, mode);
                    (size > 0).then_some((size, c))
                })
                .collect();
            if !kids.is_empty() {
                kids.sort_unstable_by_key(|k| std::cmp::Reverse(k.0));
                let total = kids.iter().fold(0u64, |sum, k| sum.saturating_add(k.0));
                self.squarify(&kids, r, total, s, h * HEIGHT_SCALE);
                return;
            }
        }
        self.leaf(id, x0, y0, x1, y1, &s);
    }

    fn squarify(&mut self, kids: &[(u64, NodeId)], rect: Rect, total: u64, s: [f64; 4], h: f64) {
        let mut r = rect;
        let mut remaining = total as f64;
        let mut i = 0;
        while i < kids.len() && !self.aborted {
            let w = r.x1 - r.x0;
            let hgt = r.y1 - r.y0;
            if w <= 0.0 || hgt <= 0.0 || remaining <= 0.0 {
                return;
            }
            // Nothing below can cover a pixel any more.
            if r.x1.round() <= r.x0.round() || r.y1.round() <= r.y0.round() {
                return;
            }
            // Lay out a column along the left edge if the space is wider
            // than tall, otherwise a row along the top.
            let vertical = w >= hgt;
            let side = if vertical { hgt } else { w };
            let scale = (w * hgt) / remaining;
            let side2 = side * side;

            let largest = kids[i].0 as f64 * scale;
            let mut j = i;
            let mut row_sum = 0.0;
            let mut worst = f64::INFINITY;
            while j < kids.len() {
                let area = kids[j].0 as f64 * scale;
                let row_area = (row_sum + kids[j].0 as f64) * scale;
                let ratio = (side2 * largest / (row_area * row_area)).max(row_area * row_area / (side2 * area));
                if j > i && ratio > worst {
                    break;
                }
                worst = ratio;
                row_sum += kids[j].0 as f64;
                j += 1;
            }

            let last_row = j == kids.len();
            let edge = if last_row {
                if vertical {
                    r.x1
                } else {
                    r.y1
                }
            } else {
                let thickness = row_sum * scale / side;
                if vertical {
                    (r.x0 + thickness).min(r.x1)
                } else {
                    (r.y0 + thickness).min(r.y1)
                }
            };
            let mut pos = if vertical { r.y0 } else { r.x0 };
            let end = if vertical { r.y1 } else { r.x1 };
            let row = &kids[i..j];
            for (n, &(size, child)) in row.iter().enumerate() {
                let next = if n + 1 == row.len() { end } else { pos + side * (size as f64 / row_sum) };
                let cr = if vertical {
                    Rect { x0: r.x0, y0: pos, x1: edge, y1: next }
                } else {
                    Rect { x0: pos, y0: r.y0, x1: next, y1: edge }
                };
                self.node(child, cr, s, h, false);
                pos = next;
            }
            if vertical {
                r.x0 = edge;
            } else {
                r.y0 = edge;
            }
            remaining -= row_sum;
            i = j;
        }
    }

    /// Colour of a node drawn as a single block: files use their extension
    /// colour; folders too small to subdivide use their largest file's.
    fn color_of(&self, id: NodeId) -> ([u8; 3], bool) {
        let tree = self.tree;
        let mut cur = id;
        for _ in 0..64 {
            if !tree.is_dir(cur) {
                let ext = tree.ext(cur);
                let color = self.p.colors.get(ext as usize).copied().unwrap_or(OTHER_COLOR);
                let dim = self.p.highlight.is_some_and(|hl| hl != ext);
                return (color, dim);
            }
            let mode = self.p.mode;
            match tree.live_children(cur).max_by_key(|&c| tree.counted_size(c, mode)) {
                Some(c) if tree.counted_size(c, mode) > 0 => cur = c,
                _ => break,
            }
        }
        (DIR_COLOR, self.p.highlight.is_some())
    }

    fn leaf(&mut self, id: NodeId, x0: u32, y0: u32, x1: u32, y1: u32, s: &[f64; 4]) {
        let leaf_idx = self.out.leaves.len() as u32;
        self.out.leaves.push(Leaf { node: id, x0, y0, x1, y1 });
        let (color, dim) = self.color_of(id);
        let mut base = color.map(|c| c as f64 / 255.0);
        if dim {
            let gray = 0.299 * base[0] + 0.587 * base[1] + 0.114 * base[2];
            let v = if self.p.dark { gray * 0.25 + 0.10 } else { gray * 0.25 + 0.62 };
            base = [v, v, v];
        }
        let cushion = self.p.style == TreemapStyle::Cushion;
        let width = self.out.width as usize;
        let edges = x1 - x0 > 2 && y1 - y0 > 2;
        for y in y0..y1 {
            let ny = -(2.0 * s[1] * (y as f64 + 0.5) + s[3]);
            let row = y as usize * width;
            for x in x0..x1 {
                let shade = if cushion {
                    let nx = -(2.0 * s[0] * (x as f64 + 0.5) + s[2]);
                    let cosa = (nx * LX + ny * LY + LZ) / (nx * nx + ny * ny + 1.0).sqrt();
                    AMBIENT + DIFFUSE * cosa.max(0.0)
                } else if edges && (x == x0 || y == y0) {
                    0.72
                } else {
                    1.0
                };
                let i = row + x as usize;
                let p = &mut self.out.pixels[i * 3..i * 3 + 3];
                for c in 0..3 {
                    p[c] = (base[c] * shade * 255.0).round().clamp(0.0, 255.0) as u8;
                }
                self.out.index[i] = leaf_idx;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Kind, NodeData, ScanInfo, TreeBuilder, NONE};
    use std::path::PathBuf;

    fn file(size: u64) -> NodeData {
        NodeData { kind: Kind::File, flags: 0, size, alloc: size, mtime: 0, files: 0, dirs: 0 }
    }

    fn sample(n: u32) -> Tree {
        let mut b = TreeBuilder::with_capacity(n as usize + 2, 1024);
        let total: u64 = (1..=n as u64).map(|i| i * i).sum::<u64>() + 1000;
        let root = b.push(NONE, b"/", NodeData { kind: Kind::Dir, files: n + 1, dirs: 1, ..file(total) });
        let first = b.len() as NodeId;
        for i in (1..=n).rev() {
            b.push(root, format!("f{i}.dat").as_bytes(), file((i as u64) * (i as u64)));
        }
        let sub = b.push(root, b"sub", NodeData { kind: Kind::Dir, files: 1, ..file(1000) });
        b.set_children(root, first, n + 1);
        let f = b.push(sub, b"inner.txt", file(1000));
        b.set_children(sub, f, 1);
        b.finish(PathBuf::from("/"), ScanInfo::default())
    }

    fn params(w: u32, h: u32, style: TreemapStyle) -> Params {
        Params {
            root: 0,
            width: w,
            height: h,
            mode: SizeMode::Logical,
            colors: Arc::new(vec![[200, 100, 50]; 8]),
            highlight: None,
            style,
            dark: false,
        }
    }

    #[test]
    fn every_pixel_is_covered_exactly_once() {
        let tree = sample(300);
        for (w, h) in [(640, 480), (97, 1013), (1, 1), (3, 500)] {
            let out = render(&tree, &params(w, h, TreemapStyle::Cushion), &|| false).unwrap();
            assert!(out.index.iter().all(|&i| i != u32::MAX), "gap in {w}x{h}");
            // Leaf rectangles must not overlap: their areas add up exactly.
            let area: u64 = out.leaves.iter().map(|l| ((l.x1 - l.x0) * (l.y1 - l.y0)) as u64).sum();
            assert_eq!(area, w as u64 * h as u64);
        }
    }

    #[test]
    fn areas_follow_sizes() {
        let tree = sample(10);
        let out = render(&tree, &params(1000, 1000, TreemapStyle::Flat), &|| false).unwrap();
        let total = tree.logical_size(0) as f64;
        for id in tree.children(0) {
            let r = out.rect_of(id).unwrap();
            let area = ((r[2] - r[0]) * (r[3] - r[1])) as f64;
            let expected = tree.logical_size(id) as f64 / total * 1e6;
            assert!((area - expected).abs() / expected < 1e-3, "{area} vs {expected}");
        }
        let leaf = out.leaf_at(999, 999).unwrap();
        assert!(leaf.x1 == 1000 && leaf.y1 == 1000);
        assert!(out.rect_of(0).is_some());
    }

    #[test]
    fn stale_render_is_abandoned() {
        let tree = sample(20000);
        assert!(render(&tree, &params(2000, 2000, TreemapStyle::Cushion), &|| true).is_none());
    }
}
