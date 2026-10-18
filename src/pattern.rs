//! Test data pattern and per-sector classification.
//!
//! A 512-byte sector is 64 little-endian u64 words. In the seeded pattern
//! `w[0]` is the sector's byte offset in the test stream, `w[1]` is the run tag
//! and the rest is pseudo-random data derived from (tag, offset). A sector that
//! does not match can therefore be recognised as a valid sector of *another*
//! offset (aliased, the fake-capacity wraparound) or of *another run* (stale:
//! the drive dropped our write and still holds old data).
//!
//! `H2testw` reproduces h2testw 1.4's pattern so existing `.h2w` files can be
//! verified (see docs/h2testw-analysis.md).

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

pub const SECTOR: usize = 512;
const WORDS: usize = SECTOR / 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Pattern {
    Seeded { tag: u64 },
    H2testw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Ok,
    /// Fewer than 8 bits differ.
    Changed,
    Corrupted,
    /// Holds a valid sector of this run written at the given other offset.
    Aliased(u64),
    /// Holds a valid sector of a different run: the write never landed.
    Stale,
}

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Fresh random run tag (never zero).
pub fn new_tag() -> u64 {
    let mut b = [0u8; 8];
    getrandom(&mut b);
    let t = u64::from_le_bytes(b);
    if t == 0 { 1 } else { t }
}

fn getrandom(buf: &mut [u8]) {
    // std's RandomState is seeded from the OS RNG; hashing a counter through it
    // gives unpredictable bytes without another dependency.
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    for (i, chunk) in buf.chunks_mut(8).enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(t ^ i as u64);
        let v = h.finish().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
}

fn words(p: Pattern, off: u64, w: &mut [u64; WORDS]) {
    match p {
        Pattern::Seeded { tag } => seeded_words(tag, off, w),
        Pattern::H2testw => h2w_words(off, w),
    }
}

/// One xorshift64 step.
#[inline]
pub fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

fn seeded_words(tag: u64, off: u64, w: &mut [u64; WORDS]) {
    w[0] = off;
    w[1] = tag;
    let mut x = splitmix(tag ^ splitmix(off)) | 1;
    for v in w.iter_mut().skip(2) {
        x = xorshift(x);
        *v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    }
}

fn h2w_words(off: u64, w: &mut [u64; WORDS]) {
    w[0] = off;
    for i in 1..WORDS {
        w[i] = w[i - 1].wrapping_mul(0x0000_0001_0000_000F).wrapping_add(0x11);
    }
}

fn to_bytes(w: &[u64; WORDS], out: &mut [u8]) {
    for (i, v) in w.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
}

fn word_at(data: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(data[i * 8..i * 8 + 8].try_into().unwrap())
}

impl Pattern {
    /// Fill `buf` (a multiple of 512 bytes) with the sectors starting at stream offset `start`.
    pub fn fill(&self, start: u64, buf: &mut [u8]) {
        let p = *self;
        buf.par_chunks_mut(SECTOR * 64).enumerate().for_each(|(ci, chunk)| {
            let mut w = [0u64; WORDS];
            let base = start + (ci * SECTOR * 64) as u64;
            for (si, sec) in chunk.chunks_mut(SECTOR).enumerate() {
                words(p, base + (si * SECTOR) as u64, &mut w);
                to_bytes(&w, sec);
            }
        });
    }

    /// Classify one sector read back from stream offset `off`.
    /// Returns the class plus (expected, found) for the first differing word.
    pub fn classify(&self, off: u64, data: &[u8]) -> (Class, Option<(u64, u64)>) {
        let mut w = [0u64; WORDS];
        words(*self, off, &mut w);
        let mut bits = 0u32;
        let mut first = None;
        for (i, &e) in w.iter().enumerate() {
            let f = word_at(data, i);
            if e != f {
                bits += (e ^ f).count_ones();
                first.get_or_insert((e, f));
            }
        }
        if bits == 0 {
            return (Class::Ok, None);
        }
        // Is it a perfectly valid sector of some other offset / run?
        let o2 = word_at(data, 0);
        if o2.is_multiple_of(SECTOR as u64) {
            let other = match *self {
                Pattern::Seeded { tag } => {
                    let t2 = word_at(data, 1);
                    if t2 != 0 { Some((Pattern::Seeded { tag: t2 }, t2 == tag)) } else { None }
                }
                Pattern::H2testw => Some((Pattern::H2testw, true)),
            };
            if let Some((p2, same_run)) = other
                && !(same_run && o2 == off) && p2.matches(o2, data) {
                    let c = if same_run { Class::Aliased(o2) } else { Class::Stale };
                    return (c, first);
                }
        }
        if bits < 8 { (Class::Changed, first) } else { (Class::Corrupted, first) }
    }

    fn matches(&self, off: u64, data: &[u8]) -> bool {
        let mut w = [0u64; WORDS];
        words(*self, off, &mut w);
        w.iter().enumerate().all(|(i, &e)| word_at(data, i) == e)
    }

    /// Fast check of a whole buffer; returns true if every sector matches.
    pub fn buffer_ok(&self, start: u64, data: &[u8]) -> bool {
        let p = *self;
        data.par_chunks(SECTOR * 64).enumerate().all(|(ci, chunk)| {
            let base = start + (ci * SECTOR * 64) as u64;
            chunk.chunks(SECTOR).enumerate().all(|(si, sec)| p.matches(base + (si * SECTOR) as u64, sec))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_alias() {
        let p = Pattern::Seeded { tag: 42 };
        let mut buf = vec![0u8; SECTOR * 4];
        p.fill(1 << 20, &mut buf);
        assert!(p.buffer_ok(1 << 20, &buf));
        assert_eq!(p.classify(1 << 20, &buf[..SECTOR]).0, Class::Ok);
        // the sector written for offset 1 MiB, read back at 9 MiB -> aliased
        assert_eq!(p.classify(9 << 20, &buf[..SECTOR]).0, Class::Aliased(1 << 20));
        // same offset, data from another run -> stale
        let q = Pattern::Seeded { tag: 43 };
        assert_eq!(q.classify(1 << 20, &buf[..SECTOR]).0, Class::Stale);
        // flip 3 bits -> changed; trash it -> corrupted
        let mut s = buf[..SECTOR].to_vec();
        s[100] ^= 0b111;
        assert_eq!(p.classify(1 << 20, &s).0, Class::Changed);
        let z = vec![0u8; SECTOR];
        assert_eq!(p.classify(1 << 20, &z).0, Class::Corrupted);
    }

    #[test]
    fn h2testw_pattern_matches_known_words() {
        let mut w = [0u64; WORDS];
        h2w_words(0, &mut w);
        assert_eq!(w[1], 0x11);
        assert_eq!(w[2], 0x11u64.wrapping_mul(0x1_0000_000F) + 0x11);
        let p = Pattern::H2testw;
        let mut buf = vec![0u8; SECTOR * 2];
        p.fill(0, &mut buf);
        assert_eq!(p.classify(SECTOR as u64 * 7, &buf[SECTOR..]).0, Class::Aliased(SECTOR as u64));
    }
}
