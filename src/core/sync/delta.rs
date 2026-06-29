//! rsync-style rolling-checksum block delta.
//!
//! The destination (which holds the old version of a file) produces a list of
//! per-block checksums (a fast 32-bit rolling "weak" sum plus a truncated strong
//! hash). The source rolls the same weak sum byte-by-byte across the new file;
//! on a weak hit it confirms with the strong hash and emits a *copy* op that
//! references a block of the old file, otherwise it accumulates *literal* bytes.
//! The destination then rebuilds the new file from its old copy plus the literal
//! data, transferring only the changed regions over the wire.
//!
//! Only full-size blocks participate in matching; the (sub-block) tail of the
//! new file is always sent as a literal. Correctness never depends on the block
//! matching: the caller verifies the whole-file hash after applying the delta.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// rsync's default minimum block length.
const BLOCK_SIZE: usize = 700;
/// Cap on block length for very large files (rsync MAX_BLOCK_SIZE is 128 KiB).
const MAX_BLOCK_SIZE: usize = 128 * 1024;
/// Length the strong per-block hash is truncated to. 16 bytes makes a false
/// block match astronomically unlikely; the whole-file hash is the backstop.
pub const STRONG_LEN: usize = 16;

const OP_LITERAL: u8 = 0x00;
const OP_COPY: u8 = 0x01;
const OP_END: u8 = 0x02;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockSum {
    pub weak: u32,
    pub strong: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockSums {
    pub block_len: u32,
    pub file_size: u64,
    pub blocks: Vec<BlockSum>,
}

/// rsync's block-length heuristic: ~sqrt(len), clamped to [BLOCK_SIZE, MAX_BLOCK_SIZE].
pub fn block_len_for(len: u64) -> usize {
    if len <= (BLOCK_SIZE * BLOCK_SIZE) as u64 {
        return BLOCK_SIZE;
    }
    let mut blength = (len as f64).sqrt() as usize;
    // round down to a multiple of 8, like rsync.
    blength &= !7;
    blength.clamp(BLOCK_SIZE, MAX_BLOCK_SIZE)
}

fn strong_hash(block: &[u8]) -> Vec<u8> {
    blake3::hash(block).as_bytes()[..STRONG_LEN].to_vec()
}

/// Weak rolling checksum (classic rsync adler-style, CHAR_OFFSET = 0).
#[derive(Clone, Copy)]
struct Rolling {
    a: u32,
    b: u32,
    len: u32,
}

impl Rolling {
    fn new(window: &[u8]) -> Self {
        let len = window.len() as u32;
        let mut a: u32 = 0;
        let mut b: u32 = 0;
        for (i, &byte) in window.iter().enumerate() {
            a = a.wrapping_add(byte as u32);
            b = b.wrapping_add((len - i as u32).wrapping_mul(byte as u32));
        }
        Self { a, b, len }
    }

    fn digest(&self) -> u32 {
        (self.a & 0xffff) | (self.b << 16)
    }

    /// Slide the window forward by one byte: drop `out`, append `inb`.
    fn roll(&mut self, out: u8, inb: u8) {
        self.a = self.a.wrapping_sub(out as u32).wrapping_add(inb as u32);
        self.b = self
            .b
            .wrapping_sub(self.len.wrapping_mul(out as u32))
            .wrapping_add(self.a);
    }
}

/// Compute block checksums for an existing file's contents.
pub fn compute_block_sums(data: &[u8], block_len: usize) -> Vec<BlockSum> {
    let mut blocks = Vec::new();
    if block_len == 0 {
        return blocks;
    }
    let mut offset = 0;
    while offset + block_len <= data.len() {
        let block = &data[offset..offset + block_len];
        blocks.push(BlockSum {
            weak: Rolling::new(block).digest(),
            strong: strong_hash(block),
        });
        offset += block_len;
    }
    blocks
}

/// Build an encoded delta turning the destination's old file (described by
/// `sums`) into `new_data`. Returns the encoded op stream.
pub fn build_delta(new_data: &[u8], sums: &BlockSums) -> Vec<u8> {
    let block_len = sums.block_len as usize;
    let mut out = Vec::new();
    if block_len == 0 || sums.blocks.is_empty() || new_data.len() < block_len {
        emit_literal(&mut out, new_data);
        out.push(OP_END);
        return out;
    }

    let mut map: HashMap<u32, Vec<usize>> = HashMap::new();
    for (idx, block) in sums.blocks.iter().enumerate() {
        map.entry(block.weak).or_default().push(idx);
    }

    let mut i = 0usize; // window start
    let mut lit_start = 0usize;
    let mut roll = Rolling::new(&new_data[0..block_len]);

    loop {
        let mut matched: Option<usize> = None;
        if let Some(candidates) = map.get(&roll.digest()) {
            let strong = strong_hash(&new_data[i..i + block_len]);
            for &bi in candidates {
                if sums.blocks[bi].strong == strong {
                    matched = Some(bi);
                    break;
                }
            }
        }

        if let Some(bi) = matched {
            if lit_start < i {
                emit_literal(&mut out, &new_data[lit_start..i]);
            }
            emit_copy(&mut out, bi as u64 * block_len as u64, block_len as u32);
            i += block_len;
            lit_start = i;
            if i + block_len > new_data.len() {
                break;
            }
            roll = Rolling::new(&new_data[i..i + block_len]);
        } else {
            if i + block_len >= new_data.len() {
                break;
            }
            roll.roll(new_data[i], new_data[i + block_len]);
            i += 1;
        }
    }

    if lit_start < new_data.len() {
        emit_literal(&mut out, &new_data[lit_start..]);
    }
    out.push(OP_END);
    out
}

fn emit_literal(out: &mut Vec<u8>, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    out.push(OP_LITERAL);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
}

fn emit_copy(out: &mut Vec<u8>, offset: u64, len: u32) {
    out.push(OP_COPY);
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
}

/// Apply an encoded delta, using `old` (a seekable reader over the destination's
/// existing file) for copy ops, writing the rebuilt file to `out`.
pub fn apply_delta<R: Read + Seek, W: Write>(old: &mut R, delta: &[u8], out: &mut W) -> Result<()> {
    let mut pos = 0usize;
    loop {
        let tag = *delta
            .get(pos)
            .ok_or_else(|| anyhow::anyhow!("delta truncated"))?;
        pos += 1;
        match tag {
            OP_LITERAL => {
                let len = read_u32(delta, &mut pos)? as usize;
                let end = pos
                    .checked_add(len)
                    .filter(|end| *end <= delta.len())
                    .ok_or_else(|| anyhow::anyhow!("delta literal overruns buffer"))?;
                out.write_all(&delta[pos..end])?;
                pos = end;
            }
            OP_COPY => {
                let offset = read_u64(delta, &mut pos)?;
                let len = read_u32(delta, &mut pos)? as usize;
                old.seek(SeekFrom::Start(offset))?;
                let mut remaining = len;
                let mut buf = [0_u8; 64 * 1024];
                while remaining > 0 {
                    let take = remaining.min(buf.len());
                    old.read_exact(&mut buf[..take])?;
                    out.write_all(&buf[..take])?;
                    remaining -= take;
                }
            }
            OP_END => break,
            other => bail!("unknown delta op {other}"),
        }
    }
    Ok(())
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let end = *pos + 4;
    if end > buf.len() {
        bail!("delta truncated reading u32");
    }
    let value = u32::from_le_bytes(buf[*pos..end].try_into().unwrap());
    *pos = end;
    Ok(value)
}

fn read_u64(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let end = *pos + 8;
    if end > buf.len() {
        bail!("delta truncated reading u64");
    }
    let value = u64::from_le_bytes(buf[*pos..end].try_into().unwrap());
    *pos = end;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip(old: &[u8], new: &[u8]) -> Vec<u8> {
        let block_len = block_len_for(old.len() as u64);
        let sums = BlockSums {
            block_len: block_len as u32,
            file_size: old.len() as u64,
            blocks: compute_block_sums(old, block_len),
        };
        let delta = build_delta(new, &sums);
        let mut out = Vec::new();
        apply_delta(&mut Cursor::new(old.to_vec()), &delta, &mut out).unwrap();
        out
    }

    #[test]
    fn rolling_matches_recompute_after_roll() {
        let data = b"the quick brown fox jumps over";
        let mut roll = Rolling::new(&data[0..8]);
        roll.roll(data[0], data[8]);
        let fresh = Rolling::new(&data[1..9]);
        assert_eq!(roll.digest(), fresh.digest());
    }

    #[test]
    fn identical_files_roundtrip() {
        let data: Vec<u8> = (0..50_000).map(|i| (i * 7 % 251) as u8).collect();
        assert_eq!(roundtrip(&data, &data), data);
    }

    #[test]
    fn prepended_bytes_roundtrip() {
        let old: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
        let mut new = b"PREFIX-CHANGED-".to_vec();
        new.extend_from_slice(&old);
        assert_eq!(roundtrip(&old, &new), new);
    }

    #[test]
    fn middle_edit_roundtrip() {
        let mut old: Vec<u8> = (0..40_000).map(|i| (i % 251) as u8).collect();
        let mut new = old.clone();
        for b in new.iter_mut().skip(15_000).take(300) {
            *b = b'Z';
        }
        old.truncate(40_000);
        assert_eq!(roundtrip(&old, &new), new);
    }

    #[test]
    fn delta_is_smaller_for_small_edit() {
        let old: Vec<u8> = (0..200_000).map(|i| (i * 13 % 251) as u8).collect();
        let mut new = old.clone();
        new[100_000] ^= 0xff;
        let block_len = block_len_for(old.len() as u64);
        let sums = BlockSums {
            block_len: block_len as u32,
            file_size: old.len() as u64,
            blocks: compute_block_sums(&old, block_len),
        };
        let delta = build_delta(&new, &sums);
        assert!(
            delta.len() < new.len() / 2,
            "delta {} should be far smaller than file {}",
            delta.len(),
            new.len()
        );
        let mut out = Vec::new();
        apply_delta(&mut Cursor::new(old), &delta, &mut out).unwrap();
        assert_eq!(out, new);
    }

    #[test]
    fn empty_and_tiny_files_roundtrip() {
        assert_eq!(roundtrip(b"", b"hello"), b"hello");
        assert_eq!(roundtrip(b"abc", b""), b"");
        assert_eq!(roundtrip(b"abc", b"abcd"), b"abcd");
    }
}
