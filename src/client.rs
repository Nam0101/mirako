//! The local side: ssh to the remote agent, push the diff, run the command, pull the diff.

use crate::config::Config;
use crate::delta;
use crate::index::{Index, Sigs};
use crate::patterns::Matcher;
use crate::progress::Progress;
use crate::proto::{self, read_frame, write_frame, Chunk, DeltaChunk, Entry, GcReport, GcReq, Kind, Req, Resp, CHUNK};
use crate::rewrite::LineRewriter;
use crate::server::read_full;
use crate::shim;
use crate::xfer::Inbox;
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, TryLockError};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread;
use std::time::Instant;

/// Non-interactive, and give up fast when the host is asleep or off the network so the
/// Gradle shim falls back to a local build instead of hanging on the TCP timeout.
const SSH_OPTS: &[&str] = &["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"];

pub const REPO: &str = "https://github.com/Nam0101/mirako";

/// `ssh <opts> host`, ready for the remote command as the next argument.
pub fn ssh(cfg: &Config) -> Command {
    let mut cmd = Command::new(&cfg.ssh[0]);
    cmd.args(&cfg.ssh[1..]).args(SSH_OPTS).arg(&cfg.host);
    cmd
}

pub struct Session {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    writer: BufWriter<std::process::ChildStdin>,
    pub remote_home: String,
    pub remote_os: String,
    /// `--quiet`: no progress lines while a transfer runs
    pub quiet: bool,
}

/// One try at `Session::connect`: up, or why the agent needs (re)installing first.
enum Attempt {
    Up(Session),
    /// the remote shell exited 127: no `remote_bin` there
    Missing,
    /// the agent left without a word: it could not decode the `Hello` (0.4 or older, bincode frames)
    Mute,
    /// the agent answered with this other version
    Version(String),
}

#[derive(Default)]
pub struct Stats {
    pub files: usize,
    pub bytes: u64,
    pub wire: u64,
    pub deleted: usize,
    pub deltas: usize,
}

impl Session {
    /// Starts `mirako serve` over ssh and shakes hands. `first` goes out in the same write as
    /// the `Hello`, so the agent is already working on it while the reply crosses the link.
    /// `ssh host 'mirako serve'` plus the handshake. When the agent is missing there, runs
    /// another version or does not answer, this binary is installed as `remote_bin` and the
    /// connection retried once.
    pub fn connect(cfg: &Config, first: Option<&Req>) -> Result<Self> {
        let why = match Self::attempt(cfg, first)? {
            Attempt::Up(s) => return Ok(s),
            Attempt::Missing => format!("no `{}` on {}", cfg.remote_bin, cfg.host),
            Attempt::Mute => format!("`{}` on {} does not answer the handshake (0.4 or older)", cfg.remote_bin, cfg.host),
            Attempt::Version(v) => format!("remote mirako is {v}, local is {}", proto::VERSION),
        };
        eprintln!("mirako: {why}: installing mirako {} there", proto::VERSION);
        remote_install(cfg).with_context(|| format!("{why}; installing it failed, try `mirako remote-install`"))?;
        match Self::attempt(cfg, first)? {
            Attempt::Up(s) => Ok(s),
            Attempt::Missing => bail!("still no `{}` on {} right after installing it", cfg.remote_bin, cfg.host),
            Attempt::Mute => bail!(
                "`{}` on {} still does not answer right after installing it",
                cfg.remote_bin,
                cfg.host
            ),
            Attempt::Version(v) => bail!("remote mirako is still {v} right after installing {}", proto::VERSION),
        }
    }

    fn attempt(cfg: &Config, first: Option<&Req>) -> Result<Attempt> {
        let mut child = ssh(cfg)
            .arg(format!("{} serve", cfg.remote_bin))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("starting ssh")?;
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut writer = BufWriter::new(child.stdin.take().unwrap());
        let handshake = (|| -> Result<Resp> {
            write_frame(
                &mut writer,
                &Req::Hello {
                    version: proto::VERSION.into(),
                },
            )?;
            if let Some(req) = first {
                write_frame(&mut writer, req)?;
            }
            writer.flush()?;
            read_frame(&mut reader)
        })();
        let hello = match handshake {
            Ok(hello) => hello,
            Err(e) => {
                match child.wait().ok().and_then(|s| s.code()) {
                    // the remote shell could not run `remote_bin`: 127 not found, 126 not executable
                    Some(126 | 127) => return Ok(Attempt::Missing),
                    // `serve` ends cleanly on a frame it cannot decode
                    Some(0) => return Ok(Attempt::Mute),
                    _ => {}
                }
                bail!(
                    "no answer from `{} serve` on {} ({e}). Is the host reachable and mirako installed there? Try `mirako remote-install`.",
                    cfg.remote_bin,
                    cfg.host
                );
            }
        };
        match hello {
            Resp::Hello { version, home, os } => {
                if version != proto::VERSION {
                    // a 0.4+ agent exits on its own here; an older one is still waiting for a frame
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(Attempt::Version(version));
                }
                Ok(Attempt::Up(Self {
                    child,
                    reader,
                    writer,
                    remote_home: home,
                    remote_os: os,
                    quiet: false,
                }))
            }
            Resp::Error { msg } => bail!("remote: {msg}"),
            other => bail!("unexpected handshake reply {other:?}"),
        }
    }

    fn send(&mut self, req: &Req) -> Result<()> {
        write_frame(&mut self.writer, req)
    }

    fn call(&mut self, req: &Req) -> Result<Resp> {
        self.send(req)?;
        self.writer.flush()?;
        self.recv()
    }

    fn recv(&mut self) -> Result<Resp> {
        recv(&mut self.reader)
    }

    /// `Flush`, then the requests of the phase after the push (the `Exec`), in one write: the
    /// agent answers the `Ack` and goes straight on to them, so the push and the command cost one
    /// round trip between them instead of one each. Returns the failed delta pushes.
    fn flush_and(&mut self, after: &[Req]) -> Result<Vec<String>> {
        self.send(&Req::Flush)?;
        for r in after {
            self.send(r)?;
        }
        self.writer.flush()?;
        match self.recv()? {
            Resp::Ack { failed } => Ok(failed),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Reads one reply and drops it, an `Error` included.
    fn discard(&mut self) -> Result<()> {
        read_frame::<_, Resp>(&mut self.reader).map(drop)
    }

    /// Upload: make the remote copy of the upload scope identical to the local one. The scope's
    /// `Manifest` request went out with the handshake (`connect`), so the agent scans its copy
    /// while the local scan runs here. `after` is queued behind the final `Flush` (see `flush_and`).
    pub fn push(&mut self, root: &Path, index: &mut Index, remote_dir: &str, exclude: &[String], after: &[Req]) -> Result<Stats> {
        let mut local = index.scan(root, &Matcher::new(exclude)?)?;
        // a `local.properties` goes up as `portable_properties` leaves it, so its entry describes that content
        let mut portable: HashMap<String, Vec<u8>> = HashMap::new();
        for e in local.iter_mut().filter(|e| e.kind == Kind::File && is_local_properties(&e.path)) {
            let bytes = portable_properties(&fs::read(root.join(&e.path))?);
            e.size = bytes.len() as u64;
            e.hash = *blake3::hash(&bytes).as_bytes();
            portable.insert(e.path.clone(), bytes);
        }
        let remote = match self.recv()? {
            Resp::Manifest { entries, .. } => entries,
            other => bail!("unexpected reply {other:?}"),
        };
        let remote_map: HashMap<&str, &Entry> = remote.iter().map(|e| (e.path.as_str(), e)).collect();
        let local_set: HashSet<&str> = local.iter().map(|e| e.path.as_str()).collect();

        let to_delete: Vec<String> = remote
            .iter()
            .filter(|e| !local_set.contains(e.path.as_str()))
            .map(|e| e.path.clone())
            .collect();
        let to_send: Vec<&Entry> = local
            .iter()
            .filter(|e| match remote_map.get(e.path.as_str()) {
                Some(r) => r.hash != e.hash || r.kind != e.kind || r.mode != e.mode,
                None => true,
            })
            .collect();

        // big changed files the remote already has an old copy of go as deltas
        let delta_candidates: Vec<String> = to_send
            .iter()
            .filter(|e| {
                e.kind == Kind::File
                    && delta::delta_worthwhile(e.size)
                    && remote_map.get(e.path.as_str()).map(|r| r.kind == Kind::File).unwrap_or(false)
            })
            .map(|e| e.path.clone())
            .collect();
        let sigs: HashMap<String, proto::Signature> = if delta_candidates.is_empty() {
            HashMap::new()
        } else {
            match self.call(&Req::Sigs {
                dir: remote_dir.into(),
                paths: delta_candidates,
            })? {
                Resp::Sigs(v) => v.into_iter().collect(),
                other => bail!("unexpected reply {other:?}"),
            }
        };

        let mut stats = Stats {
            deleted: to_delete.len(),
            ..Default::default()
        };
        if !to_delete.is_empty() {
            self.send(&Req::Delete { paths: to_delete })?;
        }
        let mut progress = Progress::new("push", Some(to_send.iter().map(|e| e.size).sum()), !self.quiet);
        for e in &to_send {
            match &e.kind {
                Kind::Symlink { target } => self.send(&Req::Symlink {
                    path: e.path.clone(),
                    target: target.clone(),
                })?,
                Kind::File => {
                    if let Some(bytes) = portable.get(&e.path) {
                        stats.wire += self.send_from(e, bytes.as_slice(), &mut progress)?;
                    } else if let Some(sig) = sigs.get(&e.path) {
                        stats.wire += self.send_delta(root, e, sig, &mut progress)?;
                        stats.deltas += 1;
                    } else {
                        stats.wire += self.send_file(root, e, &mut progress)?;
                    }
                    stats.bytes += e.size;
                }
            }
            stats.files += 1;
        }
        let failed = self.flush_and(after)?;
        if !failed.is_empty() {
            // a delta rebuilt to the wrong hash on the remote, which then refused to run `after`:
            // drop those replies, resend the files whole and queue `after` again
            for _ in after {
                self.discard()?;
            }
            let by_path: HashMap<&str, &Entry> = to_send.iter().map(|e| (e.path.as_str(), *e)).collect();
            for p in &failed {
                if let Some(e) = by_path.get(p.as_str()) {
                    stats.wire += self.send_file(root, e, &mut progress)?;
                }
            }
            let again = self.flush_and(after)?;
            if !again.is_empty() {
                bail!("remote could not store {}", again.join(", "));
            }
        }
        Ok(stats)
    }

    fn send_file(&mut self, root: &Path, e: &Entry, progress: &mut Progress) -> Result<u64> {
        let full = root.join(&e.path);
        let f = fs::File::open(&full).with_context(|| format!("reading {}", full.display()))?;
        self.send_from(e, f, progress)
    }

    /// Streams `src`, the `e.size` bytes of `e`, as `Put` chunks. Returns the bytes on the wire.
    fn send_from(&mut self, e: &Entry, mut src: impl Read, progress: &mut Progress) -> Result<u64> {
        let mut offset = 0u64;
        let mut wire = 0u64;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = read_full(&mut src, &mut buf)?;
            let last = offset + n as u64 >= e.size || n == 0;
            let data = proto::compress(&buf[..n])?;
            wire += data.len() as u64;
            self.send(&Req::Put(Chunk {
                path: e.path.clone(),
                mode: e.mode,
                mtime_ns: e.mtime_ns,
                size: e.size,
                offset,
                last,
                data,
            }))?;
            offset += n as u64;
            progress.add(n as u64);
            if last {
                break;
            }
        }
        Ok(wire)
    }

    /// `e.hash` comes from the scan that found the file changed, so the file is read once here.
    fn send_delta(&mut self, root: &Path, e: &Entry, sig: &proto::Signature, progress: &mut Progress) -> Result<u64> {
        let data = fs::read(root.join(&e.path))?;
        let head = DeltaChunk {
            path: e.path.clone(),
            mode: e.mode,
            mtime_ns: e.mtime_ns,
            size: e.size,
            hash: e.hash,
            ops: Vec::new(),
            last: false,
        };
        let writer = &mut self.writer;
        let mut covered = 0u64;
        let wire = delta::stream(&data, sig, &head, |frame| {
            // an estimate while the file is under way (a literal counts as its compressed bytes), exact at its end
            let ops = frame.ops.iter().map(|o| match o {
                proto::Op::Copy { count, .. } => *count as u64 * delta::BLOCK as u64,
                proto::Op::Data(d) => d.len() as u64,
            });
            let n = ops.sum::<u64>().min(e.size - covered);
            covered += n;
            progress.add(n);
            write_frame(writer, &Req::Delta(frame))
        })?;
        progress.add(e.size - covered);
        Ok(wire)
    }

    /// Download: the `Pull` request went up while the command ran (see `run`), so the agent is
    /// already streaming every file of the download scope that differs from the local copy; this
    /// receives them. Never deletes anything locally. `gc_queued`: a `Gc` was sent right behind
    /// the `Pull`, so its report follows the stream and is returned here.
    pub fn pull(&mut self, root: &Path, index: &mut Index, remote_dir: &str, gc_queued: bool) -> Result<(Stats, Option<Result<GcReport>>)> {
        let mut stats = Stats::default();
        let failed = self.receive(root, index, &mut stats)?;
        // read before the retry below, which the agent only sees after the `Gc`
        let gc = gc_queued.then(|| match read_frame(&mut self.reader)? {
            Resp::Gc(r) => Ok(r),
            Resp::Error { msg } => bail!("remote: {msg}"),
            other => bail!("unexpected reply {other:?}"),
        });
        if !failed.is_empty() {
            // deltas that rebuilt to the wrong hash: fetch those whole
            self.send(&Req::Fetch {
                dir: remote_dir.into(),
                paths: failed,
            })?;
            self.writer.flush()?;
            let again = self.receive(root, index, &mut stats)?;
            if !again.is_empty() {
                bail!("could not download {}", again.join(", "));
            }
        }
        Ok((stats, gc))
    }

    /// Reads one stream of `Put`/`Delta`/`Symlink` frames up to its `End`, writing the files
    /// into `root`. Returns the paths whose delta rebuilt to the wrong hash.
    fn receive(&mut self, root: &Path, index: &mut Index, stats: &mut Stats) -> Result<Vec<String>> {
        let mut inbox = Inbox::default();
        let mut failed = Vec::new();
        // Windows: the links of the host, which this side does not make (see `xfer::make_symlink`)
        let mut no_link = Vec::new();
        let mut progress = Progress::new("pull", None, !self.quiet);
        loop {
            match self.recv()? {
                Resp::Put(chunk) => {
                    stats.wire += chunk.data.len() as u64;
                    progress.add(chunk.data.len() as u64);
                    if let Some(f) = inbox.put(root, &chunk)? {
                        stats.files += 1;
                        stats.bytes += f.size;
                        index.remember(&f.path, f.size, f.mtime_ns, f.hash);
                    }
                }
                Resp::Delta(chunk) => {
                    let wire = chunk
                        .ops
                        .iter()
                        .map(|o| if let proto::Op::Data(d) = o { d.len() as u64 } else { 8 })
                        .sum::<u64>();
                    stats.wire += wire;
                    progress.add(wire);
                    if let Some(f) = inbox.delta(root, &chunk, delta::BLOCK as u32)? {
                        if f.ok {
                            stats.files += 1;
                            stats.deltas += 1;
                            stats.bytes += f.size;
                            index.remember(&f.path, f.size, f.mtime_ns, f.hash);
                        } else {
                            failed.push(f.path);
                        }
                    }
                }
                Resp::Symlink { path, .. } if cfg!(windows) => no_link.push(path),
                Resp::Symlink { path, target } => {
                    inbox.symlink(root, &path, &target)?;
                    stats.files += 1;
                }
                Resp::End => break,
                other => bail!("unexpected reply {other:?}"),
            }
        }
        drop(progress);
        if let Some(first) = no_link.first() {
            eprintln!(
                "mirako: {} symlink(s) of the host not made here, Windows gets none (the first: {first}); `exclude_remote_extra` leaves them out",
                no_link.len()
            );
        }
        Ok(failed)
    }

    pub fn gc(&mut self, req: GcReq) -> Result<GcReport> {
        match self.call(&Req::Gc(req))? {
            Resp::Gc(r) => Ok(r),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    pub fn close(mut self) {
        let _ = self.send(&Req::Bye);
        let _ = self.writer.flush();
        let _ = self.child.wait();
    }
}

fn recv(reader: &mut BufReader<ChildStdout>) -> Result<Resp> {
    match read_frame(reader)? {
        Resp::Error { msg } => bail!("remote: {msg}"),
        r => Ok(r),
    }
}

/// Streams the output of the `Exec` queued earlier, remote paths rewritten to local ones.
/// Returns the exit code. Works on the reader alone: the writer is busy on the scan thread
/// meanwhile (see `run`).
fn exec_output(reader: &mut BufReader<ChildStdout>, remote_home: &str, remote_dir: &str, local_root: &Path) -> Result<i32> {
    // the remote project path (`~` expanded with the agent's home) for output rewriting
    let remote_abs = match remote_dir.strip_prefix("~/") {
        Some(r) => format!("{remote_home}/{r}"),
        None => remote_dir.to_string(),
    };
    let local = local_root.to_string_lossy().into_owned();
    let mut out_rw = LineRewriter::new(&remote_abs, &local);
    let mut err_rw = LineRewriter::new(&remote_abs, &local);
    let stdout = io::stdout();
    let stderr = io::stderr();
    loop {
        match recv(reader)? {
            Resp::Output { stderr: is_err, data } => {
                if is_err {
                    let mut h = stderr.lock();
                    h.write_all(&err_rw.feed(&data))?;
                    h.flush()?;
                } else {
                    let mut h = stdout.lock();
                    h.write_all(&out_rw.feed(&data))?;
                    h.flush()?;
                }
            }
            Resp::Exit { code } => {
                stdout.lock().write_all(&out_rw.flush())?;
                stderr.lock().write_all(&err_rw.flush())?;
                return Ok(code);
            }
            other => bail!("unexpected reply {other:?}"),
        }
    }
}

/// The variables among `vars` that the `env` config key names (a name, or a prefix ending in
/// `*`), sorted: what the remote command gets on top of the agent's own environment.
fn forwarded_env(names: &[String], vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<(String, String)> {
    let named = |key: &str| {
        names.iter().any(|n| match n.strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => n == key,
        })
    };
    let mut env: Vec<(String, String)> = vars
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| named(k))
        .collect();
    env.sort();
    env
}

/// Keys of `local.properties` that name a path of the machine the file was written on.
const MACHINE_KEYS: [&[u8]; 3] = [b"sdk.dir", b"ndk.dir", b"cmake.dir"];

fn is_local_properties(rel: &str) -> bool {
    rel.rsplit('/').next() == Some("local.properties")
}

/// `local.properties` as the host gets it: without the lines of `MACHINE_KEYS` (the SDK is found
/// there through `ANDROID_HOME`), every other key, the ones a build reads its secrets from, as it is.
fn portable_properties(text: &[u8]) -> Vec<u8> {
    let kept: Vec<&[u8]> = text
        .split_inclusive(|&b| b == b'\n')
        .filter(|line| {
            let key = line
                .trim_ascii_start()
                .split(|b| b"=: \t\r\n".contains(b))
                .next()
                .unwrap_or_default();
            !MACHINE_KEYS.contains(&key)
        })
        .collect();
    kept.concat()
}

/// The `Pull` request: the local manifest of the download scope plus the block signatures of
/// its big files, so the agent can answer with deltas without another round trip. Built while
/// the command runs; the signatures come from the `Sigs` cache except for files that changed since.
fn pull_request(root: &Path, remote_dir: &str, exclude: &[String], local: Vec<Entry>) -> Result<Req> {
    let mut cache = Sigs::open(root);
    let big: Vec<&Entry> = local
        .iter()
        .filter(|e| e.kind == Kind::File && delta::delta_worthwhile(e.size))
        .collect();
    cache.retain(&big.iter().map(|e| e.path.as_str()).collect());
    let fresh = big
        .par_iter()
        .filter(|e| cache.get(&e.path, e.size, e.mtime_ns).is_none())
        .map(|e| Ok((*e, delta::signature(&root.join(&e.path))?)))
        .collect::<Result<Vec<_>>>()?;
    for (e, sig) in fresh {
        cache.insert(&e.path, e.size, e.mtime_ns, sig);
    }
    let sigs: Vec<(String, proto::Signature)> = big
        .iter()
        .map(|e| {
            (
                e.path.clone(),
                cache.get(&e.path, e.size, e.mtime_ns).expect("just inserted").clone(),
            )
        })
        .collect();
    cache.save();
    Ok(Req::Pull {
        dir: remote_dir.into(),
        exclude: exclude.to_vec(),
        have: local,
        sigs,
    })
}

pub struct RunOptions {
    pub push: bool,
    pub pull: bool,
    pub quiet: bool,
}

pub fn human(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
    } else if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1u64 << 20) as f64)
    } else if bytes >= 1 << 10 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn secs(t: Instant) -> String {
    format!("{:.1}s", t.elapsed().as_secs_f64())
}

/// One run per project at a time, a second one waits here for the first: both would write the
/// same files on either side, and a scan takes every `.mirako.*.tmp` it meets for a leftover and
/// deletes it. The lock is the returned file (next to the project's index cache), held until it
/// is dropped; a run that is killed lets go of it as well. A cache dir that cannot be written
/// gives no lock, as it gives no index cache.
fn lock_project(root: &Path) -> Result<Option<fs::File>> {
    let path = Index::cache_path(root).with_extension("lock");
    let _ = fs::create_dir_all(path.parent().unwrap());
    // opened as it is, not truncated: another run may hold it locked
    let Ok(file) = fs::OpenOptions::new().write(true).create(true).truncate(false).open(&path) else {
        return Ok(None);
    };
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            eprintln!("mirako: another run of this project is in progress, waiting for it to finish");
            file.lock()?;
        }
        Err(TryLockError::Error(e)) => return Err(e).with_context(|| format!("locking {}", path.display())),
    }
    Ok(Some(file))
}

/// The whole round trip. Returns the exit code to end the process with.
pub fn run(root: &Path, cfg: &Config, cmd: &[String], opts: &RunOptions) -> Result<i32> {
    let start = Instant::now();
    refresh_shim();
    let lock = lock_project(root)?;
    let remote_dir = cfg.remote_dir(root);
    let push_excludes = cfg.upload_excludes();
    let pull_excludes = cfg.download_excludes();
    // the push's `Manifest` request rides with the handshake: the agent scans its copy while the
    // `Hello` reply is still on its way here
    let first = opts.push.then(|| Req::Manifest {
        dir: remote_dir.clone(),
        exclude: push_excludes.clone(),
    });
    let mut session = match Session::connect(cfg, first.as_ref()) {
        Ok(s) => s,
        Err(e) if cfg.fallback && !cmd.is_empty() => {
            eprintln!("mirako: {e:#}");
            eprintln!("mirako: running locally instead");
            // nothing is synced from here on, and the command may be a `mirako run` of this project
            drop(lock);
            return run_local(root, cmd);
        }
        Err(e) => return Err(e),
    };
    session.quiet = opts.quiet;
    let say = |s: String| {
        if !opts.quiet {
            println!("{s}")
        }
    };
    say(format!(
        "mirako {}: {} on {}",
        proto::VERSION,
        root.file_name().unwrap_or_default().to_string_lossy(),
        cfg.host
    ));
    let mut index = Index::open(root);

    // the `Exec` goes out with the push's `Flush`, so the agent starts the command without
    // waiting for another round trip
    let mut after = Vec::new();
    if !cmd.is_empty() {
        after.push(Req::Exec {
            dir: remote_dir.clone(),
            cmd: cmd.to_vec(),
            env: forwarded_env(&cfg.env, std::env::vars_os()),
        });
    }

    if opts.push {
        let t = Instant::now();
        let s = session.push(root, &mut index, &remote_dir, &push_excludes, &after)?;
        index.save();
        say(format!(
            "push   {} files ({} as delta), {} → {} on the wire, {} deleted, {}",
            s.files,
            s.deltas,
            human(s.bytes),
            human(s.wire),
            s.deleted,
            secs(t)
        ));
    } else {
        for r in &after {
            session.send(r)?;
        }
        session.writer.flush()?;
    }

    // housekeeping on the host: stale mirrors, intermediates the client never downloads (only
    // once they are not needed any more, i.e. after a pull), Gradle's retention. Never fails a
    // build. When it leaves the current copy alone it is queued right behind the `Pull`, so its
    // report comes back in the same stream; deleting `gc_after_pull` inside the copy has to wait
    // until the pull, retries included, is complete. With `gc_days = 0` it still goes out behind a
    // `Pull`, where it costs no round trip: that is what takes Gradle's retention script off the host.
    let gc_req = (cfg.gc_days > 0 || opts.pull).then(|| GcReq {
        folder: cfg.remote_folder.clone(),
        keep_days: (cfg.gc_days > 0).then_some(cfg.gc_days),
        current: Some(remote_dir.clone()),
        build: if opts.pull { cfg.gc_after_pull.clone() } else { Vec::new() },
        gradle_days: cfg.gc_days,
        dry_run: false,
        sizes: false,
    });
    let gc_queued = opts.pull && gc_req.as_ref().is_some_and(|g| g.build.is_empty());
    let (queued_gc, later_gc) = if gc_queued { (gc_req, None) } else { (None, gc_req) };

    // while the command runs remotely, the local side of the pull is scanned on its own thread
    // and sent up as the `Pull` request, so the agent streams the outputs the moment the command
    // exits; the main thread meanwhile prints the command's output
    let mut code = 0;
    let Session {
        reader,
        writer,
        remote_home,
        ..
    } = &mut session;
    thread::scope(|scope| -> Result<()> {
        let scan = scope.spawn(|| -> Result<()> {
            if !opts.pull {
                return Ok(());
            }
            let local = index.scan(root, &Matcher::new(&pull_excludes)?)?;
            write_frame(writer, &pull_request(root, &remote_dir, &pull_excludes, local)?)?;
            if let Some(req) = queued_gc {
                write_frame(writer, &Req::Gc(req))?;
            }
            Ok(writer.flush()?)
        });
        if !cmd.is_empty() {
            let t = Instant::now();
            code = exec_output(reader, remote_home, &remote_dir, root)?;
            say(format!("exec   exit {code}, {}", secs(t)));
        }
        scan.join().expect("scan thread panicked")
    })?;

    let report = |r: Result<GcReport>| match r {
        Ok(r) => {
            let mut parts: Vec<String> = r
                .mirrors
                .iter()
                .filter(|m| m.removed)
                .map(|m| format!("{} removed ({}, unused {} d)", m.name, human(m.bytes), m.idle_days.unwrap_or(0)))
                .collect();
            if r.build_bytes > 0 {
                parts.push(format!("{} of intermediates deleted", human(r.build_bytes)));
            }
            if !parts.is_empty() {
                parts.push(format!("{} free", human(r.free)));
                say(format!("gc     {}", parts.join(", ")));
            }
        }
        Err(e) => eprintln!("mirako: gc: {e:#}"),
    };
    if opts.pull {
        let t = Instant::now();
        let (s, gc) = session.pull(root, &mut index, &remote_dir, gc_queued)?;
        index.save();
        say(format!(
            "pull   {} files ({} as delta), {} → {} on the wire, {}",
            s.files,
            s.deltas,
            human(s.bytes),
            human(s.wire),
            secs(t)
        ));
        if let Some(r) = gc {
            report(r);
        }
    }
    if let Some(req) = later_gc {
        report(session.gc(req));
    }
    session.close();
    say(format!("total  {}", secs(start)));
    Ok(code)
}

/// `mirako gc`: list the project copies on the host and remove the stale ones.
pub fn gc(cfg: &Config, days: Option<u32>, dry_run: bool) -> Result<()> {
    let keep = days.or((cfg.gc_days > 0).then_some(cfg.gc_days));
    let mut session = Session::connect(cfg, None)?;
    let r = session.gc(GcReq {
        folder: cfg.remote_folder.clone(),
        keep_days: keep,
        current: None,
        build: Vec::new(),
        gradle_days: cfg.gc_days,
        dry_run,
        sizes: true,
    })?;
    session.close();
    let rule = match keep {
        Some(d) => format!("removing copies unused for more than {d} days"),
        None => "gc_days = 0, listing only".into(),
    };
    println!("mirako {}: {} on {}, {rule}", proto::VERSION, cfg.remote_folder, cfg.host);
    let width = r.mirrors.iter().map(|m| m.name.len()).max().unwrap_or(0);
    for m in &r.mirrors {
        let (size, state) = match (m.idle_days, m.removed) {
            (None, _) => ("-".to_string(), "not synced by mirako, left alone".to_string()),
            (Some(d), true) if dry_run => (human(m.bytes), format!("unused {d} d, would be removed")),
            (Some(d), true) => (human(m.bytes), format!("unused {d} d, removed")),
            (Some(0), false) => (human(m.bytes), "used today".to_string()),
            (Some(d), false) => (human(m.bytes), format!("unused {d} d")),
        };
        println!("  {:<width$}  {size:>9}  {state}", m.name);
    }
    if r.mirrors.is_empty() {
        println!("  nothing under {}", cfg.remote_folder);
    }
    if cfg.gc_days > 0 && !dry_run {
        println!(
            "gradle  entries unused for {} days are removed by Gradle itself (~/.gradle/init.d/mirako-gc.gradle)",
            cfg.gc_days
        );
    }
    println!("free    {}", human(r.free));
    Ok(())
}

/// The program of a command as this machine runs it. The command is spelled for the host, so on
/// Windows a `./gradlew` is the `gradlew.bat` beside it.
fn local_program(root: &Path, program: &str) -> OsString {
    let bat: PathBuf = root.join(program).with_extension("bat").components().collect();
    if cfg!(windows) && bat.is_file() {
        bat.into()
    } else {
        program.into()
    }
}

pub fn run_local(root: &Path, cmd: &[String]) -> Result<i32> {
    // tell the Gradle shim not to try the remote again
    let status = Command::new(local_program(root, &cmd[0]))
        .args(&cmd[1..])
        .env("MIRAKO_LOCAL", "1")
        .current_dir(root)
        .status()
        .with_context(|| format!("running {}", cmd[0]))?;
    Ok(status.code().unwrap_or(-1))
}

/// The installed Gradle init script follows this binary: rewritten when it is this binary's and out of date.
fn refresh_shim() {
    match shim::refresh() {
        Ok(Some(p)) => eprintln!("mirako: updated {}", p.display()),
        Ok(None) => {}
        Err(e) => eprintln!("mirako: gradle shim: {e:#}"),
    }
}

/// Quick reachability + version handshake; used by the Gradle shim before hijacking a build.
pub fn check(cfg: &Config) -> Result<()> {
    refresh_shim();
    let s = Session::connect(cfg, None)?;
    let os = s.remote_os.clone();
    s.close();
    println!("mirako {}: {} ok ({os})", proto::VERSION, cfg.host);
    Ok(())
}

/// Put this version at `remote_bin` on the host: a copy of this binary when OS/arch match,
/// otherwise a build there with the host's own cargo.
pub fn remote_install(cfg: &Config) -> Result<()> {
    let uname = |host: bool| -> Result<String> {
        let out = if host {
            ssh(cfg).arg("uname -sm").stdin(Stdio::null()).output()?
        } else {
            Command::new("uname").arg("-sm").output()?
        };
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // Windows has no `uname`, and no host answers like it would: the agent is built there
    let local = if cfg!(windows) { String::new() } else { uname(false)? };
    let remote = uname(true)?;
    if remote.is_empty() {
        bail!("cannot ssh to {}", cfg.host);
    }
    if local != remote {
        return build_on_host(cfg, &remote);
    }
    let me = std::env::current_exe()?;
    let bytes = fs::read(&me)?;
    let dest = cfg.remote_bin.clone();
    let script =
        format!("mkdir -p \"$(dirname {dest})\" && cat > {dest}.tmp && chmod +x {dest}.tmp && mv {dest}.tmp {dest} && {dest} --version");
    let mut child = ssh(cfg).arg(script).stdin(Stdio::piped()).spawn()?;
    child.stdin.take().unwrap().write_all(&bytes)?;
    let status = child.wait()?;
    if !status.success() {
        bail!("install on {} failed", cfg.host);
    }
    println!("installed {} ({}) on {}", dest, human(bytes.len() as u64), cfg.host);
    Ok(())
}

/// The host runs another OS or architecture: `cargo install` the release tag of this version
/// there (needs Rust on the host) and move the binary to `remote_bin`.
fn build_on_host(cfg: &Config, remote_os: &str) -> Result<()> {
    eprintln!(
        "mirako: {} is `{remote_os}`, this binary is not: building mirako {} there with cargo (a few minutes)",
        cfg.host,
        proto::VERSION
    );
    let status = ssh(cfg)
        .arg(build_script(proto::VERSION, &cfg.remote_bin))
        .stdin(Stdio::null())
        .status()?;
    let manual = format!("`cargo install --git {REPO} --tag v{}`", proto::VERSION);
    match status.code() {
        Some(0) => {
            println!("built {} on {}", cfg.remote_bin, cfg.host);
            Ok(())
        }
        Some(3) => bail!(
            "no cargo on {}: install Rust there (https://rustup.rs) and run `mirako setup` again, or build mirako there yourself with {manual}",
            cfg.host
        ),
        _ => bail!("building mirako on {} failed (output above); {manual} there should say why", cfg.host),
    }
}

/// Remote sh: cargo from PATH or `~/.cargo/bin` (exit 3 when neither) installs the tag into a
/// scratch root, then the binary moves to `remote_bin`.
fn build_script(version: &str, remote_bin: &str) -> String {
    format!(
        "C=$(command -v cargo || echo \"$HOME/.cargo/bin/cargo\"); [ -x \"$C\" ] || exit 3; \
         \"$C\" install --git {REPO} --tag v{version} --force --root \"$HOME/.mirako-build\" && \
         mkdir -p \"$(dirname {remote_bin})\" && mv \"$HOME/.mirako-build/bin/mirako\" {remote_bin} && {remote_bin} --version"
    )
}

pub fn project_root_for(path: Option<&PathBuf>) -> Result<PathBuf> {
    let start = match path {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };
    crate::config::find_project_root(&start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_script_installs_this_versions_tag_and_exits_3_without_cargo() {
        let s = build_script("0.4.0", "~/.local/bin/mirako");
        assert!(s.contains(&format!("install --git {REPO} --tag v0.4.0")), "{s}");
        assert!(s.contains("|| exit 3;"), "{s}");
        assert!(
            s.ends_with("mv \"$HOME/.mirako-build/bin/mirako\" ~/.local/bin/mirako && ~/.local/bin/mirako --version"),
            "{s}"
        );
    }

    #[test]
    fn the_local_fallback_runs_the_bat_beside_the_command_on_windows_only() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("gradlew"), "").unwrap();
        fs::write(dir.path().join("gradlew.bat"), "").unwrap();
        let wrapper = PathBuf::from(local_program(dir.path(), "./gradlew"));
        if cfg!(windows) {
            assert_eq!(wrapper, dir.path().join("gradlew.bat"));
        } else {
            assert_eq!(wrapper, Path::new("./gradlew"));
        }
        assert_eq!(local_program(dir.path(), "echo"), OsString::from("echo"));
    }

    #[test]
    fn forwarded_env_takes_the_named_variables_and_the_prefixed_ones() {
        let vars = || {
            [
                ("PATH", "/bin"),
                ("TOKEN", "t"),
                ("TOKEN2", "no"),
                ("ORG_GRADLE_PROJECT_b", "2"),
                ("ORG_GRADLE_PROJECT_a", "1"),
            ]
            .into_iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        };
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            forwarded_env(&names(&["TOKEN", "ORG_GRADLE_PROJECT_*", "UNSET"]), vars()),
            vec![
                pair("ORG_GRADLE_PROJECT_a", "1"),
                pair("ORG_GRADLE_PROJECT_b", "2"),
                pair("TOKEN", "t")
            ]
        );
        assert!(forwarded_env(&[], vars()).is_empty());
    }

    #[test]
    fn portable_properties_drops_the_keys_naming_local_paths_and_nothing_else() {
        let text = b"# comment\nsdk.dir=/Users/me/Library/Android/sdk\nMAPS_API_KEY=abc\n  ndk.dir = /x\ncmake.dir:/y\r\nsdk.dirty=keep\nmirako.enabled=false\nlast=1";
        let want = b"# comment\nMAPS_API_KEY=abc\nsdk.dirty=keep\nmirako.enabled=false\nlast=1";
        assert_eq!(portable_properties(text), want);
        assert_eq!(portable_properties(b"sdk.dir=/x"), b"");
        assert_eq!(portable_properties(b""), b"");
        assert!(is_local_properties("local.properties") && is_local_properties("app/local.properties"));
        assert!(!is_local_properties("app/xlocal.properties") && !is_local_properties("local.properties/x"));
    }

    #[test]
    fn human_picks_the_unit_at_each_boundary() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1024), "1 KB");
        assert_eq!(human((1 << 20) - 1), "1024 KB");
        assert_eq!(human(1 << 20), "1.0 MB");
        assert_eq!(human(3 << 19), "1.5 MB");
        assert_eq!(human((1 << 30) - 1), "1024.0 MB");
        assert_eq!(human(1 << 30), "1.0 GB");
        assert_eq!(human(5 << 29), "2.5 GB");
    }

    #[test]
    fn secs_is_seconds_with_one_decimal() {
        let s = secs(Instant::now());
        assert!(s.ends_with('s'), "{s}");
        let num = s.trim_end_matches('s');
        assert_eq!(num.split('.').nth(1).map(str::len), Some(1), "{s}");
        assert!(num.parse::<f64>().unwrap() < 1.0);
    }
}
