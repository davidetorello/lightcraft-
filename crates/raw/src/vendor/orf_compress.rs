//! Olympus lossless RAW compression decoder.
//!
//! Derived from CC0 sample files and the two Olympus patents that describe the method (neither names
//! ORF and neither gives the file layout):
//!
//! - US7333035B2 (Ishikawa 2007): MED prediction from left/upper/upper-left neighbours; sign coded
//!   separately from magnitude; Golomb-Rice entropy coding; flatness run counters that control the
//!   Rice parameter `k`; counters reset per row.
//! - US7715639B2 (Ishikawa 2010): sibling with predictor variants and `k` derived from neighbours.
//!
//! # Stream layout (derived from CC0 files; not in the patents)
//!
//! The strip starts with a 16-bit big-endian header word:
//! - bits \[15:11\] = `bit_depth − 1` (12-bit Bayer → 11; always 11 on known DSLR/MILC bodies).
//! - bits \[10:0\] = 0 on every known body.
//!
//! After the header the stream is MSB-first packed bits, row by row.
//!
//! # Coding per pixel (US7333035B2 §§\[0038\]–\[0090\])
//!
//! 1. **Predict** with MED from left (A), upper (B), upper-left (C):
//!    `if C >= max(A,B) → min(A,B); elif C <= min(A,B) → max(A,B); else A+B−C`.
//!    For column 0: A = value above on all rows (0 on row 0); same for C.
//! 2. **Residual** = pixel − predict.
//! 3. **Golomb-Rice code** for the magnitude: `(|res| >> k)` one-bits, a zero-bit, then
//!    the k low bits of `|res|`.
//! 4. **Sign bit** (only when magnitude ≠ 0): 1 → positive, 0 → negative.
//! 5. **Update k** from flatness-run counters (see [`rice_k_next`]).

use crate::{RawError, Result};

/// Decode one Olympus compressed ORF strip.
///
/// `src` is the raw byte content of the single TIFF strip.
/// `out.len()` must equal `width * height`; `width > 0 && height > 0`.
pub(super) fn decode_strip(src: &[u8], width: usize, height: usize, out: &mut [u16]) -> Result<()> {
    if src.len() < 2 {
        return Err(RawError::Corrupt("ORF compressed: strip too short for header".into()));
    }
    // 16-bit big-endian header: bits [15:11] = bit_depth − 1.
    let header = (src[0] as u16) << 8 | src[1] as u16;
    let bit_depth = ((header >> 11) & 0x1f) as u32 + 1;
    if !(8..=16).contains(&bit_depth) {
        return Err(RawError::Corrupt(format!("ORF compressed: invalid bit depth {bit_depth} in header (header=0x{header:04x})")));
    }
    let max_val = (1u32 << bit_depth) - 1;

    // Bit offset: skip the 16-bit header.
    let mut bs = Bitstream::new(src, 16);

    let mut prev_row = vec![0i32; width];
    let mut cur_row = vec![0i32; width];

    for y in 0..height {
        let mut n_flat: u32 = 0;
        let mut k: u32 = 0;

        for x in 0..width {
            // MED neighbours: A=left, B=upper, C=upper-left.
            // Column 0: use the pixel directly above as A and C (mirrors dcraw practice and the
            // "border predictor" described in §[0046]).
            let a = if x > 0 { cur_row[x - 1] } else { prev_row[0] };
            let b = prev_row[x];
            let c = if x > 0 { prev_row[x - 1] } else { prev_row[0] };
            let predict = med(a, b, c);

            let magnitude = bs.read_golomb_rice(k)?;
            let residual: i32 = if magnitude == 0 {
                0
            } else {
                let sign = bs.read_bit()?;
                if sign != 0 { magnitude as i32 } else { -(magnitude as i32) }
            };

            let sample = (predict + residual).clamp(0, max_val as i32);
            cur_row[x] = sample;
            out[y * width + x] = sample as u16;

            if residual == 0 {
                n_flat = n_flat.saturating_add(1);
            } else {
                n_flat = 0;
            }
            k = rice_k_next(n_flat, magnitude, k, bit_depth);
        }

        std::mem::swap(&mut prev_row, &mut cur_row);
    }
    Ok(())
}

/// MED (Median Edge Detector) predictor (LOCO-I / ITU-T T.87, also US7333035B2 §\[0038\]).
///
/// Neighbours: A = left, B = upper, C = upper-left.
#[inline(always)]
pub(super) fn med(a: i32, b: i32, c: i32) -> i32 {
    if c >= a.max(b) {
        a.min(b)
    } else if c <= a.min(b) {
        a.max(b)
    } else {
        a + b - c
    }
}

/// Derive the next Golomb-Rice parameter k (US7333035B2 §\[0077\]–§\[0082\]).
///
/// - Large residual (magnitude ≥ 2^k): wide distribution → decrease k.
/// - Long flat run (n_flat > 2^k): narrow distribution → increase k.
/// - Otherwise: unchanged.
#[inline(always)]
fn rice_k_next(n_flat: u32, magnitude: u32, k: u32, bit_depth: u32) -> u32 {
    let k_max = bit_depth.saturating_sub(1).min(12);
    if magnitude >= (1u32 << k) {
        k.saturating_sub(1)
    } else if n_flat > (1u32 << k) {
        (k + 1).min(k_max)
    } else {
        k
    }
}

/// MSB-first packed bitstream reader over a byte slice.
struct Bitstream<'a> {
    src: &'a [u8],
    byte: usize,
    bit: u8, // next bit within src[byte], counted from 7 (MSB) down to 0
}

impl<'a> Bitstream<'a> {
    fn new(src: &'a [u8], start_bits: usize) -> Self {
        Self { src, byte: start_bits / 8, bit: 7u8.saturating_sub((start_bits % 8) as u8) }
    }

    #[inline]
    fn read_bit(&mut self) -> Result<u8> {
        let b = self.src.get(self.byte).ok_or_else(|| RawError::Corrupt("ORF compressed: bitstream overrun".into()))?;
        let v = (b >> self.bit) & 1;
        if self.bit == 0 {
            self.byte += 1;
            self.bit = 7;
        } else {
            self.bit -= 1;
        }
        Ok(v)
    }

    #[inline]
    fn read_bits(&mut self, k: u32) -> Result<u32> {
        let mut v = 0u32;
        for _ in 0..k {
            v = (v << 1) | self.read_bit()? as u32;
        }
        Ok(v)
    }

    fn read_golomb_rice(&mut self, k: u32) -> Result<u32> {
        const UNARY_CAP: u32 = 1 << 14;
        let mut q = 0u32;
        loop {
            if q > UNARY_CAP {
                return Err(RawError::Corrupt("ORF compressed: Golomb-Rice unary overflow".into()));
            }
            if self.read_bit()? == 0 {
                break;
            }
            q += 1;
        }
        let r = self.read_bits(k)?;
        Ok((q << k) | r)
    }
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers -----------------------------------------------------------

    struct BitWriter {
        bits: Vec<u8>,
        cur: u8,
        pos: u8,
    }
    impl BitWriter {
        fn new() -> Self {
            Self { bits: Vec::new(), cur: 0, pos: 7 }
        }
        fn write_bit(&mut self, b: u8) {
            self.cur |= (b & 1) << self.pos;
            if self.pos == 0 {
                self.bits.push(self.cur);
                self.cur = 0;
                self.pos = 7;
            } else {
                self.pos -= 1;
            }
        }
        fn write_bits(&mut self, v: u32, k: u32) {
            for i in (0..k).rev() {
                self.write_bit(((v >> i) & 1) as u8);
            }
        }
        fn write_golomb_rice(&mut self, k: u32, magnitude: u32) {
            let q = magnitude >> k;
            let r = magnitude & ((1 << k) - 1);
            for _ in 0..q {
                self.write_bit(1);
            }
            self.write_bit(0);
            self.write_bits(r, k);
        }
        fn finish(mut self) -> Vec<u8> {
            if self.pos != 7 {
                self.bits.push(self.cur);
            }
            self.bits
        }
    }

    /// Encode `pixels` into a synthetic compressed ORF strip (12-bit, big-endian header).
    fn encode(pixels: &[Vec<u16>]) -> Vec<u8> {
        let height = pixels.len();
        let width = if height == 0 { 0 } else { pixels[0].len() };
        let bit_depth: u32 = 12;
        let header: u16 = ((bit_depth - 1) as u16) << 11; // bits [15:11] = 11
        let mut out = vec![(header >> 8) as u8, (header & 0xff) as u8];
        let mut w = BitWriter::new();
        let mut prev_row = vec![0i32; width.max(1)];
        let mut cur_row = vec![0i32; width.max(1)];
        for y in 0..height {
            let mut n_flat = 0u32;
            let mut k = 0u32;
            for x in 0..width {
                let px = pixels[y][x] as i32;
                let a = if x > 0 { cur_row[x - 1] } else { prev_row[0] };
                let b = prev_row[x];
                let c = if x > 0 { prev_row[x - 1] } else { prev_row[0] };
                let predict = med(a, b, c);
                let residual = px - predict;
                let magnitude = residual.unsigned_abs();
                w.write_golomb_rice(k, magnitude);
                if magnitude != 0 {
                    w.write_bit(if residual > 0 { 1 } else { 0 });
                }
                cur_row[x] = px;
                if residual == 0 {
                    n_flat = n_flat.saturating_add(1);
                } else {
                    n_flat = 0;
                }
                k = rice_k_next(n_flat, magnitude, k, bit_depth);
            }
            std::mem::swap(&mut prev_row, &mut cur_row);
        }
        out.extend(w.finish());
        out
    }

    fn decode(pixels: &[Vec<u16>]) -> Vec<u16> {
        let h = pixels.len();
        let w = if h == 0 { 0 } else { pixels[0].len() };
        let strip = encode(pixels);
        let mut out = vec![0u16; w * h];
        decode_strip(&strip, w, h, &mut out).unwrap();
        out
    }

    fn flat(rows: usize, cols: usize, v: u16) -> Vec<Vec<u16>> {
        vec![vec![v; cols]; rows]
    }

    // ---- MED predictor -------------------------------------------------------

    #[test]
    fn med_all_cases() {
        assert_eq!(med(3, 5, 7), 3); // C ≥ max → min(A,B)
        assert_eq!(med(5, 3, 7), 3);
        assert_eq!(med(3, 5, 1), 5); // C ≤ min → max(A,B)
        assert_eq!(med(5, 3, 1), 5);
        assert_eq!(med(3, 5, 4), 4); // else → A+B−C
        assert_eq!(med(4, 4, 4), 4);
    }

    // ---- round-trip tests ---------------------------------------------------

    #[test]
    fn flat_black_roundtrip() {
        let pixels = flat(4, 8, 0);
        assert_eq!(decode(&pixels), pixels.into_iter().flatten().collect::<Vec<_>>());
    }

    #[test]
    fn flat_mid_roundtrip() {
        let pixels = flat(4, 8, 1000);
        assert_eq!(decode(&pixels), pixels.into_iter().flatten().collect::<Vec<_>>());
    }

    #[test]
    fn gradient_roundtrip() {
        let (w, h) = (16usize, 8usize);
        let pixels: Vec<Vec<u16>> = (0..h).map(|y| (0..w).map(|x| ((y * w + x) % 4096) as u16).collect()).collect();
        let expected: Vec<u16> = pixels.iter().flatten().copied().collect();
        assert_eq!(decode(&pixels), expected);
    }

    #[test]
    fn checkerboard_roundtrip() {
        let (w, h) = (16usize, 6usize);
        let pixels: Vec<Vec<u16>> = (0..h).map(|y| (0..w).map(|x| if (x + y) % 2 == 0 { 500u16 } else { 3500u16 }).collect()).collect();
        let expected: Vec<u16> = pixels.iter().flatten().copied().collect();
        assert_eq!(decode(&pixels), expected);
    }

    #[test]
    fn natural_scene_roundtrip() {
        let (w, h) = (32usize, 16usize);
        let pixels: Vec<Vec<u16>> = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| {
                        let base = (y * 100 + x * 30) as u16;
                        let noise = ((x * 7 + y * 13) % 15) as u16;
                        (base + noise).min(4095)
                    })
                    .collect()
            })
            .collect();
        let expected: Vec<u16> = pixels.iter().flatten().copied().collect();
        assert_eq!(decode(&pixels), expected);
    }

    // ---- robustness ----------------------------------------------------------

    #[test]
    fn truncated_strip_is_an_error_not_a_panic() {
        let (w, h) = (8usize, 4usize);
        let full = encode(&flat(h, w, 1000));
        for cut in [0usize, 1, 2, 3, full.len() / 2, full.len() - 1] {
            let _ = decode_strip(&full[..cut], w, h, &mut vec![0u16; w * h]);
            // Must not panic; result may be Ok (cut after all data) or Err.
        }
    }

    #[test]
    fn all_zero_strip_is_ok() {
        // A flat-black image (all pixels = 0) should round-trip cleanly.
        let (w, h) = (8usize, 4usize);
        assert_eq!(decode(&flat(h, w, 0)), vec![0u16; w * h]);
    }

    #[test]
    fn invalid_bit_depth_is_an_error() {
        // bits [15:11] = 0 → depth = 1 (< 8: invalid)
        let strip = vec![0x00u8, 0x00, 0x00, 0x00];
        assert!(decode_strip(&strip, 1, 1, &mut [0u16; 1]).is_err());
        // bits [15:11] = 31 → depth = 32 (> 16: invalid)
        let strip = vec![0xf8u8, 0x00, 0x00, 0x00];
        assert!(decode_strip(&strip, 1, 1, &mut [0u16; 1]).is_err());
    }
}
