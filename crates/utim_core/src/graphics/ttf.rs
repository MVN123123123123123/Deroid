//! Minimal, zero-dependency TrueType (TTF) font parser and scanline rasterizer.
//!
//! Designed specifically for loading authentic packaged Google Noto Sans
//! (`NotoSans-Regular.ttf` or `NotoSans-Bold.ttf`) from system font paths,
//! providing production-quality anti-aliased glyph rendering with zero external
//! crates and zero dynamic heap allocation on the render hot path.
//!
//! Conforms to `AGENTS.md`:
//! - Pure stdlib implementation (no FreeType, HarfBuzz, fontdue, or rusttype).
//! - Sub-millisecond cold boot footprint.
//! - Graceful deterministic fallback to the home-made typography engine if the font
//!   file is missing, corrupt, or unreadable.

use std::fs;
use std::path::Path;

/// Standard system search paths for Google Noto Sans.
pub const NOTO_SANS_CANDIDATE_PATHS: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/opentype/noto/NotoSans-Regular.otf",
    "/system/fonts/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];

/// Flattened line segment in font design units (1000 units/em).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TtfLine {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

/// Pre-parsed TrueType glyph for fast rasterization.
#[derive(Debug, Clone)]
pub struct TtfGlyph {
    pub advance: f32,
    pub lsb: f32,
    pub bbox: (f32, f32, f32, f32), // (min_x, min_y, max_x, max_y)
    pub lines: Vec<TtfLine>,
}

/// Loaded TrueType font holding ASCII printable glyphs (0x20..0x7F).
#[derive(Debug, Clone)]
pub struct TrueTypeFont {
    pub units_per_em: f32,
    pub ascender: f32,
    pub descender: f32,
    pub glyphs: [Option<TtfGlyph>; 96],
}

impl TrueTypeFont {
    /// Attempt to load and parse Google Noto Sans from standard system paths.
    pub fn load_system_noto() -> Option<Self> {
        for path in NOTO_SANS_CANDIDATE_PATHS {
            if Path::new(path).exists() {
                if let Some(font) = Self::load_from_file(path) {
                    return Some(font);
                }
            }
        }
        None
    }

    /// Load and parse a TrueType font from a file path.
    pub fn load_from_file(path: &str) -> Option<Self> {
        let bytes = fs::read(path).ok()?;
        Self::parse(&bytes).ok()
    }

    /// Parse a TrueType font from raw binary data.
    pub fn parse(data: &[u8]) -> Result<Self, &'static str> {
        if data.len() < 12 {
            return Err("TTF data too small for offset table");
        }

        let num_tables = read_u16(data, 4) as usize;
        if data.len() < 12 + num_tables * 16 {
            return Err("TTF table directory truncated");
        }

        // Locate required tables
        let mut head_range = None;
        let mut hhea_range = None;
        let mut hmtx_range = None;
        let mut cmap_range = None;
        let mut loca_range = None;
        let mut glyf_range = None;

        for i in 0..num_tables {
            let offset_pos = 12 + i * 16;
            let tag = &data[offset_pos..offset_pos + 4];
            let offset = read_u32(data, offset_pos + 8) as usize;
            let length = read_u32(data, offset_pos + 12) as usize;

            if offset.saturating_add(length) > data.len() {
                continue;
            }

            match tag {
                b"head" => head_range = Some((offset, length)),
                b"hhea" => hhea_range = Some((offset, length)),
                b"hmtx" => hmtx_range = Some((offset, length)),
                b"cmap" => cmap_range = Some((offset, length)),
                b"loca" => loca_range = Some((offset, length)),
                b"glyf" => glyf_range = Some((offset, length)),
                _ => {}
            }
        }

        let (head_off, _) = head_range.ok_or("Missing head table")?;
        let (hhea_off, _) = hhea_range.ok_or("Missing hhea table")?;
        let (hmtx_off, _) = hmtx_range.ok_or("Missing hmtx table")?;
        let (cmap_off, _) = cmap_range.ok_or("Missing cmap table")?;
        let (loca_off, _) = loca_range.ok_or("Missing loca table")?;
        let (glyf_off, _) = glyf_range.ok_or("Missing glyf table")?;

        let units_per_em = read_u16(data, head_off + 18) as f32;
        let index_to_loc_format = read_i16(data, head_off + 50);
        let num_hmetrics = read_u16(data, hhea_off + 34) as usize;
        let ascender = read_i16(data, hhea_off + 4) as f32;
        let descender = read_i16(data, hhea_off + 6) as f32;

        let glyf_data = &data[glyf_off..];
        let loca_data = &data[loca_off..];
        let hmtx_data = &data[hmtx_off..];

        // Parse cmap format 4 subtable
        let cmap_data = &data[cmap_off..];
        let num_cmap_subtables = read_u16(cmap_data, 2) as usize;
        let mut fmt4_offset = None;

        for i in 0..num_cmap_subtables {
            let sub_pos = 4 + i * 8;
            if sub_pos + 8 <= cmap_data.len() {
                let sub_off = read_u32(cmap_data, sub_pos + 4) as usize;
                if sub_off + 2 <= cmap_data.len() && read_u16(cmap_data, sub_off) == 4 {
                    fmt4_offset = Some(sub_off);
                    break;
                }
            }
        }

        let fmt4_off = fmt4_offset.ok_or("No format 4 cmap subtable found")?;
        let fmt4 = &cmap_data[fmt4_off..];
        let seg_count = (read_u16(fmt4, 6) / 2) as usize;

        if fmt4.len() < 16 + seg_count * 8 {
            return Err("cmap format 4 subtable truncated");
        }

        let end_codes_pos = 14;
        let start_codes_pos = 16 + seg_count * 2;
        let id_deltas_pos = 16 + seg_count * 4;
        let id_range_offsets_pos = 16 + seg_count * 6;

        let get_glyph_id = |cp: u16| -> u16 {
            for i in 0..seg_count {
                let end_code = read_u16(fmt4, end_codes_pos + i * 2);
                if end_code >= cp {
                    let start_code = read_u16(fmt4, start_codes_pos + i * 2);
                    if start_code <= cp {
                        let id_range_offset = read_u16(fmt4, id_range_offsets_pos + i * 2);
                        let id_delta = read_i16(fmt4, id_deltas_pos + i * 2);
                        if id_range_offset == 0 {
                            return (cp as i16).wrapping_add(id_delta) as u16;
                        } else {
                            let ro_addr = id_range_offsets_pos + i * 2;
                            let target_addr = ro_addr + id_range_offset as usize + ((cp - start_code) as usize * 2);
                            if target_addr + 2 <= fmt4.len() {
                                let gid = read_u16(fmt4, target_addr);
                                if gid != 0 {
                                    return (gid as i16).wrapping_add(id_delta) as u16;
                                }
                            }
                        }
                    }
                    break;
                }
            }
            0
        };

        const NONE: Option<TtfGlyph> = None;
        let mut glyphs: [Option<TtfGlyph>; 96] = [NONE; 96];

        for cp in 0x20u16..0x80u16 {
            let gid = get_glyph_id(cp) as usize;
            let (aw, lsb) = if gid < num_hmetrics && (gid * 4 + 4) <= hmtx_data.len() {
                let a = read_u16(hmtx_data, gid * 4) as f32;
                let l = read_i16(hmtx_data, gid * 4 + 2) as f32;
                (a, l)
            } else {
                (units_per_em * 0.6, 0.0)
            };

            // Get glyph offset from loca table
            let (off_start, off_end) = if index_to_loc_format == 0 {
                if (gid * 2 + 4) <= loca_data.len() {
                    let s = (read_u16(loca_data, gid * 2) as usize) * 2;
                    let e = (read_u16(loca_data, (gid + 1) * 2) as usize) * 2;
                    (s, e)
                } else {
                    (0, 0)
                }
            } else if (gid * 4 + 8) <= loca_data.len() {
                let s = read_u32(loca_data, gid * 4) as usize;
                let e = read_u32(loca_data, (gid + 1) * 4) as usize;
                (s, e)
            } else {
                (0, 0)
            };

            if off_start >= off_end || off_end > glyf_data.len() {
                // Empty glyph (e.g. space)
                glyphs[(cp - 0x20) as usize] = Some(TtfGlyph {
                    advance: aw,
                    lsb,
                    bbox: (0.0, 0.0, 0.0, 0.0),
                    lines: Vec::new(),
                });
                continue;
            }

            let gslice = &glyf_data[off_start..off_end];
            let num_contours = read_i16(gslice, 0);
            if num_contours <= 0 {
                // Compound glyph or empty
                glyphs[(cp - 0x20) as usize] = Some(TtfGlyph {
                    advance: aw,
                    lsb,
                    bbox: (0.0, 0.0, 0.0, 0.0),
                    lines: Vec::new(),
                });
                continue;
            }

            let nc = num_contours as usize;
            if gslice.len() < 10 + nc * 2 {
                continue;
            }

            let x_min = read_i16(gslice, 2) as f32;
            let y_min = read_i16(gslice, 4) as f32;
            let x_max = read_i16(gslice, 6) as f32;
            let y_max = read_i16(gslice, 8) as f32;

            let mut end_pts = Vec::with_capacity(nc);
            for c in 0..nc {
                end_pts.push(read_u16(gslice, 10 + c * 2) as usize);
            }

            let num_pts = end_pts[nc - 1] + 1;
            let ins_len = read_u16(gslice, 10 + nc * 2) as usize;
            let mut ptr = 12 + nc * 2 + ins_len;

            // Read flags
            let mut flags = Vec::with_capacity(num_pts);
            while flags.len() < num_pts && ptr < gslice.len() {
                let fl = gslice[ptr];
                ptr += 1;
                flags.push(fl);
                if (fl & 0x08) != 0 && ptr < gslice.len() {
                    let repeat = gslice[ptr] as usize;
                    ptr += 1;
                    for _ in 0..repeat {
                        if flags.len() < num_pts {
                            flags.push(fl);
                        }
                    }
                }
            }

            if flags.len() < num_pts {
                continue;
            }

            // Read X coordinates
            let mut xs = Vec::with_capacity(num_pts);
            let mut cur_x: i32 = 0;
            for &fl in &flags {
                if (fl & 0x02) != 0 {
                    if ptr < gslice.len() {
                        let dx = gslice[ptr] as i32;
                        ptr += 1;
                        cur_x += if (fl & 0x10) != 0 { dx } else { -dx };
                    }
                } else if (fl & 0x10) == 0 {
                    if ptr + 2 <= gslice.len() {
                        let dx = read_i16(gslice, ptr) as i32;
                        ptr += 2;
                        cur_x += dx;
                    }
                }
                xs.push(cur_x as f32);
            }

            // Read Y coordinates
            let mut ys = Vec::with_capacity(num_pts);
            let mut cur_y: i32 = 0;
            for &fl in &flags {
                if (fl & 0x04) != 0 {
                    if ptr < gslice.len() {
                        let dy = gslice[ptr] as i32;
                        ptr += 1;
                        cur_y += if (fl & 0x20) != 0 { dy } else { -dy };
                    }
                } else if (fl & 0x20) == 0 {
                    if ptr + 2 <= gslice.len() {
                        let dy = read_i16(gslice, ptr) as i32;
                        ptr += 2;
                        cur_y += dy;
                    }
                }
                ys.push(cur_y as f32);
            }

            if xs.len() < num_pts || ys.len() < num_pts {
                continue;
            }

            // Decompose contours into lines
            let mut lines = Vec::new();
            let mut start_idx = 0;
            for &ep in &end_pts {
                if ep < start_idx || ep >= num_pts {
                    continue;
                }
                let contour_len = ep - start_idx + 1;
                if contour_len >= 2 {
                    let mut contour_pts = Vec::with_capacity(contour_len);
                    for i in start_idx..=ep {
                        contour_pts.push((xs[i], ys[i], (flags[i] & 0x01) != 0));
                    }
                    contour_to_lines(&contour_pts, &mut lines);
                }
                start_idx = ep + 1;
            }

            glyphs[(cp - 0x20) as usize] = Some(TtfGlyph {
                advance: aw,
                lsb,
                bbox: (x_min, y_min, x_max, y_max),
                lines,
            });
        }

        Ok(Self {
            units_per_em,
            ascender,
            descender,
            glyphs,
        })
    }

    /// Advance width in design units for character `b`.
    #[inline]
    pub fn advance(&self, b: u8) -> f32 {
        if (0x20..0x80).contains(&b) {
            if let Some(g) = &self.glyphs[(b - 0x20) as usize] {
                return g.advance;
            }
        }
        self.units_per_em * 0.6
    }

    /// Bounding box in design units for character `b`.
    #[inline]
    pub fn bbox(&self, b: u8) -> (f32, f32, f32, f32) {
        if (0x20..0x80).contains(&b) {
            if let Some(g) = &self.glyphs[(b - 0x20) as usize] {
                return g.bbox;
            }
        }
        (0.0, 0.0, 0.0, 0.0)
    }

    /// Rasterize glyph `b` into mask at `(ox, oy)` with subpixel anti-aliasing.
    #[allow(clippy::too_many_arguments)]
    pub fn rasterize_glyph(
        &self,
        b: u8,
        x: f32,
        scale: f32,
        baseline: f32,
        mask: &mut [u8],
        bw: i32,
        bh: i32,
        ox: i32,
        oy: i32,
    ) -> bool {
        if !(0x20..0x80).contains(&b) {
            return false;
        }
        let Some(g) = &self.glyphs[(b - 0x20) as usize] else {
            return false;
        };
        if g.lines.is_empty() {
            return false;
        }

        // Bounded stack array for line intersections per sub-scanline
        const MAX_INTERSECTIONS: usize = 32;
        let mut xs = [0.0f32; MAX_INTERSECTIONS];
        let mut touched = false;

        // Render each scanline row with 4x supersampling
        for r in 0..bh {
            let py = oy + r;
            let row_offset = (r as usize) * (bw as usize);

            for sub in 0..4 {
                let sy = py as f32 + (sub as f32 + 0.5) * 0.25;
                let mut count = 0;

                for l in &g.lines {
                    let sy0 = baseline - l.y0 * scale;
                    let sy1 = baseline - l.y1 * scale;

                    if (sy0 <= sy && sy < sy1) || (sy1 <= sy && sy < sy0) {
                        let sx0 = x + l.x0 * scale;
                        let sx1 = x + l.x1 * scale;
                        let xi = sx0 + (sy - sy0) * (sx1 - sx0) / (sy1 - sy0);
                        if count < MAX_INTERSECTIONS {
                            xs[count] = xi;
                            count += 1;
                        }
                    }
                }

                if count < 2 {
                    continue;
                }

                // Insertion sort intersections
                for i in 1..count {
                    let key = xs[i];
                    let mut j = i;
                    while j > 0 && xs[j - 1] > key {
                        xs[j] = xs[j - 1];
                        j -= 1;
                    }
                    xs[j] = key;
                }

                // Fill between pairs using even-odd rule
                let mut k = 0;
                while k + 1 < count {
                    let x0_span = xs[k];
                    let x1_span = xs[k + 1];

                    let c_start = (x0_span - ox as f32).floor().max(0.0) as i32;
                    let c_end = (x1_span - ox as f32).ceil().min(bw as f32) as i32;

                    for col in c_start..c_end {
                        let px_left = ox as f32 + col as f32;
                        let px_right = px_left + 1.0;
                        let overlap = (px_right.min(x1_span) - px_left.max(x0_span)).max(0.0);
                        if overlap > 0.0 {
                            let idx = row_offset + col as usize;
                            let added = (overlap * 64.0) as u16;
                            let new_val = (mask[idx] as u16 + added).min(255) as u8;
                            mask[idx] = new_val;
                            touched = true;
                        }
                    }
                    k += 2;
                }
            }
        }

        touched
    }
}

/// Convert a contour of on/off curve points into a sequence of flat line segments.
fn contour_to_lines(pts: &[(f32, f32, bool)], lines: &mut Vec<TtfLine>) {
    let n = pts.len();
    if n < 2 {
        return;
    }

    // Expand implicit on-curve points between consecutive off-curve points
    let mut expanded = Vec::with_capacity(n * 2);
    for i in 0..n {
        let p_curr = pts[i];
        let p_next = pts[(i + 1) % n];
        expanded.push(p_curr);
        if !p_curr.2 && !p_next.2 {
            expanded.push(((p_curr.0 + p_next.0) * 0.5, (p_curr.1 + p_next.1) * 0.5, true));
        }
    }

    // Rotate so the start is on-curve
    let start_idx = expanded.iter().position(|p| p.2).unwrap_or(0);
    let total = expanded.len();

    let mut i = 0;
    while i < total {
        let p0 = expanded[(start_idx + i) % total];
        let p1 = expanded[(start_idx + i + 1) % total];

        if p1.2 {
            // Straight line segment
            lines.push(TtfLine {
                x0: p0.0,
                y0: p0.1,
                x1: p1.0,
                y1: p1.1,
            });
            i += 1;
        } else {
            // Quadratic Bézier curve: p0 (start), p1 (control), p2 (end)
            let p2 = expanded[(start_idx + i + 2) % total];
            // Flatten into 4 chords for sub-pixel accuracy
            for step in 0..4 {
                let t0 = step as f32 * 0.25;
                let t1 = (step + 1) as f32 * 0.25;

                let mt0 = 1.0 - t0;
                let ax0 = mt0 * mt0 * p0.0 + 2.0 * mt0 * t0 * p1.0 + t0 * t0 * p2.0;
                let ay0 = mt0 * mt0 * p0.1 + 2.0 * mt0 * t0 * p1.1 + t0 * t0 * p2.1;

                let mt1 = 1.0 - t1;
                let ax1 = mt1 * mt1 * p0.0 + 2.0 * mt1 * t1 * p1.0 + t1 * t1 * p2.0;
                let ay1 = mt1 * mt1 * p0.1 + 2.0 * mt1 * t1 * p1.1 + t1 * t1 * p2.1;

                lines.push(TtfLine {
                    x0: ax0,
                    y0: ay0,
                    x1: ax1,
                    y1: ay1,
                });
            }
            i += 2;
        }
    }
}

#[inline]
fn read_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([buf[offset], buf[offset + 1]])
}

#[inline]
fn read_i16(buf: &[u8], offset: usize) -> i16 {
    i16::from_be_bytes([buf[offset], buf[offset + 1]])
}

#[inline]
fn read_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_noto_loads_and_parses() {
        let font = TrueTypeFont::load_system_noto();
        assert!(font.is_some(), "System Google Noto Sans TTF should be found on this system");
        let font = font.unwrap();
        assert_eq!(font.units_per_em, 1000.0);

        // Digits 0..9 should be tabular
        let d0_adv = font.advance(b'0');
        assert!(d0_adv > 0.0);
        for c in b'1'..=b'9' {
            assert_eq!(font.advance(c), d0_adv, "Noto Sans digits must be tabular");
        }

        // Test rasterizing 'A' into a small mask
        let mut mask = [0u8; 64 * 64];
        let touched = font.rasterize_glyph(b'A', 10.0, 30.0 / 1000.0, 45.0, &mut mask, 30, 40, 10, 10);
        assert!(touched, "Glyph 'A' must produce non-zero mask pixels");
        let inked_count = mask.iter().filter(|&&v| v > 0).count();
        assert!(inked_count > 50, "Glyph 'A' should ink substantial pixels");
    }

    #[test]
    fn test_fallback_on_corrupt_data() {
        let garbage = [0u8; 32];
        let res = TrueTypeFont::parse(&garbage);
        assert!(res.is_err(), "Garbage bytes must be rejected gracefully");
    }
}
