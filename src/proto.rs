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

/// What `mirako gc`, and every run when `gc_days` is set, asks the agent to clean up.
#[derive(Serialize, Deserialize, Debug)]
pub struct GcReq {
    /// the `remote_folder`: every direct sub-directory mirako has synced before (it has an
    /// index cache there) is a project mirror
    pub folder: String,
    /// remove mirrors not synced for more than this many days; `None` removes none
    pub keep_days: Option<u32>,
    /// the project of this session: never removed, and `build` is deleted inside it
    pub current: Option<String>,
    /// exclude-style patterns deleted inside `current` (intermediates the client never downloads)
    pub build: Vec<String>,
    /// Gradle's cache retention on the host in days (`~/.gradle/init.d/mirako-gc.gradle`); 0 restores its default
    pub gradle_days: u32,
    pub dry_run: bool,
    /// measure every mirror, not only the removed ones
    pub sizes: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Mirror {
    pub name: String,
    /// 0 when not measured
    pub bytes: u64,
    /// `None`: never synced by mirako, left alone
    pub idle_days: Option<u32>,
    pub removed: bool,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct GcReport {
    pub mirrors: Vec<Mirror>,
    /// deleted inside `current` by the `build` patterns
    pub build_bytes: u64,
    /// free space on the volume of `folder`
    pub free: u64,
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
    /// Housekeeping on the host; answered with `Gc`.
    Gc(GcReq),
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
    Gc(GcReport),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip<T: Serialize + DeserializeOwned>(msg: &T) -> T {
        let mut buf = Vec::new();
        write_frame(&mut buf, msg).unwrap();
        let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
        assert_eq!(len, buf.len() - 4, "length header counts the payload only");
        let mut r = Cursor::new(buf);
        let back = read_frame(&mut r).unwrap();
        assert_eq!(r.position() as usize, len + 4, "read_frame consumes exactly one frame");
        back
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
    fn gc_request_survives_a_frame_roundtrip() {
        let req = Req::Gc(GcReq {
            folder: "~/mirako".into(),
            keep_days: Some(14),
            current: Some("app-1234".into()),
            build: vec!["build".into(), "!build/keep".into()],
            gradle_days: 7,
            dry_run: true,
            sizes: false,
        });
        let Req::Gc(g) = roundtrip(&req) else { panic!("wrong variant") };
        assert_eq!(g.folder, "~/mirako");
        assert_eq!(g.keep_days, Some(14));
        assert_eq!(g.current.as_deref(), Some("app-1234"));
        assert_eq!(g.build, vec!["build".to_string(), "!build/keep".to_string()]);
        assert_eq!(g.gradle_days, 7);
        assert!(g.dry_run);
        assert!(!g.sizes);
    }

    #[test]
    fn gc_report_with_mirrors_survives_a_frame_roundtrip() {
        let resp = Resp::Gc(GcReport {
            mirrors: vec![
                Mirror {
                    name: "a".into(),
                    bytes: 1 << 40,
                    idle_days: Some(30),
                    removed: true,
                },
                Mirror {
                    name: "b".into(),
                    bytes: 0,
                    idle_days: None,
                    removed: false,
                },
            ],
            build_bytes: 123,
            free: u64::MAX,
        });
        let Resp::Gc(r) = roundtrip(&resp) else { panic!("wrong variant") };
        assert_eq!(r.mirrors.len(), 2);
        assert_eq!(
            (
                r.mirrors[0].name.as_str(),
                r.mirrors[0].bytes,
                r.mirrors[0].idle_days,
                r.mirrors[0].removed
            ),
            ("a", 1 << 40, Some(30), true)
        );
        assert_eq!(
            (
                r.mirrors[1].name.as_str(),
                r.mirrors[1].bytes,
                r.mirrors[1].idle_days,
                r.mirrors[1].removed
            ),
            ("b", 0, None, false)
        );
        assert_eq!(r.build_bytes, 123);
        assert_eq!(r.free, u64::MAX);
    }

    #[test]
    fn fetch_with_a_signature_survives_a_frame_roundtrip() {
        let sig = Signature {
            block: 65536,
            size: 65536 + 7,
            blocks: vec![(0xdead_beef, [1; 16]), (42, [2; 16])],
        };
        let req = Req::Fetch {
            dir: "/r/app".into(),
            paths: vec!["app/build/x.apk".into()],
            sigs: vec![("app/build/x.apk".into(), sig)],
        };
        let Req::Fetch { dir, paths, sigs } = roundtrip(&req) else {
            panic!("wrong variant")
        };
        assert_eq!(dir, "/r/app");
        assert_eq!(paths, vec!["app/build/x.apk".to_string()]);
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].0, "app/build/x.apk");
        assert_eq!((sigs[0].1.block, sigs[0].1.size), (65536, 65543));
        assert_eq!(sigs[0].1.blocks, vec![(0xdead_beef, [1; 16]), (42, [2; 16])]);
    }

    #[test]
    fn output_bytes_survive_a_frame_roundtrip_byte_exact() {
        let data = vec![0u8, 0xff, b'\n', 0x80, b'x'];
        let Resp::Output { stderr, data: back } = roundtrip(&Resp::Output {
            stderr: true,
            data: data.clone(),
        }) else {
            panic!("wrong variant")
        };
        assert!(stderr);
        assert_eq!(back, data);
    }

    #[test]
    fn consecutive_frames_are_read_back_in_order() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Req::Flush).unwrap();
        write_frame(&mut buf, &Req::Bye).unwrap();
        let mut r = Cursor::new(buf);
        assert!(matches!(read_frame::<_, Req>(&mut r).unwrap(), Req::Flush));
        assert!(matches!(read_frame::<_, Req>(&mut r).unwrap(), Req::Bye));
        assert!(read_frame::<_, Req>(&mut r).is_err());
    }

    #[test]
    fn read_frame_rejects_a_length_above_the_limit() {
        let mut buf = (MAX_FRAME + 1).to_be_bytes().to_vec();
        buf.extend_from_slice(&[0; 16]);
        let err = read_frame::<_, Req>(&mut Cursor::new(buf)).unwrap_err();
        assert!(format!("{err:#}").contains("exceeds the limit"), "{err:#}");
    }

    #[test]
    fn read_frame_on_an_empty_reader_says_connection_closed() {
        let err = read_frame::<_, Req>(&mut Cursor::new(Vec::new())).unwrap_err();
        assert!(format!("{err:#}").contains("connection closed"), "{err:#}");
    }

    #[test]
    fn read_frame_errors_on_a_truncated_header_or_body() {
        assert!(read_frame::<_, Req>(&mut Cursor::new(vec![0, 0])).is_err());
        let mut buf = Vec::new();
        write_frame(&mut buf, &Req::Hello { version: "1.2.3".into() }).unwrap();
        buf.truncate(buf.len() - 2);
        assert!(read_frame::<_, Req>(&mut Cursor::new(buf)).is_err());
    }

    #[test]
    fn compress_roundtrips_empty_tiny_compressible_and_random_data() {
        for data in [Vec::new(), vec![7u8], b"abcdefgh".repeat(128 * 1024), noise(300_000, 9)] {
            let z = compress(&data).unwrap();
            assert_eq!(decompress(&z).unwrap(), data, "len {}", data.len());
        }
        assert!(compress(&b"abcdefgh".repeat(128 * 1024)).unwrap().len() < 64 * 1024);
    }

    #[test]
    fn decompress_of_garbage_errors() {
        assert!(decompress(b"definitely not a zstd frame").is_err());
        assert!(decompress(&[]).is_err());
    }

    #[test]
    fn streaming_frames_without_a_content_size_still_decompress() {
        let data = b"hello mirako ".repeat(1000);
        let z = zstd::stream::encode_all(&data[..], ZSTD_LEVEL).unwrap();
        assert_eq!(decompress(&z).unwrap(), data);
    }

    #[test]
    fn decompress_accepts_up_to_two_chunks_and_rejects_more() {
        let one = vec![0u8; CHUNK];
        assert_eq!(decompress(&compress(&one).unwrap()).unwrap().len(), CHUNK);
        let two = vec![0u8; 2 * CHUNK];
        assert_eq!(decompress(&compress(&two).unwrap()).unwrap().len(), 2 * CHUNK);
        let bomb = compress(&vec![0u8; 2 * CHUNK + 1]).unwrap();
        let err = decompress(&bomb).unwrap_err();
        assert!(format!("{err:#}").contains("exceeds the chunk limit"), "{err:#}");
    }
}
