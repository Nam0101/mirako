//! Block-level delta transfer (the rsync algorithm): the receiver sends block checksums of its
//! old copy, the sender slides a window over the new file and emits "copy block n" for every
//! window it recognises and literal bytes for the rest. An APK rebuilt after a one-line change
//! shares most of its stored entries with the previous one, so a 90 MB file travels as a few MB.

use crate::proto::{self, DeltaChunk, Op, Signature, CHUNK};
use crate::server::read_full;
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const BLOCK: usize = 64 * 1024;
/// Smaller files are cheaper to send whole than to signature + delta.
pub const MIN_DELTA_SIZE: u64 = 256 * 1024;
/// The sender holds the new file in memory while rolling; cap it.
pub const MAX_DELTA_SIZE: u64 = 1 << 30;

pub fn delta_worthwhile(size: u64) -> bool {
    (MIN_DELTA_SIZE..=MAX_DELTA_SIZE).contains(&size)
}

fn weak(data: &[u8]) -> (u32, u32) {
    let mut a: u32 = 0;
    let mut b: u32 = 0;
    let n = data.len() as u32;
    for (i, &x) in data.iter().enumerate() {
        a = a.wrapping_add(x as u32);
        b = b.wrapping_add((n - i as u32).wrapping_mul(x as u32));
    }
    (a & 0xffff, b & 0xffff)
}

fn strong(data: &[u8]) -> [u8; 16] {
    let h = blake3::hash(data);
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.as_bytes()[..16]);
    out
}

/// Streams the file one block at a time, so a 1 GB output costs 64 KB of memory, not 1 GB.
pub fn signature(path: &Path) -> Result<Signature> {
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0u8; BLOCK];
    let mut blocks = Vec::new();
    let mut size = 0u64;
    loop {
        let n = read_full(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        let (a, b) = weak(&buf[..n]);
        blocks.push((a | (b << 16), strong(&buf[..n])));
        size += n as u64;
    }
    Ok(Signature {
        block: BLOCK as u32,
        size,
        blocks,
    })
}

/// A 2^20-bit filter over the weak checksums: the per-byte rolling loop tests one bit here and
/// only touches the hash map on a hit, so unmatched data costs a few ns per byte, not a SipHash.
const FILTER_BITS: u32 = 20;

fn filter_slot(w: u32) -> (usize, u64) {
    let h = w.wrapping_mul(0x9E37_79B1) >> (32 - FILTER_BITS);
    ((h >> 6) as usize, 1u64 << (h & 63))
}

/// Emits ops describing `new` in terms of `sig`. Literal runs are zstd-compressed in pieces of
/// at most `CHUNK` bytes so the caller can stream them.
pub fn delta(new: &[u8], sig: &Signature, mut emit: impl FnMut(Op) -> Result<()>) -> Result<()> {
    let b = sig.block as usize;
    let n = new.len();
    let mut lookup: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut filter = vec![0u64; 1 << (FILTER_BITS - 6)];
    for (i, (w, _)) in sig.blocks.iter().enumerate() {
        // the last block may be partial; a full window can never equal it
        if i + 1 == sig.blocks.len() && sig.size as usize % b != 0 {
            break;
        }
        lookup.entry(*w).or_default().push(i as u32);
        let (slot, bit) = filter_slot(*w);
        filter[slot] |= bit;
    }

    let mut literal: Vec<u8> = Vec::new();
    let flush_literal = |literal: &mut Vec<u8>, emit: &mut dyn FnMut(Op) -> Result<()>| -> Result<()> {
        if !literal.is_empty() {
            emit(Op::Data(proto::compress(literal)?))?;
            literal.clear();
        }
        Ok(())
    };
    let mut pending_copy: Option<(u32, u32)> = None;
    let flush_copy = |pending: &mut Option<(u32, u32)>, emit: &mut dyn FnMut(Op) -> Result<()>| -> Result<()> {
        if let Some((index, count)) = pending.take() {
            emit(Op::Copy { index, count })?;
        }
        Ok(())
    };

    if n < b || lookup.is_empty() {
        for piece in new.chunks(CHUNK) {
            emit(Op::Data(proto::compress(piece)?))?;
        }
        return Ok(());
    }

    let mut i = 0usize;
    let (mut a, mut bb) = weak(&new[0..b]);
    while i + b <= n {
        let w = a | (bb << 16);
        let (slot, bit) = filter_slot(w);
        let mut matched = None;
        if filter[slot] & bit != 0 {
            if let Some(cands) = lookup.get(&w) {
                let s = strong(&new[i..i + b]);
                matched = cands.iter().copied().find(|&idx| sig.blocks[idx as usize].1 == s);
            }
        }
        if let Some(idx) = matched {
            flush_literal(&mut literal, &mut emit)?;
            match pending_copy {
                Some((start, count)) if start + count == idx => pending_copy = Some((start, count + 1)),
                _ => {
                    flush_copy(&mut pending_copy, &mut emit)?;
                    pending_copy = Some((idx, 1));
                }
            }
            i += b;
            if i + b <= n {
                let (na, nb) = weak(&new[i..i + b]);
                a = na;
                bb = nb;
            }
            continue;
        }
        flush_copy(&mut pending_copy, &mut emit)?;
        literal.push(new[i]);
        if literal.len() >= CHUNK {
            flush_literal(&mut literal, &mut emit)?;
        }
        // roll the window one byte
        let out = new[i] as u32;
        if i + b < n {
            let inn = new[i + b] as u32;
            a = a.wrapping_add(inn).wrapping_sub(out) & 0xffff;
            bb = bb.wrapping_sub((b as u32).wrapping_mul(out)).wrapping_add(a) & 0xffff;
        }
        i += 1;
    }
    flush_copy(&mut pending_copy, &mut emit)?;
    literal.extend_from_slice(&new[i..]);
    flush_literal(&mut literal, &mut emit)?;
    Ok(())
}

/// Runs `delta` and packs the ops into `DeltaChunk` frames of about `CHUNK` bytes, handing each
/// to `emit` as soon as it is full so the transfer overlaps the rolling. Returns the bytes on the wire.
pub fn stream(new: &[u8], sig: &Signature, head: &DeltaChunk, mut emit: impl FnMut(DeltaChunk) -> Result<()>) -> Result<u64> {
    let frame = |ops: Vec<Op>, last: bool| DeltaChunk {
        path: head.path.clone(),
        mode: head.mode,
        mtime_ns: head.mtime_ns,
        size: head.size,
        hash: head.hash,
        ops,
        last,
    };
    let mut ops = Vec::new();
    let mut pending = 0usize;
    let mut wire = 0u64;
    delta(new, sig, |op| {
        let n = match &op {
            Op::Data(d) => d.len(),
            Op::Copy { .. } => 8,
        };
        pending += n;
        wire += n as u64;
        ops.push(op);
        if pending >= CHUNK {
            emit(frame(std::mem::take(&mut ops), false))?;
            pending = 0;
        }
        Ok(())
    })?;
    emit(frame(ops, true))?;
    Ok(wire)
}

/// Rebuilds a file from `old` + `ops` into `out`, feeding every byte through `hasher`.
pub fn apply(
    old: Option<&mut fs::File>,
    old_size: u64,
    block: u32,
    ops: &[Op],
    out: &mut impl Write,
    hasher: &mut blake3::Hasher,
) -> Result<()> {
    let b = block as u64;
    let mut old = old;
    let mut buf = Vec::new();
    for op in ops {
        match op {
            Op::Copy { index, count } => {
                let Some(old) = old.as_deref_mut() else {
                    anyhow::bail!("delta copy without an old file")
                };
                // a run of blocks is one contiguous range of the old file: read it in big pieces
                let start = *index as u64 * b;
                let mut left = (start + *count as u64 * b).min(old_size).saturating_sub(start);
                old.seek(SeekFrom::Start(start))?;
                while left > 0 {
                    let len = left.min(CHUNK as u64) as usize;
                    buf.resize(len, 0);
                    old.read_exact(&mut buf)?;
                    hasher.update(&buf);
                    out.write_all(&buf)?;
                    left -= len as u64;
                }
            }
            Op::Data(z) => {
                let data = proto::decompress(z)?;
                hasher.update(&data);
                out.write_all(&data)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn rebuild(old: &[u8], new: &[u8]) -> (Vec<u8>, usize, usize) {
        let dir = std::env::temp_dir().join(format!("mirako-delta-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let old_path = dir.join(format!("old-{}", new.len()));
        fs::File::create(&old_path).unwrap().write_all(old).unwrap();
        let sig = signature(&old_path).unwrap();
        let mut ops = Vec::new();
        delta(new, &sig, |op| {
            ops.push(op);
            Ok(())
        })
        .unwrap();
        let literal: usize = ops.iter().map(|o| if let Op::Data(d) = o { d.len() } else { 0 }).sum();
        let copies = ops.iter().filter(|o| matches!(o, Op::Copy { .. })).count();
        let mut out = Vec::new();
        let mut h = blake3::Hasher::new();
        let mut f = fs::File::open(&old_path).unwrap();
        apply(Some(&mut f), old.len() as u64, sig.block, &ops, &mut out, &mut h).unwrap();
        assert_eq!(h.finalize(), blake3::hash(new));
        (out, literal, copies)
    }

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn identical_file_is_all_copies() {
        let data = noise(10 * BLOCK + 123, 1);
        let (out, literal, copies) = rebuild(&data, &data);
        assert_eq!(out, data);
        assert!(literal < 200, "literal {literal}");
        assert!(copies >= 1);
    }

    #[test]
    fn insertion_in_the_middle_only_sends_the_change() {
        let old = noise(40 * BLOCK, 2);
        let mut new = old.clone();
        new.splice(BLOCK * 7 + 100..BLOCK * 7 + 100, noise(5000, 3));
        let (out, literal, _) = rebuild(&old, &new);
        assert_eq!(out, new);
        assert!(literal < 2 * BLOCK + 5000, "literal {literal}");
    }

    #[test]
    fn unrelated_files_and_small_files_still_rebuild() {
        let old = noise(3 * BLOCK, 4);
        let new = noise(3 * BLOCK + 17, 5);
        assert_eq!(rebuild(&old, &new).0, new);
        let tiny_old = noise(10, 6);
        let tiny_new = noise(12, 7);
        assert_eq!(rebuild(&tiny_old, &tiny_new).0, tiny_new);
        assert_eq!(rebuild(&old, &[]).0, Vec::<u8>::new());
    }
}
