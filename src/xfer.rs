//! Receiving side of file transfers, shared by the client (pull) and the agent (push):
//! whole-file chunks and delta chunks land in a temp file next to the destination and are
//! renamed into place when complete, so a broken connection never leaves a half-written file.
//! Nothing is fsync'ed: that costs ~4 ms per file on macOS and the next sync repairs whatever a
//! power cut might lose. A file that comes in one chunk (any under `CHUNK`) is written on a pool
//! of `WRITERS` threads while the next frames are read: its handful of syscalls is the whole
//! cost, and one thread gets through ~4 500 files a second.

use crate::delta;
use crate::proto::{self, Chunk, DeltaChunk};
use anyhow::{bail, Context, Result};
use filetime::FileTime;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, LazyLock};

/// Threads writing one-chunk files. Creating and renaming files contends in the kernel: a cold
/// push of 50 000 small files to APFS took 11.2 s written inline, 6.0 s with 2 writers, 6.3–6.8 s
/// with 3, 6.8–7.4 s with 4 and 9.6 s with 10.
const WRITERS: usize = 2;

static WRITER_POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(WRITERS)
        .thread_name(|i| format!("mirako-write-{i}"))
        .build()
        .expect("writer threads")
});

/// Compressed bytes the pool may hold at once (a job counts at least `JOB_COST`), so a stream of
/// files arriving faster than they are written waits instead of piling up in memory.
const IN_FLIGHT: usize = 32 << 20;
const JOB_COST: usize = 4096;

pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    // on Windows `\` separates as well and `C:` starts the path over; no file name there holds either
    let foreign = cfg!(windows) && rel.contains(['\\', ':']);
    if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|c| c == "..") || foreign {
        bail!("refusing path `{rel}`");
    }
    Ok(root.join(rel))
}

/// `canonicalize` without the `\\?\` prefix Windows puts on the result: under that prefix `/` is
/// no separator, and the paths of the wire are joined to a root as they come. Every root (and
/// the binary named in the Gradle init script) goes through this.
pub fn canonical(p: &Path) -> io::Result<PathBuf> {
    let c = p.canonicalize()?;
    #[cfg(windows)]
    if let Some(plain) = c.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        return Ok(match plain.strip_prefix(r"UNC\") {
            Some(share) => PathBuf::from(format!(r"\\{share}")),
            None => PathBuf::from(plain),
        });
    }
    Ok(c)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// Windows has no permission bits to set.
#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> io::Result<()> {
    Ok(())
}

/// `make` at `dest`, made again after removing the directory in its way when `replace_dir`: a
/// push mirrors, and a directory turned into a file or a link here may stay behind on the host,
/// empty or holding excluded files only, since the manifest names files and not directories.
fn over_dir(dest: &Path, replace_dir: bool, make: impl Fn() -> io::Result<()>) -> io::Result<()> {
    match make() {
        Err(_) if replace_dir && fs::symlink_metadata(dest).is_ok_and(|m| m.is_dir()) => {
            fs::remove_dir_all(dest)?;
            make()
        }
        r => r,
    }
}

#[cfg(unix)]
fn make_symlink(target: &str, link: &Path, replace_dir: bool) -> Result<()> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(link);
    Ok(over_dir(link, replace_dir, || std::os::unix::fs::symlink(target, link))?)
}

/// Windows gets no links, and what is at `link` stays: making one takes Developer Mode there,
/// and its kind (file or directory) would depend on files that are still on their way.
#[cfg(not(unix))]
fn make_symlink(target: &str, link: &Path, _: bool) -> Result<()> {
    bail!(
        "{} is a symlink to `{target}` on the other side: not made on Windows",
        link.display()
    )
}

struct Inbound {
    file: fs::File,
    tmp: PathBuf,
    dest: PathBuf,
    /// delta transfers only
    old: Option<(fs::File, u64)>,
    hasher: blake3::Hasher,
}

/// A finished file: `ok == false` means a delta rebuilt to the wrong hash and was discarded.
pub struct Finished {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: [u8; 32],
    pub ok: bool,
}

/// What a pool write sends back: the bytes it held against `IN_FLIGHT`, and how it went.
type Written = (usize, Result<Finished>);

pub struct Inbox {
    open: HashMap<String, Inbound>,
    /// the agent's: see `mirror`
    replace_dirs: bool,
    tx: mpsc::Sender<Written>,
    rx: mpsc::Receiver<Written>,
    in_flight: usize,
    done: Vec<Finished>,
    error: Option<anyhow::Error>,
}

impl Default for Inbox {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Inbox {
            open: HashMap::new(),
            replace_dirs: false,
            tx,
            rx,
            in_flight: 0,
            done: Vec::new(),
            error: None,
        }
    }
}

fn tmp_path(dest: &Path) -> PathBuf {
    dest.with_file_name(format!(".mirako.{}.tmp", dest.file_name().unwrap_or_default().to_string_lossy()))
}

/// Moves a complete `tmp` over `dest` with its mode and mtime; returns the mtime a later scan
/// reads back.
fn install(tmp: &Path, dest: &Path, mode: u32, mtime_ns: i64, replace_dir: bool) -> Result<i64> {
    set_mode(tmp, mode)?;
    let _ = fs::remove_file(dest);
    over_dir(dest, replace_dir, || fs::rename(tmp, dest))?;
    // NTFS keeps 100 ns: report the mtime a later scan reads back, or its cache entry never hits
    let mtime_ns = if cfg!(windows) {
        mtime_ns - mtime_ns.rem_euclid(100)
    } else {
        mtime_ns
    };
    let ft = FileTime::from_unix_time(mtime_ns.div_euclid(1_000_000_000), mtime_ns.rem_euclid(1_000_000_000) as u32);
    filetime::set_file_mtime(dest, ft)?;
    Ok(mtime_ns)
}

/// A file that came in one chunk, on a thread of the pool.
fn write_whole(dest: &Path, c: Chunk, replace_dir: bool) -> Result<Finished> {
    let written = (|| -> Result<(u64, [u8; 32], i64)> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = proto::decompress(&c.data)?;
        let tmp = tmp_path(dest);
        fs::write(&tmp, &data)?;
        let mtime_ns = install(&tmp, dest, c.mode, c.mtime_ns, replace_dir)?;
        Ok((data.len() as u64, *blake3::hash(&data).as_bytes(), mtime_ns))
    })();
    match written {
        Ok((size, hash, mtime_ns)) => Ok(Finished {
            path: c.path,
            size,
            mtime_ns,
            hash,
            ok: true,
        }),
        Err(e) => Err(e.context(format!("writing {}", c.path))),
    }
}

impl Inbox {
    /// The agent's inbox: a push mirrors, so a directory where a file or link goes is removed
    /// (the client's pull never deletes anything, and reports it instead).
    pub fn mirror() -> Self {
        Inbox {
            replace_dirs: true,
            ..Self::default()
        }
    }

    fn start(&mut self, root: &Path, rel: &str, with_old: bool) -> Result<()> {
        let dest = safe_join(root, rel)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = tmp_path(&dest);
        let file = fs::File::create(&tmp)?;
        let old = if with_old {
            let f = fs::File::open(&dest).with_context(|| format!("old copy of {rel} missing for delta"))?;
            let size = f.metadata()?.len();
            Some((f, size))
        } else {
            None
        };
        self.open.insert(
            rel.to_string(),
            Inbound {
                file,
                tmp,
                dest,
                old,
                hasher: blake3::Hasher::new(),
            },
        );
        Ok(())
    }

    fn finish(&mut self, rel: &str, mode: u32, mtime_ns: i64, expected: Option<[u8; 32]>) -> Result<Finished> {
        let Inbound {
            file, tmp, dest, hasher, ..
        } = self.open.remove(rel).unwrap();
        let size = file.metadata()?.len();
        drop(file);
        let hash = *hasher.finalize().as_bytes();
        if let Some(exp) = expected {
            if exp != hash {
                let _ = fs::remove_file(&tmp);
                return Ok(Finished {
                    path: rel.into(),
                    size,
                    mtime_ns,
                    hash,
                    ok: false,
                });
            }
        }
        let mtime_ns = install(&tmp, &dest, mode, mtime_ns, self.replace_dirs)?;
        Ok(Finished {
            path: rel.into(),
            size,
            mtime_ns,
            hash,
            ok: true,
        })
    }

    /// A file in one chunk goes to the pool and comes back from `settle`; the last chunk of a
    /// longer one finishes it here.
    pub fn put(&mut self, root: &Path, c: Chunk) -> Result<Option<Finished>> {
        if c.offset == 0 && c.last {
            let dest = safe_join(root, &c.path)?;
            self.spawn_write(dest, c);
            return Ok(None);
        }
        if c.offset == 0 {
            self.start(root, &c.path, false)?;
        }
        let data = proto::decompress(&c.data)?;
        let inb = self
            .open
            .get_mut(&c.path)
            .with_context(|| format!("chunk without start for {}", c.path))?;
        inb.hasher.update(&data);
        std::io::Write::write_all(&mut inb.file, &data)?;
        if c.last {
            return Ok(Some(self.finish(&c.path, c.mode, c.mtime_ns, None)?));
        }
        Ok(None)
    }

    pub fn delta(&mut self, root: &Path, c: &DeltaChunk, block: u32) -> Result<Option<Finished>> {
        if !self.open.contains_key(&c.path) {
            self.start(root, &c.path, true)?;
        }
        let inb = self.open.get_mut(&c.path).unwrap();
        let (old, old_size) = inb.old.as_mut().map(|(f, s)| (f, *s)).unwrap();
        delta::apply(Some(old), old_size, block, &c.ops, &mut inb.file, &mut inb.hasher)?;
        if c.last {
            return Ok(Some(self.finish(&c.path, c.mode, c.mtime_ns, Some(c.hash))?));
        }
        Ok(None)
    }

    pub fn symlink(&mut self, root: &Path, rel: &str, target: &str) -> Result<()> {
        make_symlink(target, &safe_join(root, rel)?, self.replace_dirs)
    }

    /// Waits for the pool writes and returns the files they finished; the first write that
    /// failed is the error, and the others go unreported (a later scan hashes them again).
    pub fn settle(&mut self) -> Result<Vec<Finished>> {
        while self.in_flight > 0 {
            self.take_one();
        }
        let done = std::mem::take(&mut self.done);
        match self.error.take() {
            Some(e) => Err(e),
            None => Ok(done),
        }
    }

    fn spawn_write(&mut self, dest: PathBuf, c: Chunk) {
        let cost = c.data.len().max(JOB_COST);
        while self.in_flight > 0 && self.in_flight + cost > IN_FLIGHT {
            self.take_one();
        }
        let tx = self.tx.clone();
        let replace_dir = self.replace_dirs;
        self.in_flight += cost;
        WRITER_POOL.spawn(move || {
            // the receiver is gone only when the transfer was abandoned
            let _ = tx.send((cost, write_whole(&dest, c, replace_dir)));
        });
    }

    fn take_one(&mut self) {
        // `self.tx` keeps the channel open, and every job sends once
        let (cost, r) = self.rx.recv().expect("pool write lost");
        self.in_flight -= cost;
        match r {
            Ok(f) => self.done.push(f),
            Err(e) => {
                self.error.get_or_insert(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index;
    use crate::proto::Signature;

    /// A multiple of 100 ns, which is all NTFS keeps.
    const MTIME: i64 = 1_700_000_000_123_456_700;

    fn chunk(path: &str, offset: u64, last: bool, bytes: &[u8], size: u64) -> Chunk {
        Chunk {
            path: path.into(),
            mode: 0o640,
            mtime_ns: MTIME,
            size,
            offset,
            last,
            data: proto::compress(bytes).unwrap(),
        }
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

    fn tmp_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".mirako.") && n.ends_with(".tmp"))
            .collect()
    }

    fn delta_frames(old_path: &Path, new: &[u8], hash: [u8; 32]) -> Vec<DeltaChunk> {
        let sig: Signature = delta::signature(old_path).unwrap();
        let head = DeltaChunk {
            path: "f.bin".into(),
            mode: 0o640,
            mtime_ns: MTIME,
            size: new.len() as u64,
            hash,
            ops: Vec::new(),
            last: false,
        };
        let mut frames = Vec::new();
        delta::stream(new, &sig, &head, |f| {
            frames.push(f);
            Ok(())
        })
        .unwrap();
        frames
    }

    #[test]
    fn safe_join_accepts_relative_paths_and_rejects_escapes() {
        let root = Path::new("/r");
        assert_eq!(safe_join(root, "a").unwrap(), root.join("a"));
        assert_eq!(safe_join(root, "a/b.txt").unwrap(), root.join("a/b.txt"));
        for bad in ["", "/abs", "..", "a/../b", "../x", "a/.."] {
            assert!(safe_join(root, bad).is_err(), "{bad:?} accepted");
        }
        // `\` and `C:` are ordinary characters of a file name, except on Windows
        for windows_escape in ["a\\..\\..\\b", "\\abs", "C:\\abs", "C:rel", "a/b:stream"] {
            assert_eq!(safe_join(root, windows_escape).is_err(), cfg!(windows), "{windows_escape:?}");
        }
        // current behaviour: `.` and empty components are harmless and accepted as is
        assert_eq!(safe_join(root, "a/./b").unwrap(), root.join("a/./b"));
        assert_eq!(safe_join(root, "a//b").unwrap(), root.join("a//b"));
    }

    #[test]
    fn a_canonical_root_takes_the_slash_paths_of_the_wire() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path()).unwrap();
        assert!(!root.to_string_lossy().starts_with(r"\\?\"), "{}", root.display());
        let mut inbox = Inbox::default();
        inbox.put(&root, chunk("a/b/f", 0, true, b"x", 1)).unwrap();
        inbox.settle().unwrap();
        assert_eq!(fs::read(dir.path().join("a").join("b").join("f")).unwrap(), b"x");
        assert!(canonical(&root.join("missing")).is_err());
    }

    #[test]
    fn put_in_two_chunks_writes_the_file_with_mode_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let data = noise(10_000, 1);
        let (a, b) = data.split_at(4_000);
        let mut inbox = Inbox::default();

        let first = inbox.put(root, chunk("x/y/f.bin", 0, false, a, data.len() as u64)).unwrap();
        assert!(first.is_none());
        let done = inbox
            .put(root, chunk("x/y/f.bin", a.len() as u64, true, b, data.len() as u64))
            .unwrap()
            .expect("finished on the last chunk");

        assert!(done.ok);
        assert_eq!(done.path, "x/y/f.bin");
        assert_eq!(done.size, data.len() as u64);
        assert_eq!(done.mtime_ns, MTIME);
        assert_eq!(done.hash, *blake3::hash(&data).as_bytes());
        let dest = root.join("x/y/f.bin");
        assert_eq!(fs::read(&dest).unwrap(), data);
        let md = fs::metadata(&dest).unwrap();
        assert_eq!(index::mode(&md), if cfg!(unix) { 0o640 } else { 0o755 });
        assert_eq!(index::mtime_ns(&md), MTIME);
        assert!(tmp_files(&root.join("x/y")).is_empty());
    }

    /// A file in one chunk, written by the pool.
    fn put_whole(root: &Path, c: Chunk) -> Finished {
        let mut inbox = Inbox::default();
        assert!(inbox.put(root, c).unwrap().is_none(), "finished before `settle`");
        let mut done = inbox.settle().unwrap();
        assert_eq!(done.len(), 1);
        done.pop().unwrap()
    }

    #[test]
    fn put_in_one_chunk_writes_the_file_with_mode_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let data = noise(10_000, 4);
        let done = put_whole(root, chunk("x/y/f.bin", 0, true, &data, data.len() as u64));
        assert!(done.ok);
        assert_eq!(done.path, "x/y/f.bin");
        assert_eq!(done.size, data.len() as u64);
        assert_eq!(done.mtime_ns, MTIME);
        assert_eq!(done.hash, *blake3::hash(&data).as_bytes());
        let dest = root.join("x/y/f.bin");
        assert_eq!(fs::read(&dest).unwrap(), data);
        let md = fs::metadata(&dest).unwrap();
        assert_eq!(index::mode(&md), if cfg!(unix) { 0o640 } else { 0o755 });
        assert_eq!(index::mtime_ns(&md), MTIME);
        assert!(tmp_files(&root.join("x/y")).is_empty());
    }

    #[test]
    fn put_replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("f"), b"a much longer old content").unwrap();
        assert!(put_whole(root, chunk("f", 0, true, b"new", 3)).ok);
        assert_eq!(fs::read(root.join("f")).unwrap(), b"new");
        assert!(tmp_files(root).is_empty());
    }

    #[test]
    fn put_of_an_empty_file_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let done = put_whole(root, chunk("empty", 0, true, b"", 0));
        assert!(done.ok);
        assert_eq!(done.size, 0);
        assert_eq!(done.hash, *blake3::hash(b"").as_bytes());
        assert_eq!(fs::read(root.join("empty")).unwrap(), b"");
    }

    #[test]
    fn a_failed_pool_write_is_the_error_of_settle_after_the_others_finish() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("blocker"), b"a file, not a directory").unwrap();
        let mut inbox = Inbox::default();
        inbox.put(root, chunk("blocker/f", 0, true, b"x", 1)).unwrap();
        for i in 0..50 {
            inbox.put(root, chunk(&format!("ok/{i}"), 0, true, b"y", 1)).unwrap();
        }
        let err = format!("{:#}", inbox.settle().err().unwrap());
        assert!(err.contains("writing blocker/f"), "{err}");
        assert!((0..50).all(|i| root.join(format!("ok/{i}")).exists()));
        // nothing left in flight: the inbox takes the next stream
        inbox.put(root, chunk("next", 0, true, b"z", 1)).unwrap();
        assert_eq!(inbox.settle().unwrap().len(), 1);
    }

    #[test]
    fn a_chunk_without_a_start_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = Inbox::default().put(dir.path(), chunk("f", 10, true, b"x", 11)).err().unwrap();
        assert!(err.to_string().contains("chunk without start"), "{err}");
        assert!(!dir.path().join("f").exists());
    }

    #[test]
    fn put_refuses_an_escaping_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Inbox::default().put(dir.path(), chunk("../f", 0, true, b"x", 1)).is_err());
    }

    #[test]
    fn delta_rebuilds_the_new_version_from_the_old_copy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let old = noise(5 * delta::BLOCK, 2);
        let mut new = old.clone();
        new.splice(2 * delta::BLOCK + 77..2 * delta::BLOCK + 77, noise(3000, 3));
        fs::write(root.join("f.bin"), &old).unwrap();

        let frames = delta_frames(&root.join("f.bin"), &new, *blake3::hash(&new).as_bytes());
        let mut inbox = Inbox::default();
        let mut finished = None;
        for (i, f) in frames.iter().enumerate() {
            let r = inbox.delta(root, f, delta::BLOCK as u32).unwrap();
            assert_eq!(r.is_some(), i == frames.len() - 1);
            finished = r;
        }
        let done = finished.unwrap();
        assert!(done.ok);
        assert_eq!(done.path, "f.bin");
        assert_eq!(done.size, new.len() as u64);
        assert_eq!(done.hash, *blake3::hash(&new).as_bytes());
        assert_eq!(fs::read(root.join("f.bin")).unwrap(), new);
        let md = fs::metadata(root.join("f.bin")).unwrap();
        assert_eq!(index::mode(&md), if cfg!(unix) { 0o640 } else { 0o755 });
        assert_eq!(index::mtime_ns(&md), MTIME);
        assert!(tmp_files(root).is_empty());
    }

    #[test]
    fn a_delta_with_the_wrong_hash_is_discarded_and_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let old = noise(5 * delta::BLOCK, 4);
        let mut new = old.clone();
        new.splice(delta::BLOCK..delta::BLOCK, noise(500, 5));
        fs::write(root.join("f.bin"), &old).unwrap();

        let frames = delta_frames(&root.join("f.bin"), &new, [0; 32]);
        let mut inbox = Inbox::default();
        let mut finished = None;
        for f in &frames {
            finished = inbox.delta(root, f, delta::BLOCK as u32).unwrap();
        }
        let done = finished.unwrap();
        assert!(!done.ok);
        assert_eq!(done.path, "f.bin");
        assert_eq!(done.hash, *blake3::hash(&new).as_bytes());
        assert_eq!(fs::read(root.join("f.bin")).unwrap(), old);
        assert!(tmp_files(root).is_empty());
    }

    #[test]
    fn delta_without_an_old_copy_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let c = DeltaChunk {
            path: "missing.bin".into(),
            mode: 0o644,
            mtime_ns: MTIME,
            size: 0,
            hash: [0; 32],
            ops: Vec::new(),
            last: true,
        };
        let err = Inbox::default().delta(dir.path(), &c, delta::BLOCK as u32).err().unwrap();
        let msg = format!("{err:#}");
        assert!(msg.contains("old copy of missing.bin missing"), "{msg}");
    }

    #[test]
    #[cfg(unix)] // creating a link on Windows takes Developer Mode
    fn symlink_creates_parents_and_replaces_files_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut inbox = Inbox::default();

        inbox.symlink(root, "a/b/link", "../target").unwrap();
        assert_eq!(fs::read_link(root.join("a/b/link")).unwrap(), Path::new("../target"));

        inbox.symlink(root, "a/b/link", "other").unwrap();
        assert_eq!(fs::read_link(root.join("a/b/link")).unwrap(), Path::new("other"));

        fs::write(root.join("file"), b"x").unwrap();
        inbox.symlink(root, "file", "t").unwrap();
        assert_eq!(fs::read_link(root.join("file")).unwrap(), Path::new("t"));

        assert!(inbox.symlink(root, "../escape", "t").is_err());
        assert!(inbox.symlink(root, "a/../../escape", "t").is_err());
    }
}
