//! Wire protocol between the local client and the `mirako serve` agent on the remote.
//! Frames are `u32` big-endian length + bincode payload, in both directions over ssh stdio.
//!
//! Transfers are pipelined: the sender streams `Put`/`Delta` frames without waiting and only
//! `Flush` is answered, so the round-trip latency of the link is paid once per phase, not per file.

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::cell::RefCell;
use std::io::{Read, Write};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Files are sent in chunks so a 100 MB APK never has to sit in memory twice.
pub const CHUNK: usize = 4 * 1024 * 1024;
pub const ZSTD_LEVEL: i32 = 3;
const MAX_FRAME: u32 = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    File,
    Symlink { target: String },
}

/// One file of a manifest. `path` is relative to the project root, `/`-separated.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub hash: [u8; 32],
}

/// A piece of a whole-file transfer, in either direction.
#[derive(Serialize, Deserialize, Debug)]
pub struct Chunk {
    pub path: String,
    pub mode: u32,
    pub mtime_ns: i64,
    pub size: u64,
    pub offset: u64,
    pub last: bool,
    /// zstd-compressed bytes
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// Block checksums of the receiver's old copy of a file (rsync algorithm).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Signature {
    pub block: u32,
    pub size: u64,
    /// (weak rolling checksum, truncated blake3) per block, in order
    pub blocks: Vec<(u32, [u8; 16])>,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Op {
    /// `count` consecutive blocks of the old file starting at `index`
    Copy { index: u32, count: u32 },
    /// zstd-compressed literal bytes
    Data(#[serde(with = "serde_bytes")] Vec<u8>),
}

/// A piece of a delta transfer. The receiver rebuilds the file from its old copy + ops.
#[derive(Serialize, Deserialize, Debug)]
pub struct DeltaChunk {
    pub path: String,
    pub mode: u32,
    pub mtime_ns: i64,
    pub size: u64,
    /// blake3 of the complete new file, verified after rebuilding
    pub hash: [u8; 32],
    pub ops: Vec<Op>,
    pub last: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Req {
    Hello {
        version: String,
    },
    /// Scan `dir` (remote path, `~` allowed) minus `exclude` patterns.
    Manifest {
        dir: String,
        exclude: Vec<String>,
    },
    Put(Chunk),
    Delta(DeltaChunk),
    Symlink {
        path: String,
        target: String,
    },
    Delete {
        paths: Vec<String>,
    },
    /// Block signatures of the remote's current copy of `paths` (for a delta push).
    Sigs {
        dir: String,
        paths: Vec<String>,
    },
    /// Answered with `Ack` once everything before it has been applied.
    Flush,
    Exec {
        dir: String,
        cmd: Vec<String>,
    },
    /// `sigs` carries the local old copies' signatures: those files come back as deltas.
    Fetch {
        dir: String,
        paths: Vec<String>,
        sigs: Vec<(String, Signature)>,
    },
    Bye,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Resp {
    Hello {
        version: String,
        home: String,
        os: String,
    },
    Manifest {
        root: String,
        entries: Vec<Entry>,
    },
    /// `failed`: delta pushes whose rebuilt file did not match its hash; resend them whole.
    Ack {
        failed: Vec<String>,
    },
    Sigs(Vec<(String, Signature)>),
    Output {
        stderr: bool,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    Exit {
        code: i32,
    },
    Put(Chunk),
    Delta(DeltaChunk),
    Symlink {
        path: String,
        target: String,
    },
    End,
    Error {
        msg: String,
    },
}

thread_local! {
    static COMPRESSOR: RefCell<zstd::bulk::Compressor<'static>> = RefCell::new(zstd::bulk::Compressor::new(ZSTD_LEVEL).expect("zstd"));
    static DECOMPRESSOR: RefCell<zstd::bulk::Decompressor<'static>> = RefCell::new(zstd::bulk::Decompressor::new().expect("zstd"));
}

/// zstd with a per-thread context kept across calls: `encode_all` would allocate a fresh one
/// per chunk, which costs more than compressing a small file.
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    Ok(COMPRESSOR.with(|c| c.borrow_mut().compress(data))?)
}

pub fn decompress(z: &[u8]) -> Result<Vec<u8>> {
    match zstd::zstd_safe::get_frame_content_size(z) {
        // nothing legitimately decompresses past one chunk plus one delta block
        Ok(Some(n)) if n > 2 * CHUNK as u64 => bail!("zstd frame of {n} bytes exceeds the chunk limit"),
        Ok(Some(n)) => Ok(DECOMPRESSOR.with(|d| d.borrow_mut().decompress(z, n as usize))?),
        // frames from a streaming encoder carry no size: fall back to the streaming decoder
        _ => Ok(zstd::decode_all(z)?),
    }
}

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    let bytes = bincode::serialize(msg)?;
    let len = u32::try_from(bytes.len()).context("frame too large")?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&bytes)?;
    Ok(())
}

pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).context("connection closed")?;
    let len = u32::from_be_bytes(len);
    if len > MAX_FRAME {
        bail!("frame of {len} bytes exceeds the limit");
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(bincode::deserialize(&buf)?)
}
