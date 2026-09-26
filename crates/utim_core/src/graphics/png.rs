//! Dependency-free PNG decoder used by the launcher to blit real application icons.
//!
//! Decodes the icon subset of the PNG specification (RFC 2083) straight into
//! 8-bit RGBA: zlib/DEFLATE (stored, fixed and dynamic Huffman), all five scanline
//! filters, colour types 0/2/3/4/6, bit depths 1..16, `tRNS` transparency and
//! Adam7 interlacing. Every malformed or oversized input fails fast with `None`
//! so untrusted image files can never panic or exhaust the compositor.

/// Decoded 8-bit RGBA image (`pixels.len() == width * height * 4`).
#[derive(Clone, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl core::fmt::Debug for RgbaImage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RgbaImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.pixels.len())
            .finish()
    }
}

impl RgbaImage {
    /// Scale down (area averaged in premultiplied space) so the longest edge is at
    /// most `max_edge`, preserving aspect ratio. Images that already fit are kept
    /// untouched, so no copy happens on the common path.
    pub fn fit_within(self, max_edge: u32) -> RgbaImage {
        let longest = self.width.max(self.height);
        if longest <= max_edge || self.width == 0 || self.height == 0 {
            return self;
        }
        // A source block of 257x257 already saturates a u32 colour accumulator
        // (65025 * 257 * 257 > u32::MAX), so the sums must be u64.
        debug_assert!(
            longest / max_edge.max(1) <= 257,
            "block too large; widen the accumulator"
        );
        let sw = self.width as usize;
        let sh = self.height as usize;
        let dw = ((self.width as u64 * max_edge as u64) / longest as u64).max(1) as usize;
        let dh = ((self.height as u64 * max_edge as u64) / longest as u64).max(1) as usize;
        let mut out = vec![0u8; dw * dh * 4];

        for dy in 0..dh {
            let sy0 = dy * sh / dh;
            let sy1 = ((dy + 1) * sh / dh).max(sy0 + 1);
            for dx in 0..dw {
                let sx0 = dx * sw / dw;
                let sx1 = ((dx + 1) * sw / dw).max(sx0 + 1);
                let mut sr = 0u64;
                let mut sg = 0u64;
                let mut sb = 0u64;
                let mut sa = 0u64;
                for sy in sy0..sy1 {
                    let mut p = (sy * sw + sx0) * 4;
                    for _ in sx0..sx1 {
                        let a = self.pixels[p + 3] as u64;
                        sr += self.pixels[p] as u64 * a;
                        sg += self.pixels[p + 1] as u64 * a;
                        sb += self.pixels[p + 2] as u64 * a;
                        sa += a;
                        p += 4;
                    }
                }
                let n = ((sx1 - sx0) * (sy1 - sy0)) as u64;
                let o = (dy * dw + dx) * 4;
                // Round-to-nearest unpremultiply; a fully transparent block
                // (division by zero) falls back to zero instead of trapping.
                // d >= 1 on every real block; checked ops keep it total.
                let unpremul = |v: u64, d: u64| -> u8 {
                    v.checked_add(d / 2)
                        .and_then(|num| num.checked_div(d))
                        .unwrap_or(0) as u8
                };
                out[o] = unpremul(sr, sa);
                out[o + 1] = unpremul(sg, sa);
                out[o + 2] = unpremul(sb, sa);
                out[o + 3] = unpremul(sa, n);
            }
        }

        RgbaImage {
            width: dw as u32,
            height: dh as u32,
            pixels: out,
        }
    }
}

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Upper bound on decoded pixels (2048x2048 = 16 MiB of RGBA) for hostile files.
const MAX_PIXELS: u64 = 4_194_304;
/// Bound on a single IDAT payload so chunk parsing cannot run away.
const MAX_IDAT_BYTES: usize = 32 * 1024 * 1024;

/// Decode a PNG byte stream into RGBA8. Returns `None` on any malformed input.
pub fn decode_png(data: &[u8]) -> Option<RgbaImage> {
    if data.len() < 8 || data[..8] != PNG_MAGIC {
        return None;
    }

    let mut width = 0u32;
    let mut height = 0u32;
    let mut depth = 0u8;
    let mut color = 0u8;
    let mut interlace = 0u8;
    let mut have_ihdr = false;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut transparency: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();

    let mut pos = 8usize;
    while pos + 12 <= data.len() {
        let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let body_start = pos + 8;
        let body_end = body_start.checked_add(len)?;
        let crc_start = body_end;
        let next = crc_start.checked_add(4)?;
        if next > data.len() {
            return None;
        }
        let ctype = &data[pos + 4..body_start];
        let crc = u32::from_be_bytes([
            data[crc_start],
            data[crc_start + 1],
            data[crc_start + 2],
            data[crc_start + 3],
        ]);
        if crc32(&data[pos + 4..body_end]) != crc {
            return None;
        }
        let body = &data[body_start..body_end];

        match ctype {
            b"IHDR" => {
                if len != 13 {
                    return None;
                }
                width = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                height = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
                depth = body[8];
                color = body[9];
                interlace = body[12];
                if body[10] != 0 || body[11] != 0 || interlace > 1 {
                    return None;
                }
                if width == 0
                    || height == 0
                    || width as u64 * height as u64 > MAX_PIXELS
                {
                    return None;
                }
                have_ihdr = true;
            }
            b"PLTE" => {
                if len == 0 || !len.is_multiple_of(3) || len > 256 * 3 {
                    return None;
                }
                let (triples, _) = body.as_chunks::<3>();
                palette = triples.to_vec();
            }
            b"tRNS" => transparency = body.to_vec(),
            b"IDAT" => {
                idat.len().checked_add(len).filter(|t| *t <= MAX_IDAT_BYTES)?;
                idat.extend_from_slice(body);
            }
            b"IEND" => break,
            _ => {}
        }
        pos = next;
    }

    if !have_ihdr || idat.is_empty() {
        return None;
    }
    let channels = channels_of(color)?;
    validate_header(depth, color, &palette)?;

    let raw_len = raw_data_len(width, height, depth, channels, interlace)?;
    let mut raw = inflate(&idat, raw_len)?;

    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let key = transparency_key(color, channels, &transparency);
    let palette = if color == 3 {
        Some(Palette::new(&palette, &transparency))
    } else {
        None
    };

    let passes: &[(u32, u32, u32, u32)] = if interlace == 0 { &ADAM7[..1] } else { &ADAM7[1..] };
    let mut cursor = 0usize;
    for &(x0, y0, dx, dy) in passes {
        let (cols, rows) = pass_dims(width, height, x0, y0, dx, dy);
        let (cols, rows) = (cols as usize, rows as usize);
        if cols == 0 || rows == 0 {
            continue;
        }
        let row_bytes = (cols * channels as usize * depth as usize).div_ceil(8);
        let region_len = (row_bytes + 1).checked_mul(rows)?;
        let end = cursor.checked_add(region_len)?;
        if end > raw.len() {
            return None;
        }
        let bpp = (channels as usize * depth as usize).div_ceil(8);
        unfilter(&mut raw[cursor..end], rows, row_bytes, bpp)?;
        convert_pass(
            &raw[cursor..end],
            rows,
            cols,
            row_bytes,
            depth,
            channels,
            palette.as_ref(),
            key,
            width as usize,
            &mut pixels,
            x0 as usize,
            dx as usize,
            y0 as usize,
            dy as usize,
        );
        cursor = end;
    }

    Some(RgbaImage {
        width,
        height,
        pixels,
    })
}

/// Adam7 pass geometry: (x offset, y offset, x step, y step). The first entry is
/// the trivial full-image pass used for non-interlaced data.
const ADAM7: [(u32, u32, u32, u32); 8] = [
    (0, 0, 1, 1),
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

fn pass_dims(w: u32, h: u32, x0: u32, y0: u32, dx: u32, dy: u32) -> (u32, u32) {
    let cols = if x0 < w { (w - x0).div_ceil(dx) } else { 0 };
    let rows = if y0 < h { (h - y0).div_ceil(dy) } else { 0 };
    (cols, rows)
}

fn channels_of(color: u8) -> Option<u8> {
    match color {
        0 | 3 => Some(1),
        2 => Some(3),
        4 => Some(2),
        6 => Some(4),
        _ => None,
    }
}

fn validate_header(depth: u8, color: u8, palette: &[[u8; 3]]) -> Option<()> {
    let depth_ok = match color {
        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
        2 => matches!(depth, 8 | 16),
        3 => matches!(depth, 1 | 2 | 4 | 8) && !palette.is_empty(),
        4 | 6 => matches!(depth, 8 | 16),
        _ => return None,
    };
    if depth_ok {
        Some(())
    } else {
        None
    }
}

fn raw_data_len(w: u32, h: u32, depth: u8, channels: u8, interlace: u8) -> Option<usize> {
    if interlace == 0 {
        let row_bytes = (w as usize).checked_mul(channels as usize)?.checked_mul(depth as usize)?;
        let row_bytes = row_bytes.checked_add(7)? / 8;
        return row_bytes.checked_add(1)?.checked_mul(h as usize);
    }
    let mut total = 0usize;
    for &(x0, y0, dx, dy) in ADAM7.iter().skip(1) {
        let (cols, rows) = pass_dims(w, h, x0, y0, dx, dy);
        if cols == 0 || rows == 0 {
            continue;
        }
        let row_bytes = (cols as usize).checked_mul(channels as usize)?.checked_mul(depth as usize)?;
        let row_bytes = row_bytes.checked_add(7)? / 8;
        total = total.checked_add((row_bytes + 1).checked_mul(rows as usize)?)?;
    }
    Some(total)
}

struct Palette {
    entries: Vec<[u8; 3]>,
    alpha: Vec<u8>,
}

impl Palette {
    fn new(plte: &[[u8; 3]], trns: &[u8]) -> Palette {
        Palette {
            entries: plte.to_vec(),
            alpha: trns.to_vec(),
        }
    }
}

/// `tRNS` colour-key for greyscale / truecolour images.
fn transparency_key(color: u8, channels: u8, trns: &[u8]) -> Option<[u16; 4]> {
    if color != 0 && color != 2 {
        return None;
    }
    if trns.len() < channels as usize * 2 {
        return None;
    }
    let mut key = [0u16; 4];
    for i in 0..channels as usize {
        key[i] = u16::from_be_bytes([trns[i * 2], trns[i * 2 + 1]]);
    }
    Some(key)
}

#[inline]
fn sample(row: &[u8], index: usize, depth: u8) -> u16 {
    match depth {
        8 => row[index] as u16,
        16 => u16::from_be_bytes([row[index * 2], row[index * 2 + 1]]),
        _ => {
            let bit = index * depth as usize;
            let shift = 8 - depth as u32 - (bit & 7) as u32;
            ((row[bit >> 3] >> shift) as u16) & ((1u16 << depth) - 1)
        }
    }
}

#[inline]
fn gray_to_u8(raw: u16, depth: u8) -> u8 {
    match depth {
        16 => (raw >> 8) as u8,
        8 => raw as u8,
        _ => {
            let max = (1u32 << depth) - 1;
            ((raw as u32 * 255 + max / 2) / max) as u8
        }
    }
}

#[inline]
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i32 + b as i32 - c as i32;
    let pa = (p - a as i32).abs();
    let pb = (p - b as i32).abs();
    let pc = (p - c as i32).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Undo PNG scanline filtering in place. `buf` is `rows * (row_bytes + 1)` bytes.
fn unfilter(buf: &mut [u8], rows: usize, row_bytes: usize, bpp: usize) -> Option<()> {
    let stride = row_bytes + 1;
    if rows == 0 {
        return Some(());
    }
    if buf.len() < stride * rows || bpp == 0 {
        return None;
    }
    for y in 0..rows {
        let start = y * stride;
        let filter = buf[start];
        let (head, tail) = buf.split_at_mut(start + 1);
        let row = &mut tail[..row_bytes];
        let prev = if y == 0 { None } else { Some(&head[start - stride + 1..start]) };
        match filter {
            0 => {}
            1 => {
                for i in bpp..row_bytes {
                    let a = row[i - bpp];
                    row[i] = row[i].wrapping_add(a);
                }
            }
            2 => {
                if let Some(prev) = prev {
                    for i in 0..row_bytes {
                        row[i] = row[i].wrapping_add(prev[i]);
                    }
                }
            }
            3 => {
                for i in 0..row_bytes {
                    let a = if i >= bpp { row[i - bpp] } else { 0u8 };
                    let b = prev.map_or(0, |p| p[i]);
                    row[i] = row[i].wrapping_add(((a as u16 + b as u16) / 2) as u8);
                }
            }
            4 => {
                for i in 0..row_bytes {
                    let a = if i >= bpp { row[i - bpp] } else { 0u8 };
                    let b = prev.map_or(0, |p| p[i]);
                    let c = if i >= bpp { prev.map_or(0, |p| p[i - bpp]) } else { 0u8 };
                    row[i] = row[i].wrapping_add(paeth(a, b, c));
                }
            }
            _ => return None,
        }
    }
    Some(())
}

#[allow(clippy::too_many_arguments)]
fn convert_pass(
    data: &[u8],
    rows: usize,
    cols: usize,
    row_bytes: usize,
    depth: u8,
    channels: u8,
    palette: Option<&Palette>,
    key: Option<[u16; 4]>,
    out_w: usize,
    out: &mut [u8],
    x0: usize,
    dx: usize,
    y0: usize,
    dy: usize,
) {
    let stride = row_bytes + 1;
    for j in 0..rows {
        let off = j * stride;
        let row = &data[off + 1..off + 1 + row_bytes];
        let oy = y0 + j * dy;
        for i in 0..cols {
            let base = (oy * out_w + (x0 + i * dx)) * 4;
            match channels {
                1 => {
                    let raw = sample(row, i, depth);
                    if let Some(pal) = palette {
                        // Indexed colour: palette lookup + per-entry alpha.
                        let idx = raw as usize;
                        if idx < pal.entries.len() {
                            out[base] = pal.entries[idx][0];
                            out[base + 1] = pal.entries[idx][1];
                            out[base + 2] = pal.entries[idx][2];
                            out[base + 3] = pal.alpha.get(idx).copied().unwrap_or(255);
                        } else {
                            out[base + 3] = 0;
                        }
                    } else {
                        let g = gray_to_u8(raw, depth);
                        let transparent = key.is_some_and(|k| raw == k[0]);
                        out[base] = g;
                        out[base + 1] = g;
                        out[base + 2] = g;
                        out[base + 3] = if transparent { 0 } else { 255 };
                    }
                }
                2 => {
                    let o = i * 2;
                    let g = gray_to_u8(sample(row, o, depth), depth);
                    out[base] = g;
                    out[base + 1] = g;
                    out[base + 2] = g;
                    out[base + 3] = gray_to_u8(sample(row, o + 1, depth), depth);
                }
                3 => {
                    let o = i * 3;
                    let r = sample(row, o, depth);
                    let g = sample(row, o + 1, depth);
                    let b = sample(row, o + 2, depth);
                    let transparent = key.is_some_and(|k| r == k[0] && g == k[1] && b == k[2]);
                    out[base] = gray_to_u8(r, depth);
                    out[base + 1] = gray_to_u8(g, depth);
                    out[base + 2] = gray_to_u8(b, depth);
                    out[base + 3] = if transparent { 0 } else { 255 };
                }
                _ => {
                    let o = i * 4;
                    out[base] = gray_to_u8(sample(row, o, depth), depth);
                    out[base + 1] = gray_to_u8(sample(row, o + 1, depth), depth);
                    out[base + 2] = gray_to_u8(sample(row, o + 2, depth), depth);
                    out[base + 3] = gray_to_u8(sample(row, o + 3, depth), depth);
                }
            }
        }
    }
}

// --- CRC-32 (chunk integrity) and Adler-32 (zlib trailer) ---

/// 16-entry nibble table: 64 bytes of .rodata, 2 lookups per byte instead of
/// 8 shift/xor/mask rounds.
const fn crc_nibble() -> [u32; 16] {
    let mut t = [0u32; 16];
    let mut i = 0;
    while i < 16 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 4 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}
static CRC_NIB: [u32; 16] = crc_nibble();

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        crc = (crc >> 4) ^ CRC_NIB[(crc & 0xF) as usize];
        crc = (crc >> 4) ^ CRC_NIB[(crc & 0xF) as usize];
    }
    !crc
}

/// NMAX = 5552 is the largest n for which 255*n*(n+1)/2 + 1 < 2^32, so `a`
/// and `b` cannot overflow u32 between reductions and the `%` runs once per
/// 5552-byte chunk instead of twice per byte.
fn adler32(data: &[u8]) -> u32 {
    const NMAX: usize = 5552;
    const MOD: u32 = 65_521;
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(NMAX) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

// --- DEFLATE (RFC 1951) + zlib (RFC 1950) ---

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CLEN_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, bit: 0 }
    }

    #[inline]
    fn read_bit(&mut self) -> Option<u8> {
        if self.bit >= self.data.len() * 8 {
            return None;
        }
        let b = (self.data[self.bit >> 3] >> (self.bit & 7)) & 1;
        self.bit += 1;
        Some(b)
    }

    fn read_bits(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if self.bit.saturating_add(n as usize) > self.data.len() * 8 {
            return None;
        }
        let mut value = 0u32;
        for i in 0..n {
            value |= (self.read_bit()? as u32) << i;
        }
        Some(value)
    }

    fn align_byte(&mut self) {
        self.bit = (self.bit + 7) & !7;
    }

    fn read_u16_le(&mut self) -> Option<u16> {
        self.align_byte();
        let byte = self.bit >> 3;
        if byte + 2 > self.data.len() {
            return None;
        }
        let v = u16::from_le_bytes([self.data[byte], self.data[byte + 1]]);
        self.bit += 16;
        Some(v)
    }

    fn take_bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        self.align_byte();
        let byte = self.bit >> 3;
        let end = byte.checked_add(n)?;
        if end > self.data.len() {
            return None;
        }
        self.bit = end * 8;
        Some(&self.data[byte..end])
    }
}

/// Canonical Huffman table in "puff" form: `counts[len]` codes, symbols sorted by
/// (length, value).
struct Huffman {
    counts: [i32; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Option<Huffman> {
        let mut counts = [0i32; 16];
        for &l in lengths {
            if l > 15 {
                return None;
            }
            counts[l as usize] += 1;
        }
        counts[0] = 0;

        let mut left = 1i32;
        for &count in counts.iter().skip(1) {
            left <<= 1;
            left -= count;
            if left < 0 {
                return None; // over-subscribed code
            }
        }

        let mut offsets = [0u16; 16];
        let mut total = 0u16;
        for len in 1..16 {
            offsets[len] = total;
            total += counts[len] as u16;
        }
        let mut symbols = vec![0u16; total as usize];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                let slot = offsets[l as usize] as usize;
                if slot >= symbols.len() {
                    return None;
                }
                symbols[slot] = sym as u16;
                offsets[l as usize] += 1;
            }
        }
        Some(Huffman { counts, symbols })
    }

    fn decode(&self, br: &mut BitReader) -> Option<u16> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..16 {
            code |= br.read_bit()? as i32;
            let count = self.counts[len];
            if code - count < first {
                return self.symbols.get((index + (code - first)) as usize).copied();
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        None
    }
}

fn fixed_tables() -> (Huffman, Huffman) {
    let mut lit = [0u8; 288];
    for (i, l) in lit.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let dist = [5u8; 30];
    (
        Huffman::new(&lit).expect("fixed literal table is valid"),
        Huffman::new(&dist).expect("fixed distance table is valid"),
    )
}

fn dynamic_tables(br: &mut BitReader) -> Option<(Huffman, Huffman)> {
    let hlit = br.read_bits(5)? as usize + 257;
    let hdist = br.read_bits(5)? as usize + 1;
    let hclen = br.read_bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return None;
    }
    let mut cl_lengths = [0u8; 19];
    for i in 0..hclen {
        cl_lengths[CLEN_ORDER[i]] = br.read_bits(3)? as u8;
    }
    let cl = Huffman::new(&cl_lengths)?;

    let mut lengths = vec![0u8; hlit + hdist];
    let mut i = 0usize;
    while i < lengths.len() {
        let sym = cl.decode(br)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return None;
                }
                let prev = lengths[i - 1];
                let repeat = 3 + br.read_bits(2)? as usize;
                if i + repeat > lengths.len() {
                    return None;
                }
                for _ in 0..repeat {
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let repeat = 3 + br.read_bits(3)? as usize;
                if i + repeat > lengths.len() {
                    return None;
                }
                i += repeat;
            }
            18 => {
                let repeat = 11 + br.read_bits(7)? as usize;
                if i + repeat > lengths.len() {
                    return None;
                }
                i += repeat;
            }
            _ => return None,
        }
    }

    let lit = Huffman::new(&lengths[..hlit])?;
    let dist = Huffman::new(&lengths[hlit..])?;
    Some((lit, dist))
}

fn decode_block(
    br: &mut BitReader,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
    limit: usize,
) -> Option<()> {
    loop {
        let sym = lit.decode(br)?;
        if sym < 256 {
            if out.len() >= limit {
                return None;
            }
            out.push(sym as u8);
        } else if sym == 256 {
            return Some(());
        } else {
            let li = sym as usize - 257;
            if li >= LENGTH_BASE.len() {
                return None;
            }
            let len = LENGTH_BASE[li] as usize + br.read_bits(LENGTH_EXTRA[li] as u32)? as usize;
            let dsym = dist.decode(br)? as usize;
            if dsym >= DIST_BASE.len() {
                return None;
            }
            let distance = DIST_BASE[dsym] as usize + br.read_bits(DIST_EXTRA[dsym] as u32)? as usize;
            if distance == 0 || distance > out.len() {
                return None;
            }
            if out.len() + len > limit {
                return None;
            }
            let start = out.len() - distance;
            for k in 0..len {
                let byte = out[start + k];
                out.push(byte);
            }
        }
    }
}

/// Inflate a zlib stream into exactly `expected_len` bytes.
fn inflate(stream: &[u8], expected_len: usize) -> Option<Vec<u8>> {
    if stream.len() < 6 {
        return None;
    }
    let cmf = stream[0];
    let flg = stream[1];
    if cmf & 0x0F != 8 || cmf >> 4 > 7 {
        return None;
    }
    if !((cmf as u32) << 8 | flg as u32).is_multiple_of(31) {
        return None;
    }
    if flg & 0x20 != 0 {
        return None; // preset dictionary unsupported
    }

    let adler_expected = u32::from_be_bytes([
        stream[stream.len() - 4],
        stream[stream.len() - 3],
        stream[stream.len() - 2],
        stream[stream.len() - 1],
    ]);
    let mut br = BitReader::new(&stream[2..stream.len() - 4]);
    let mut out: Vec<u8> = Vec::with_capacity(expected_len);

    loop {
        let last = br.read_bits(1)?;
        let btype = br.read_bits(2)?;
        match btype {
            0 => {
                let len = br.read_u16_le()?;
                let nlen = br.read_u16_le()?;
                if len ^ 0xFFFF != nlen {
                    return None;
                }
                let bytes = br.take_bytes(len as usize)?;
                if out.len() + bytes.len() > expected_len {
                    return None;
                }
                out.extend_from_slice(bytes);
            }
            1 => {
                let (lit, dist) = fixed_tables();
                decode_block(&mut br, &lit, &dist, &mut out, expected_len)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut br)?;
                decode_block(&mut br, &lit, &dist, &mut out, expected_len)?;
            }
            _ => return None,
        }
        if last == 1 {
            break;
        }
    }

    if out.len() != expected_len || adler32(&out) != adler_expected {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/png_fixtures.rs"));

    #[test]
    fn decodes_every_supported_colour_type() {
        let cases: [(&[u8], &[u8], &str); 8] = [
            (RGBA8_PNG, RGBA8_EXPECT, "rgba8"),
            (RGB8_PNG, RGB8_EXPECT, "rgb8"),
            (PALETTE4_PNG, PALETTE4_EXPECT, "palette4+trns"),
            (GRAY4_PNG, GRAY4_EXPECT, "gray4"),
            (GRAY_ALPHA8_PNG, GRAY_ALPHA8_EXPECT, "gray+alpha"),
            (GRAY16_PNG, GRAY16_EXPECT, "gray16"),
            (GRAY1_PNG, GRAY1_EXPECT, "gray1"),
            (INTERLACED_PNG, INTERLACED_EXPECT, "adam7 interlaced"),
        ];
        for (png, expect, label) in cases {
            let img = decode_png(png).unwrap_or_else(|| panic!("decode failed: {label}"));
            assert_eq!(img.width, 8, "{label} width");
            assert_eq!(img.height, 8, "{label} height");
            assert_eq!(img.pixels, expect, "{label} pixels");
        }
    }

    #[test]
    fn fit_within_preserves_aspect_and_bounds_memory() {
        let img = decode_png(INTERLACED_PNG).expect("fixture");
        let fitted = img.fit_within(4);
        assert_eq!((fitted.width, fitted.height), (4, 4));
        assert_eq!(fitted.pixels.len(), 64);
        // Already-small images are returned untouched (zero-copy path).
        let small = decode_png(RGBA8_PNG).expect("fixture");
        let same = small.clone().fit_within(64);
        assert_eq!(same.pixels, small.pixels);
    }

    #[test]
    fn rejects_corrupt_and_truncated_input() {
        // Truncated stream.
        let cut = &RGBA8_PNG[..RGBA8_PNG.len() - 20];
        assert!(decode_png(cut).is_none());
        // Corrupted chunk CRC.
        let mut bad = RGBA8_PNG.to_vec();
        let idx = bad.len() - 8;
        bad[idx] ^= 0xFF;
        assert!(decode_png(&bad).is_none());
        // Not a PNG at all.
        assert!(decode_png(b"nope").is_none());
        assert!(decode_png(&[]).is_none());
    }
}
