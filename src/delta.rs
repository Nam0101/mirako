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
        let dir = tempfile::tempdir().unwrap();
        let old_path = dir.path().join("old");
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

    fn old_file(data: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old");
        fs::write(&path, data).unwrap();
        (dir, path)
    }

    fn head(new: &[u8]) -> DeltaChunk {
        DeltaChunk {
            path: "app/build/x.apk".into(),
            mode: 0o644,
            mtime_ns: 1_700_000_000_123_456_789,
            size: new.len() as u64,
            hash: *blake3::hash(new).as_bytes(),
            ops: Vec::new(),
            last: false,
        }
    }

    fn op_wire(op: &Op) -> u64 {
        match op {
            Op::Data(d) => d.len() as u64,
            Op::Copy { .. } => 8,
        }
    }

    #[test]
    fn delta_worthwhile_boundaries() {
        assert!(!delta_worthwhile(0));
        assert!(!delta_worthwhile(MIN_DELTA_SIZE - 1));
        assert!(delta_worthwhile(MIN_DELTA_SIZE));
        assert!(delta_worthwhile(MAX_DELTA_SIZE));
        assert!(!delta_worthwhile(MAX_DELTA_SIZE + 1));
    }

    #[test]
    fn signature_of_an_empty_file_has_no_blocks() {
        let (_d, path) = old_file(&[]);
        let sig = signature(&path).unwrap();
        assert_eq!((sig.block, sig.size), (BLOCK as u32, 0));
        assert!(sig.blocks.is_empty());
    }

    #[test]
    fn signature_counts_whole_blocks_and_a_trailing_partial_one() {
        let data = noise(3 * BLOCK + 1, 11);
        let (_d, exact) = old_file(&data[..3 * BLOCK]);
        let sig = signature(&exact).unwrap();
        assert_eq!((sig.size, sig.blocks.len()), (3 * BLOCK as u64, 3));
        let (_d2, plus) = old_file(&data);
        let sig2 = signature(&plus).unwrap();
        assert_eq!((sig2.size, sig2.blocks.len()), (3 * BLOCK as u64 + 1, 4));
        assert_eq!(sig.blocks[..], sig2.blocks[..3]);
        let (a, b) = weak(&data[3 * BLOCK..]);
        assert_eq!(sig2.blocks[3], (a | (b << 16), strong(&data[3 * BLOCK..])));
    }

    #[test]
    fn rolling_checksum_matches_a_fresh_one_after_every_byte() {
        let data = noise(BLOCK + 500, 12);
        let b = BLOCK as u32;
        let (mut a, mut bb) = weak(&data[..BLOCK]);
        for i in 0..500 {
            let out = data[i] as u32;
            let inn = data[i + BLOCK] as u32;
            a = a.wrapping_add(inn).wrapping_sub(out) & 0xffff;
            bb = bb.wrapping_sub(b.wrapping_mul(out)).wrapping_add(a) & 0xffff;
            assert_eq!((a, bb), weak(&data[i + 1..i + 1 + BLOCK]), "at {i}");
        }
    }

    #[test]
    fn data_shifted_by_one_byte_still_copies_almost_everything() {
        let old = noise(10 * BLOCK, 13);
        let mut new = vec![0xAB];
        new.extend_from_slice(&old);
        let (out, literal, copies) = rebuild(&old, &new);
        assert_eq!(out, new);
        assert!(literal < 100, "literal {literal}");
        assert_eq!(copies, 1);
    }

    #[test]
    fn deletion_append_prepend_and_truncation_rebuild() {
        let old = noise(12 * BLOCK + 321, 14);
        let mut deleted = old.clone();
        deleted.drain(5 * BLOCK + 10..6 * BLOCK + 4000);
        let mut appended = old.clone();
        appended.extend(noise(7000, 15));
        let mut prepended = noise(3000, 16);
        prepended.extend_from_slice(&old);
        let truncated = old[..8 * BLOCK + 77].to_vec();
        for (name, new) in [
            ("deleted", deleted),
            ("appended", appended),
            ("prepended", prepended),
            ("truncated", truncated),
        ] {
            let (out, literal, copies) = rebuild(&old, &new);
            assert_eq!(out, new, "{name}");
            assert!(literal < 2 * BLOCK + 8000, "{name} literal {literal}");
            assert!(copies >= 1, "{name}");
        }
    }

    #[test]
    fn the_trailing_partial_block_of_the_old_file_is_never_a_copy_source() {
        let old = noise(3 * BLOCK + 100, 17);
        let mut new = old.clone();
        let n = new.len();
        new[n - 100..].copy_from_slice(&noise(100, 18));
        let (_d, path) = old_file(&old);
        let sig = signature(&path).unwrap();
        let mut ops = Vec::new();
        delta(&new, &sig, |op| {
            ops.push(op);
            Ok(())
        })
        .unwrap();
        assert!(ops.iter().all(|o| !matches!(o, Op::Copy { index, count } if index + count > 3)));
        assert_eq!(rebuild(&old, &new).0, new);
        // the same old tail, unchanged, is resent as a literal too
        assert_eq!(rebuild(&old, &old).0, old);
    }

    #[test]
    fn stream_splits_a_large_unrelated_file_into_chunk_sized_frames() {
        let old = noise(2 * BLOCK, 19);
        let new = noise(10 * 1024 * 1024, 20);
        let (_d, path) = old_file(&old);
        let sig = signature(&path).unwrap();
        let h = head(&new);
        let mut frames = Vec::new();
        let wire = stream(&new, &sig, &h, |f| {
            frames.push(f);
            Ok(())
        })
        .unwrap();
        assert!(frames.len() >= 3, "{} frames", frames.len());
        let (last, rest) = frames.split_last().unwrap();
        assert!(last.last);
        for f in rest {
            assert!(!f.last);
            let data: usize = f.ops.iter().map(|o| if let Op::Data(d) = o { d.len() } else { 0 }).sum();
            assert!(data >= CHUNK, "frame data {data}");
        }
        for f in &frames {
            assert_eq!(f.path, h.path);
            assert_eq!((f.mode, f.mtime_ns, f.size, f.hash), (h.mode, h.mtime_ns, h.size, h.hash));
        }
        let sum: u64 = frames.iter().flat_map(|f| &f.ops).map(op_wire).sum();
        assert_eq!(wire, sum);

        let ops: Vec<Op> = frames.into_iter().flat_map(|f| f.ops).collect();
        let mut out = Vec::new();
        let mut hasher = blake3::Hasher::new();
        apply(
            Some(&mut fs::File::open(&path).unwrap()),
            old.len() as u64,
            sig.block,
            &ops,
            &mut out,
            &mut hasher,
        )
        .unwrap();
        assert_eq!(out, new);
        assert_eq!(*hasher.finalize().as_bytes(), h.hash);
    }

    #[test]
    fn stream_of_an_identical_file_is_one_final_frame() {
        let data = noise(6 * BLOCK, 21);
        let (_d, path) = old_file(&data);
        let sig = signature(&path).unwrap();
        let mut frames = Vec::new();
        let wire = stream(&data, &sig, &head(&data), |f| {
            frames.push(f);
            Ok(())
        })
        .unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].last);
        assert!(matches!(frames[0].ops[..], [Op::Copy { index: 0, count: 6 }]));
        assert_eq!(wire, 8);
    }

    #[test]
    fn apply_copy_without_an_old_file_errors() {
        let mut out = Vec::new();
        let err = apply(
            None,
            0,
            BLOCK as u32,
            &[Op::Copy { index: 0, count: 1 }],
            &mut out,
            &mut blake3::Hasher::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("without an old file"), "{err}");
    }

    #[test]
    fn apply_clamps_a_copy_range_past_the_end_of_the_old_file() {
        let old = noise(3 * BLOCK + 100, 22);
        let (_d, path) = old_file(&old);
        let mut out = Vec::new();
        let ops = [Op::Copy { index: 0, count: 4 }];
        apply(
            Some(&mut fs::File::open(&path).unwrap()),
            old.len() as u64,
            BLOCK as u32,
            &ops,
            &mut out,
            &mut blake3::Hasher::new(),
        )
        .unwrap();
        assert_eq!(out, old);
        // a run starting past the end copies nothing
        let mut out = Vec::new();
        let ops = [Op::Copy { index: 9, count: 2 }];
        apply(
            Some(&mut fs::File::open(&path).unwrap()),
            old.len() as u64,
            BLOCK as u32,
            &ops,
            &mut out,
            &mut blake3::Hasher::new(),
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn apply_errors_on_a_corrupt_data_op() {
        let mut out = Vec::new();
        let ops = [Op::Data(b"not zstd at all".to_vec())];
        assert!(apply(None, 0, BLOCK as u32, &ops, &mut out, &mut blake3::Hasher::new()).is_err());
    }
}
