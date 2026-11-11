//! rowser-text: TrueType font loading and glyph rasterization, from scratch.
//!
//! Parses sfnt (TTF) files: `cmap` (formats 4 & 12), `head`, `hhea`, `hmtx`,
//! `maxp`, `loca`, `glyf` (simple + single-level composite glyphs), `name`.
//! Rasterizes quadratic outlines with a non-zero winding scanline fill at
//! 4x4 supersampling. CFF/OTF outlines and hinting are not supported.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

// ---------- font discovery ----------

/// A font file found on the system, with enough metadata to select it.
#[derive(Debug, Clone)]
pub struct FontFace {
    pub family: String,
    /// 0=regular, 1=bold
    pub bold: bool,
    pub italic: bool,
    pub path: PathBuf,
    /// CSS weight 100–900 (scanned faces: 400, or 700 when `bold`).
    pub weight: u16,
    /// font-stretch percentage; 100 = normal width.
    pub stretch: u16,
    /// unicode-range restriction (@font-face faces); empty = unrestricted.
    pub unicode_ranges: Vec<(u32, u32)>,
    /// registered from a web font / local() alias — outranks a fuzzy
    /// substring hit on a scanned system face.
    pub registered: bool,
    /// deactivated by `clear_registered` (navigation) — never selected.
    pub disabled: bool,
}

/// Scan a directory (non-recursively) for .ttf/.otf files and read their
/// `name` tables. Fonts without a `glyf` table (CFF .otf) are skipped.
pub fn scan_dir(dir: &Path) -> Vec<FontFace> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_font = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("ttf"));
        if !is_font {
            continue;
        }
        if let Some(face) = inspect(&path) {
            out.push(face);
        }
    }
    out
}

/// Scan the usual Linux font directories.
pub fn scan_system() -> Vec<FontFace> {
    let mut out = Vec::new();
    for dir in [
        "/usr/share/fonts",
        "/usr/local/share/fonts",
        "~/.local/share/fonts",
        "~/.fonts",
    ] {
        let dir = if let Some(rest) = dir.strip_prefix("~/") {
            dirs_home().join(rest)
        } else {
            PathBuf::from(dir)
        };
        // One level of subdirectories (fonts are grouped by family).
        if let Ok(entries) = fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.extend(scan_dir(&p));
                } else {
                    out.extend(scan_dir(&dir));
                    break;
                }
            }
        }
    }
    out
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME").map_or_else(|_| PathBuf::from("/"), PathBuf::from)
}

fn inspect(path: &Path) -> Option<FontFace> {
    let data = fs::read(path).ok()?;
    let font = Font::parse(&data)?;
    let (family, bold, italic) = font.names()?;
    Some(FontFace {
        family,
        bold,
        italic,
        path: path.to_path_buf(),
        weight: if bold { 700 } else { 400 },
        stretch: 100,
        unicode_ranges: Vec::new(),
        registered: false,
        disabled: false,
    })
}

/// Pick the best face for (family-ish, bold, italic) from a scanned set.
/// Family matching is a case-insensitive substring check.
pub fn select<'a>(
    faces: &'a [FontFace],
    family: &str,
    bold: bool,
    italic: bool,
) -> Option<&'a FontFace> {
    let want = family.to_lowercase();
    let mut best: Option<(&FontFace, u32)> = None;
    for f in faces {
        if !f.family.to_lowercase().contains(&want) {
            continue;
        }
        let mut score = 0u32;
        if f.bold == bold {
            score += 4;
        }
        if f.italic == italic {
            score += 2;
        }
        score += 1;
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((f, score));
        }
    }
    best.map(|(f, _)| f)
}

// ---------- TTF parsing ----------

struct Tables {
    head: usize,
    hhea: usize,
    maxp: usize,
    loca: usize,
    glyf: usize,
    hmtx: usize,
    cmap: usize,
    name: usize,
    kern: Option<usize>,
}

/// A parsed TrueType font. Keeps the raw file bytes and resolves tables
/// on demand — parsing is zero-copy apart from `loca`/`hmtx` caches.
pub struct Font {
    data: &'static [u8],
    tables: Tables,
    units_per_em: u16,
    num_glyphs: u16,
    num_h_metrics: u16,
    loca_long: bool,
    pub ascender: i16,
    pub descender: i16,
    pub line_gap: i16,
    // caches filled lazily
    loca: Vec<u32>,
    hmtx: Vec<(u16, i16)>, // (advance, lsb)
    cmap4: Option<usize>,  // offset of format-4 subtable
    cmap12: Option<usize>, // offset of format-12 subtable
    kern_pairs: HashMap<u32, i16>,
    kern_loaded: bool,
    /// swash handle over leaked data for hinted rasterization
    swash: Option<swash::FontRef<'static>>,
}

fn be_u16(d: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([d[o], d[o + 1]])
}
fn be_i16(d: &[u8], o: usize) -> i16 {
    i16::from_be_bytes([d[o], d[o + 1]])
}
fn be_u32(d: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

impl Font {
    /// Parse a font file. Returns None for CFF fonts (no `glyf` table) and
    /// malformed data.
    pub fn parse(data: &[u8]) -> Option<Font> {
        if data.len() < 12 {
            return None;
        }
        let num_tables = be_u16(data, 4) as usize;
        let mut tables = Tables {
            head: 0,
            hhea: 0,
            maxp: 0,
            loca: 0,
            glyf: 0,
            hmtx: 0,
            cmap: 0,
            name: 0,
            kern: None,
        };
        for i in 0..num_tables {
            let rec = 12 + i * 16;
            if rec + 16 > data.len() {
                return None;
            }
            let tag = &data[rec..rec + 4];
            let off = be_u32(data, rec + 8) as usize;
            if off + 4 > data.len() {
                continue;
            }
            match tag {
                b"head" => tables.head = off,
                b"hhea" => tables.hhea = off,
                b"maxp" => tables.maxp = off,
                b"loca" => tables.loca = off,
                b"glyf" => tables.glyf = off,
                b"hmtx" => tables.hmtx = off,
                b"cmap" => tables.cmap = off,
                b"name" => tables.name = off,
                b"kern" => tables.kern = Some(off),
                _ => {}
            }
        }
        if tables.head == 0 || tables.glyf == 0 || tables.cmap == 0 {
            return None;
        }
        let d = &data;
        let mut font = Font {
            units_per_em: be_u16(d, tables.head + 18),
            loca_long: be_i16(d, tables.head + 50) != 0,
            num_glyphs: be_u16(d, tables.maxp + 4),
            num_h_metrics: be_u16(d, tables.hhea + 34),
            ascender: be_i16(d, tables.hhea + 4),
            descender: be_i16(d, tables.hhea + 6),
            line_gap: be_i16(d, tables.hhea + 8),
            data: {
                // Leak the font bytes once: tables parse from it and swash's
                // FontRef borrows it — fonts live for the process lifetime.
                Box::leak(data.to_vec().into_boxed_slice())
            },
            tables,
            loca: Vec::new(),
            hmtx: Vec::new(),
            cmap4: None,
            cmap12: None,
            kern_pairs: HashMap::new(),
            kern_loaded: false,
            swash: None,
        };
        font.load_loca();
        font.load_hmtx();
        font.find_cmap();
        if font.cmap4.is_none() && font.cmap12.is_none() {
            return None;
        }
        font.swash = swash::FontRef::from_index(font.data, 0);
        Some(font)
    }

    pub fn load(path: &Path) -> Option<Font> {
        Self::parse(&fs::read(path).ok()?)
    }

    fn load_loca(&mut self) {
        let n = self.num_glyphs as usize + 1;
        self.loca = Vec::with_capacity(n);
        let d = &self.data;
        for i in 0..n {
            let v = if self.loca_long {
                be_u32(d, self.tables.loca + i * 4)
            } else {
                u32::from(be_u16(d, self.tables.loca + i * 2)) * 2
            };
            self.loca.push(v);
        }
    }

    fn load_hmtx(&mut self) {
        let d = &self.data;
        let mut adv = 0u16;
        for i in 0..self.num_glyphs as usize {
            if i < self.num_h_metrics as usize {
                let o = self.tables.hmtx + i * 4;
                adv = be_u16(d, o);
                self.hmtx.push((adv, be_i16(d, o + 2)));
            } else {
                let o = self.tables.hmtx
                    + self.num_h_metrics as usize * 4
                    + (i - self.num_h_metrics as usize) * 2;
                self.hmtx.push((adv, be_i16(d, o)));
            }
        }
    }

    fn find_cmap(&mut self) {
        let d = &self.data;
        let base = self.tables.cmap;
        let count = be_u16(d, base + 2) as usize;
        // Prefer format 12 (full Unicode), else format 4 platform 3.
        let mut best4 = None;
        for i in 0..count {
            let rec = base + 4 + i * 8;
            let platform = be_u16(d, rec);
            let encoding = be_u16(d, rec + 2);
            let sub = base + be_u32(d, rec + 4) as usize;
            if sub + 2 > d.len() {
                continue;
            }
            let fmt = be_u16(d, sub);
            match (fmt, platform, encoding) {
                (12, 3, 10) | (12, 0, _) => self.cmap12 = Some(sub),
                (4, 3, 1) | (4, 0, _) | (4, 3, 0) => best4 = Some(sub),
                _ => {}
            }
        }
        self.cmap4 = best4;
    }

    /// Family name plus rough style flags from the `name` table.
    pub fn names(&self) -> Option<(String, bool, bool)> {
        let d = &self.data;
        let base = self.tables.name;
        if base == 0 || base + 6 > d.len() {
            return None;
        }
        let count = be_u16(d, base + 2) as usize;
        let strings = base + be_u16(d, base + 4) as usize;
        let mut family = None;
        let mut subfamily = String::new();
        for i in 0..count {
            let rec = base + 6 + i * 12;
            if rec + 12 > d.len() {
                break;
            }
            let platform = be_u16(d, rec);
            let name_id = be_u16(d, rec + 6);
            let len = be_u16(d, rec + 8) as usize;
            let off = strings + be_u16(d, rec + 10) as usize;
            if off + len > d.len() {
                continue;
            }
            let raw = &d[off..off + len];
            // Windows platform strings are UTF-16BE; others treat as latin1.
            let text = if platform == 3 {
                raw.chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .map(|c| char::from_u32(u32::from(c)).unwrap_or('?'))
                    .collect()
            } else {
                raw.iter().map(|&b| b as char).collect()
            };
            match name_id {
                1 | 16 if family.is_none() => family = Some(text),
                2 | 17 => subfamily = text,
                _ => {}
            }
        }
        let sub = subfamily.to_lowercase();
        family.map(|f| {
            (
                f,
                sub.contains("bold"),
                sub.contains("italic") || sub.contains("oblique"),
            )
        })
    }

    /// Map a Unicode scalar to a glyph id (0 = .notdef).
    pub fn glyph_id(&self, c: char) -> u16 {
        let cp = c as u32;
        if let Some(sub) = self.cmap12 {
            if let Some(g) = self.cmap12_lookup(sub, cp) {
                return g;
            }
        }
        if cp <= 0xffff {
            if let Some(sub) = self.cmap4 {
                return self.cmap4_lookup(sub, cp as u16);
            }
        }
        0
    }

    fn cmap12_lookup(&self, sub: usize, cp: u32) -> Option<u16> {
        let d = &self.data;
        let n = be_u32(d, sub + 12) as usize;
        // groups: startChar, endChar, startGlyph — binary search
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let o = sub + 16 + mid * 12;
            let start = be_u32(d, o);
            let end = be_u32(d, o + 4);
            if cp < start {
                hi = mid;
            } else if cp > end {
                lo = mid + 1;
            } else {
                return Some((be_u32(d, o + 8) + cp - start) as u16);
            }
        }
        None
    }

    fn cmap4_lookup(&self, sub: usize, cp: u16) -> u16 {
        let d = &self.data;
        let seg_x2 = be_u16(d, sub + 6) as usize;
        let segs = seg_x2 / 2;
        let end_codes = sub + 14;
        let start_codes = end_codes + seg_x2 + 2;
        let id_deltas = start_codes + seg_x2;
        let id_range_off = id_deltas + seg_x2;
        for i in 0..segs {
            let end = be_u16(d, end_codes + i * 2);
            if cp > end {
                continue;
            }
            let start = be_u16(d, start_codes + i * 2);
            if cp < start {
                return 0;
            }
            let delta = be_i16(d, id_deltas + i * 2) as i32;
            let ro = be_u16(d, id_range_off + i * 2) as usize;
            if ro == 0 {
                return (cp as i32 + delta).rem_euclid(65536) as u16;
            }
            let glyph_idx_off = id_range_off + i * 2 + ro + (cp as usize - start as usize) * 2;
            if glyph_idx_off + 2 > d.len() {
                return 0;
            }
            let glyph = be_u16(d, glyph_idx_off);
            if glyph == 0 {
                return 0;
            }
            return (glyph as i32 + delta).rem_euclid(65536) as u16;
        }
        0
    }

    fn load_kern(&mut self) {
        self.kern_loaded = true;
        let Some(base) = self.tables.kern else { return };
        let d = &self.data;
        if base + 4 > d.len() {
            return;
        }
        let n_tables = be_u16(d, base + 2) as usize;
        let mut o = base + 4;
        for _ in 0..n_tables.min(4) {
            if o + 6 > d.len() {
                break;
            }
            let len = be_u16(d, o + 2) as usize;
            let coverage = be_u16(d, o + 4);
            // format is the high byte; only format 0 horizontal (bit0) is used
            if coverage >> 8 != 0 || coverage & 1 == 0 || len < 14 {
                o += len.max(1);
                continue;
            }
            let n_pairs = be_u16(d, o + 6) as usize;
            for i in 0..n_pairs {
                let p = o + 14 + i * 6;
                if p + 6 > o + len || p + 6 > d.len() {
                    break;
                }
                let left = be_u16(d, p) as u32;
                let right = be_u16(d, p + 2) as u32;
                let val = be_i16(d, p + 4);
                self.kern_pairs.insert((left << 16) | right, val);
            }
            o += len.max(1);
        }
    }

    /// Kerning adjustment in font units between two glyphs.
    pub fn kern(&mut self, left: u16, right: u16) -> i16 {
        if !self.kern_loaded {
            self.load_kern();
        }
        self.kern_pairs
            .get(&((left as u32) << 16 | right as u32))
            .copied()
            .unwrap_or(0)
    }

    pub fn swash_font(&self) -> Option<swash::FontRef<'static>> {
        self.swash
    }

    /// Horizontal advance for a glyph, in font units.
    pub fn advance_units(&self, gid: u16) -> u16 {
        self.hmtx.get(gid as usize).map_or(0, |m| m.0)
    }

    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }

    /// Line height (ascent - descent + gap) in font units.
    pub fn line_height_units(&self) -> i32 {
        self.ascender as i32 - self.descender as i32 + self.line_gap as i32
    }

    // ---------- outlines ----------

    fn glyph_data(&self, gid: u16) -> Option<&[u8]> {
        let (i, n) = (gid as usize, self.num_glyphs as usize);
        if i >= n {
            return None;
        }
        let (s, e) = (self.loca[i] as usize, self.loca[i + 1] as usize);
        if e <= s {
            return None; // empty glyph (space)
        }
        let base = self.tables.glyf;
        self.data.get(base + s..base + e)
    }

    /// Flattened outline: list of contours, each a polyline of points in
    /// font units. Composite glyphs are resolved one level deep.
    pub fn outline(&self, gid: u16) -> Vec<Vec<(f32, f32)>> {
        let mut contours = Vec::new();
        self.outline_into(gid, 0.0, 0.0, 1.0, &mut contours, 0);
        contours
    }

    fn outline_into(
        &self,
        gid: u16,
        dx: f32,
        dy: f32,
        scale: f32,
        out: &mut Vec<Vec<(f32, f32)>>,
        depth: u8,
    ) {
        if depth > 4 {
            return;
        }
        let Some(g) = self.glyph_data(gid) else {
            return;
        };
        if g.len() < 10 {
            return;
        }
        let n_contours = be_i16(g, 0);
        if n_contours >= 0 {
            self.simple_outline(g, n_contours as usize, dx, dy, scale, out);
        } else {
            self.composite_outline(g, dx, dy, scale, out, depth);
        }
    }

    fn simple_outline(
        &self,
        g: &[u8],
        n_contours: usize,
        dx: f32,
        dy: f32,
        scale: f32,
        out: &mut Vec<Vec<(f32, f32)>>,
    ) {
        if g.len() < 10 + n_contours * 2 + 2 {
            return;
        }
        let end_pts: Vec<usize> = (0..n_contours)
            .map(|i| be_u16(g, 10 + i * 2) as usize)
            .collect();
        let n_pts = end_pts.last().copied().unwrap_or(0) + 1;
        let ins_len = be_u16(g, 10 + n_contours * 2) as usize;
        let flags_start = 10 + n_contours * 2 + 2 + ins_len;
        // flags (with repeats)
        let mut flags = Vec::with_capacity(n_pts);
        let mut o = flags_start;
        while flags.len() < n_pts && o < g.len() {
            let f = g[o];
            o += 1;
            flags.push(f);
            if f & 0x08 != 0 {
                let rep = g.get(o).copied().unwrap_or(0) as usize;
                o += 1;
                for _ in 0..rep {
                    if flags.len() < n_pts {
                        flags.push(f);
                    }
                }
            }
        }
        if flags.len() < n_pts {
            return;
        }
        // x coords (flag bit1 = short, bit4 = same/positive)
        let mut xs = Vec::with_capacity(n_pts);
        let mut x = 0i32;
        for &f in &flags {
            if f & 0x02 != 0 {
                let v = g.get(o).copied().unwrap_or(0) as i32;
                o += 1;
                x += if f & 0x10 != 0 { v } else { -v };
            } else if f & 0x10 == 0 {
                if o + 2 > g.len() {
                    return;
                }
                x += be_i16(g, o) as i32;
                o += 2;
            }
            xs.push(x);
        }
        // y coords (bit2 = short, bit5 = same/positive)
        let mut ys = Vec::with_capacity(n_pts);
        let mut y = 0i32;
        for &f in &flags {
            if f & 0x04 != 0 {
                let v = g.get(o).copied().unwrap_or(0) as i32;
                o += 1;
                y += if f & 0x20 != 0 { v } else { -v };
            } else if f & 0x20 == 0 {
                if o + 2 > g.len() {
                    return;
                }
                y += be_i16(g, o) as i32;
                o += 2;
            }
            ys.push(y);
        }

        // Build contours; off-curve points get implicit on-curve midpoints.
        let mut start = 0usize;
        for &end in &end_pts {
            let idx: Vec<usize> = (start..=end).collect();
            start = end + 1;
            if idx.len() < 2 {
                continue;
            }
            let on = |i: usize| flags[idx[i]] & 0x01 != 0;
            let pt = |i: usize| (xs[idx[i]] as f32, ys[idx[i]] as f32);
            let n = idx.len();
            // Find first on-curve point (or synthesize one).
            let first_on = (0..n).find(|&i| on(i));
            let Some(first) = first_on else { continue };
            // Walk the contour emitting line segments and quadratic curves
            // flattened to lines.
            let mut poly = Vec::new();
            let mut i = 0usize;
            let count = n + 1;
            // current point tracker
            let mut cur = pt(first);
            poly.push((cur.0 * scale + dx, cur.1 * scale + dy));
            while i < count - 1 {
                let next_i = (first + i + 1) % n;
                let on_next = on(next_i);
                let p = pt(next_i);
                if on_next {
                    cur = p;
                    poly.push((p.0 * scale + dx, p.1 * scale + dy));
                    i += 1;
                } else {
                    // off-curve: quadratic through implicit midpoint
                    let after_i = (next_i + 1) % n;
                    let q = if on(after_i) {
                        pt(after_i)
                    } else {
                        ((p.0 + pt(after_i).0) / 2.0, (p.1 + pt(after_i).1) / 2.0)
                    };
                    flatten_quad(cur, p, q, scale, dx, dy, &mut poly);
                    cur = q;
                    i += if on(after_i) { 2 } else { 1 };
                }
            }
            if poly.len() > 2 {
                out.push(poly);
            }
        }
    }

    fn composite_outline(
        &self,
        g: &[u8],
        dx: f32,
        dy: f32,
        scale: f32,
        out: &mut Vec<Vec<(f32, f32)>>,
        depth: u8,
    ) {
        let mut o = 10usize;
        loop {
            if o + 4 > g.len() {
                break;
            }
            let flags = be_u16(g, o);
            let comp_gid = be_u16(g, o + 2);
            o += 4;
            let (a1, a2) = if flags & 0x0001 != 0 {
                let v = (be_i16(g, o) as f32, be_i16(g, o + 2) as f32);
                o += 4;
                v
            } else {
                let v = (g[o] as i8 as f32, g[o + 1] as i8 as f32);
                o += 2;
                v
            };
            // args are dx/dy when bit1 set; point-matching unsupported → 0 offset
            let (cdx, cdy) = if flags & 0x0002 != 0 {
                (a1, a2)
            } else {
                (0.0, 0.0)
            };
            let mut cscale = scale;
            if flags & 0x0008 != 0 {
                cscale *= f2dot14(be_i16(g, o));
                o += 2;
            } else if flags & 0x0040 != 0 {
                // x/y scale differ — approximate with x
                cscale *= f2dot14(be_i16(g, o));
                o += 4;
            } else if flags & 0x0080 != 0 {
                o += 8; // 2x2 matrix — unsupported, use uniform scale anyway
            }
            self.outline_into(
                comp_gid,
                dx + cdx * scale,
                dy + cdy * scale,
                cscale,
                out,
                depth + 1,
            );
            if flags & 0x0020 == 0 {
                break;
            }
        }
    }

    // ---------- rasterization ----------

    /// Rasterize a glyph at `px` pixels-per-em into an alpha bitmap.
    /// Returns (width, height, left-bearing px, top px from baseline, alpha).
    pub fn raster(&self, gid: u16, px: f32) -> Option<Raster> {
        let contours = self.outline(gid);
        if contours.is_empty() {
            return None;
        }
        let s = px / self.units_per_em as f32;
        let mut pts: Vec<Vec<(f32, f32)>> = contours
            .iter()
            .map(|c| c.iter().map(|&(x, y)| (x * s, y * s)).collect())
            .collect();
        // y-axis flip: font coords are y-up, bitmap is y-down.
        for c in &mut pts {
            for p in c.iter_mut() {
                p.1 = -p.1;
            }
        }
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for c in &pts {
            for &(x, y) in c {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
        let (w, h) = (
            (max_x - min_x).ceil() as i32 + 2,
            (max_y - min_y).ceil() as i32 + 2,
        );
        if w <= 0 || h <= 0 {
            return None;
        }
        const SS: i32 = 4;
        let mut cov = vec![0u8; (w * SS) as usize * (h * SS) as usize];
        for c in &pts {
            raster_contour(c, min_x - 1.0, min_y - 1.0, &mut cov, w * SS, h * SS);
        }
        // Box downsample
        let mut alpha = vec![0u8; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let mut sum = 0u32;
                for sy in 0..SS {
                    for sx in 0..SS {
                        sum += cov
                            [(y * SS + sy) as usize * (w * SS) as usize + (x * SS + sx) as usize]
                            as u32;
                    }
                }
                alpha[(y * w + x) as usize] = (sum / (SS * SS) as u32) as u8;
            }
        }
        Some(Raster {
            width: w as u32,
            height: h as u32,
            bearing_x: (min_x - 1.0).round() as i32,
            // Distance the ink extends above the baseline (y-up, positive).
            // Draw at: top = baseline - bearing_y.
            bearing_y: (-min_y - 1.0).round() as i32,
            alpha,
        })
    }
}

/// A rasterized glyph: grayscale coverage, positioned relative to the pen.
pub struct Raster {
    pub width: u32,
    pub height: u32,
    /// Pixels right of the pen origin where ink starts.
    pub bearing_x: i32,
    /// Pixels above the baseline where ink starts (y-up convention).
    pub bearing_y: i32,
    pub alpha: Vec<u8>,
}

impl Font {
    /// Pixel advance of a glyph at the given pixels-per-em.
    pub fn advance(&self, gid: u16, px: f32) -> f32 {
        self.advance_units(gid) as f32 * px / self.units_per_em as f32
    }

    /// Width of a text run at `px`, applying kerning.
    pub fn measure(&mut self, text: &str, px: f32) -> f32 {
        let s = px / self.units_per_em as f32;
        let mut w = 0.0f32;
        let mut prev = 0u16;
        for c in text.chars() {
            let gid = self.glyph_id(c);
            w += self.kern(prev, gid) as f32 * s;
            w += self.advance_units(gid) as f32;
            prev = gid;
        }
        w * s
    }

    /// Ascent at `px` (distance from baseline to top of line box).
    pub fn ascent(&self, px: f32) -> f32 {
        self.ascender as f32 * px / self.units_per_em as f32
    }

    /// Full line height at `px` (ascent - descent + gap).
    pub fn line_height(&self, px: f32) -> f32 {
        self.line_height_units() as f32 * px / self.units_per_em as f32
    }
}

/// Loads and caches fonts by (family, bold, italic). Lazily parses files —
/// rasterizing glyphs is cached per (glyph, pixel size) inside `GlyphCache`.
pub struct FontCache {
    faces: Vec<FontFace>,
    fonts: HashMap<usize, Font>, // index into faces
    glyphs: HashMap<(usize, u16, u32), Option<Raster>>, // (face idx, gid, px*4)
    ctx: swash::scale::ScaleContext,
}

impl Default for FontCache {
    fn default() -> Self {
        Self::new()
    }
}

impl FontCache {
    pub fn new() -> Self {
        Self {
            faces: scan_system(),
            fonts: HashMap::new(),
            glyphs: HashMap::new(),
            ctx: swash::scale::ScaleContext::new(),
        }
    }

    /// Resolve `family` (a CSS comma-separated list) to a font index. Each
    /// candidate substring-matches face families; generic keywords expand to
    /// preferred concrete families.
    pub fn resolve(&mut self, family: &str, bold: bool, italic: bool) -> Option<usize> {
        self.resolve_face(family, if bold { 700 } else { 400 }, 100, italic, None)
    }

    /// Full-strength resolution: numeric CSS `weight` (100–900), `stretch`
    /// percent, and an optional first char of the run — faces restricted by
    /// unicode-range (@font-face) only match when they cover that char.
    /// Each comma-separated name in `family` is tried in order; generic
    /// keywords expand to preferred concrete families.
    pub fn resolve_face(
        &mut self,
        family: &str,
        weight: u16,
        stretch: u16,
        italic: bool,
        ch: Option<char>,
    ) -> Option<usize> {
        let generics = |g: &str| -> &[&str] {
            match g {
                "monospace" | "ui-monospace" | "mono" | "fixed" | "fixed-width" | "consolas"
                | "menlo" | "sfmono" | "sf mono" => &[
                    "geist mono",
                    "jetbrains",
                    "noto sans mono",
                    "dejavu sans mono",
                    "mono",
                ],
                "sans-serif" | "sans" | "system-ui" | "ui-sans-serif" | "-apple-system"
                | "blinkmacsystemfont" | "-webkit-standard" => {
                    &["liberation sans", "noto sans", "dejavu sans", "geist"]
                }
                "serif" | "ui-serif" => &["liberation serif", "noto serif", "dejavu serif"],
                "cursive" | "fantasy" => &["comic", "liberation sans"],
                "emoji" => &["noto color emoji", "emoji", "symbola", "sans"],
                "math" => &["noto sans math", "stix", "dejavu sans"],
                _ => &[],
            }
        };
        for cand in family.split(',') {
            let want = cand
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_lowercase();
            if want.is_empty() {
                continue;
            }
            let wants: Vec<String> = {
                let g = generics(want.as_str());
                if g.is_empty() {
                    vec![want]
                } else {
                    g.iter().map(|s| s.to_string()).collect()
                }
            };
            for want in wants {
                if let Some(idx) = self.best_face(&want, weight, stretch, italic, ch) {
                    if !self.fonts.contains_key(&idx) {
                        if let Some(font) = Font::load(&self.faces[idx].path) {
                            self.fonts.insert(idx, font);
                        }
                    }
                    return Some(idx);
                }
            }
        }
        None
    }

    /// Best face substring-matching `want` for (weight, stretch, italic,
    /// optional first-char). Registered (@font-face / local-alias) faces
    /// outrank fuzzy system matches; unicode-range restricted faces need
    /// coverage of `ch` (unrestricted when `ch` is None).
    fn best_face(
        &self,
        want: &str,
        weight: u16,
        stretch: u16,
        italic: bool,
        ch: Option<char>,
    ) -> Option<usize> {
        let cp = ch.map(|c| c as u32);
        let mut best: Option<(usize, u32)> = None;
        for (i, f) in self.faces.iter().enumerate() {
            if f.disabled || !f.family.to_lowercase().contains(want) {
                continue;
            }
            if !f.unicode_ranges.is_empty() {
                if let Some(cp) = cp {
                    if !f.unicode_ranges.iter().any(|&(a, b)| cp >= a && cp <= b) {
                        continue;
                    }
                }
            }
            let mut score = 1u32;
            if f.italic == italic {
                score += 4;
            }
            // nearest weight wins: ±50 gets 2, within 150 gets 1
            let wd = (f.weight as i32 - weight as i32).unsigned_abs();
            score += match wd {
                0..=50 => 2,
                51..=150 => 1,
                _ => 0,
            };
            if (f.stretch as i32 - stretch as i32).unsigned_abs() <= 10 {
                score += 1;
            }
            if f.registered {
                score += 8;
            }
            if best.is_none_or(|(_, s)| score > s) {
                best = Some((i, score));
            }
        }
        best.map(|(i, _)| i)
    }

    pub fn font(&mut self, idx: usize) -> Option<&mut Font> {
        self.fonts.get_mut(&idx)
    }

    /// Cached glyph raster at `px` (quantized to quarter pixels).
    pub fn raster(&mut self, idx: usize, gid: u16, px: f32) -> Option<&Raster> {
        let key = (idx, gid, (px * 4.0) as u32);
        if !self.glyphs.contains_key(&key) {
            let font = self.fonts.get(&idx)?;
            let r = if let Some(sf) = font.swash_font() {
                let mut scaler = self.ctx.builder(sf).size(px).hint(true).build();
                swash::scale::Render::new(&[swash::scale::Source::Outline])
                    .render(&mut scaler, gid)
                    .map(|img| Raster {
                        width: img.placement.width,
                        height: img.placement.height,
                        bearing_x: img.placement.left,
                        bearing_y: img.placement.top,
                        alpha: img.data,
                    })
            } else {
                font.raster(gid, px)
            };
            self.glyphs.insert(key, r);
        }
        self.glyphs.get(&key)?.as_ref()
    }

    /// Register a downloaded @font-face font under its declared `family`,
    /// so `font-family: <family>` resolves to it ahead of fuzzy system
    /// matches. Returns the face index, or None when `bytes` isn't a
    /// decodable (TrueType-outline) font.
    pub fn register_bytes(
        &mut self,
        family: &str,
        bytes: &[u8],
        weight: u16,
        italic: bool,
        unicode_ranges: Vec<(u32, u32)>,
    ) -> Option<usize> {
        let font = Font::parse(bytes)?;
        let idx = self.faces.len();
        self.faces.push(FontFace {
            family: family.to_string(),
            bold: weight >= 600,
            italic,
            path: PathBuf::new(),
            weight,
            stretch: 100,
            unicode_ranges,
            registered: true,
            disabled: false,
        });
        self.fonts.insert(idx, font);
        Some(idx)
    }

    /// `local("Name")` source: alias an already-installed face under the
    /// @font-face `family`. Returns the new index when a face matched.
    pub fn register_local(
        &mut self,
        family: &str,
        local_name: &str,
        weight: u16,
        italic: bool,
        unicode_ranges: Vec<(u32, u32)>,
    ) -> Option<usize> {
        let want = local_name.trim().to_lowercase();
        if want.is_empty() {
            return None;
        }
        let src = self.faces.iter().position(|f| {
            !f.registered && !f.disabled && f.family.to_lowercase().contains(&want)
        })?;
        let mut face = self.faces[src].clone();
        face.family = family.to_string();
        face.weight = weight;
        face.bold = weight >= 600;
        face.italic = italic;
        face.unicode_ranges = unicode_ranges;
        face.registered = true;
        let idx = self.faces.len();
        self.faces.push(face);
        Some(idx)
    }

    /// Deactivate every web-registered face — the @font-face set is per
    /// page, so navigation calls this before registering the next batch.
    /// Indices stay stable; display lists built from a stale page still
    /// hold valid (if disabled) indices.
    pub fn clear_registered(&mut self) {
        for (i, f) in self.faces.iter_mut().enumerate() {
            if f.registered && !f.disabled {
                f.disabled = true;
                self.fonts.remove(&i);
                self.glyphs.retain(|&(gi, _, _), _| gi != i);
            }
        }
    }
}

fn f2dot14(v: i16) -> f32 {
    v as f32 / 16384.0
}

fn flatten_quad(
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    scale: f32,
    dx: f32,
    dy: f32,
    out: &mut Vec<(f32, f32)>,
) {
    // Flatten quadratic bezier by adaptive subdivision (16 segs is plenty at UI sizes).
    const N: i32 = 16;
    for i in 1..=N {
        let t = i as f32 / N as f32;
        let u = 1.0 - t;
        let x = u * u * p0.0 + 2.0 * u * t * p1.0 + t * t * p2.0;
        let y = u * u * p0.1 + 2.0 * u * t * p1.1 + t * t * p2.1;
        out.push((x * scale + dx, y * scale + dy));
    }
}

/// Scanline non-zero winding fill of one closed polyline into a coverage
/// buffer at supersampled resolution. `ox/oy` are the pixel-space offset
/// the polyline coordinates are relative to. The coverage buffer is
/// `w`×`h` *supersamples* — coordinates are scaled by SS accordingly.
fn raster_contour(poly: &[(f32, f32)], ox: f32, oy: f32, cov: &mut [u8], w: i32, h: i32) {
    const SS: f32 = 4.0;
    // For each scanline (supersampled row), collect edge crossings.
    for sy in 0..h {
        let y = sy as f32 + 0.5;
        let mut xs: Vec<(f32, i32)> = Vec::new(); // (x, winding delta)
        let n = poly.len();
        for i in 0..n {
            let (x0, y0) = poly[i];
            let (x1, y1) = poly[(i + 1) % n];
            let (ya, yb) = ((y0 - oy) * SS, (y1 - oy) * SS);
            if (ya <= y) == (yb <= y) || ya == yb {
                continue;
            }
            let t = (y - ya) / (yb - ya);
            let x = (x0 - ox + t * (x1 - x0)) * SS;
            xs.push((x, if yb > ya { 1 } else { -1 }));
        }
        xs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut winding = 0i32;
        let mut last_x = 0f32;
        for (x, delta) in xs {
            if winding != 0 {
                let (a, b) = (last_x.max(0.0), x.min(w as f32));
                let (ia, ib) = (a.floor() as i32, b.ceil() as i32);
                for px_x in ia..ib.min(w) {
                    // fractional coverage within this supersample cell
                    let cover = (b.min(px_x as f32 + 1.0) - a.max(px_x as f32)).clamp(0.0, 1.0);
                    let idx = sy as usize * w as usize + px_x as usize;
                    cov[idx] = cov[idx].saturating_add((cover * 255.0) as u8);
                }
            }
            winding += delta;
            last_x = x;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn liberation() -> Font {
        Font::load(Path::new(
            "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        ))
        .expect("load liberation")
    }

    #[test]
    fn parses_font() {
        let f = liberation();
        assert!(f.units_per_em() > 0);
        assert!(f.num_glyphs > 100);
    }

    #[test]
    fn maps_ascii() {
        let f = liberation();
        assert_ne!(f.glyph_id('A'), 0);
        assert_ne!(f.glyph_id('z'), 0);
        assert_ne!(f.glyph_id('A'), f.glyph_id('z'));
    }

    #[test]
    fn rasterizes_letter() {
        let f = liberation();
        let gid = f.glyph_id('H');
        let r = f.raster(gid, 32.0).expect("raster");
        assert!(r.width > 10 && r.height > 15);
        let ink: u64 = r.alpha.iter().map(|&a| a as u64).sum();
        assert!(ink > 1000, "glyph should have substantial ink");
    }

    #[test]
    fn composite_accented_char() {
        let f = liberation();
        let gid = f.glyph_id('é');
        // 'é' is usually a composite of e + acute — must still produce ink
        if let Some(r) = f.raster(gid, 32.0) {
            let ink: u64 = r.alpha.iter().map(|&a| a as u64).sum();
            assert!(ink > 1000);
        }
    }

    #[test]
    fn discovers_fonts() {
        let faces = scan_dir(Path::new("/usr/share/fonts/liberation"));
        assert!(faces.len() >= 12);
        let sans = select(&faces, "sans", true, false).unwrap();
        assert!(sans.bold);
    }

    fn font_bytes() -> Vec<u8> {
        std::fs::read("/usr/share/fonts/liberation/LiberationSans-Regular.ttf").unwrap()
    }

    #[test]
    fn registers_bytes_as_family() {
        let mut c = FontCache::new();
        let idx = c
            .register_bytes("Totally Custom", &font_bytes(), 400, false, vec![])
            .expect("register");
        assert_eq!(c.resolve("totally custom", false, false), Some(idx));
        assert!(c.font(idx).is_some());
    }

    #[test]
    fn registered_weight_and_range_gate() {
        let mut c = FontCache::new();
        let up = c
            .register_bytes("Ranged", &font_bytes(), 400, false, vec![(0x41, 0x5a)])
            .unwrap();
        let lo = c
            .register_bytes("Ranged", &font_bytes(), 400, false, vec![(0x61, 0x7a)])
            .unwrap();
        // first char of the run picks the covering face
        assert_eq!(
            c.resolve_face("ranged", 400, 100, false, Some('H')),
            Some(up)
        );
        assert_eq!(
            c.resolve_face("ranged", 400, 100, false, Some('h')),
            Some(lo)
        );
        // no char context → unrestricted resolution still works
        assert!(c.resolve_face("ranged", 400, 100, false, None).is_some());
    }

    #[test]
    fn register_local_aliases_system_face() {
        let mut c = FontCache::new();
        let idx = c
            .register_local("Alias Fam", "liberation sans", 700, false, vec![])
            .expect("local match");
        assert_eq!(
            c.resolve_face("alias fam", 700, 100, false, None),
            Some(idx)
        );
        assert!(c.font(idx).is_some());
    }

    #[test]
    fn clear_registered_disables_web_faces() {
        let mut c = FontCache::new();
        c.register_bytes("Gone Fam", &font_bytes(), 400, false, vec![])
            .unwrap();
        c.clear_registered();
        assert!(c.resolve("gone fam", false, false).is_none());
    }
}
