//! The local side: ssh to the remote agent, push the diff, run the command, pull the diff.

use crate::config::Config;
use crate::delta;
use crate::index::Index;
use crate::patterns::Matcher;
use crate::proto::{self, read_frame, write_frame, Chunk, DeltaChunk, Entry, Kind, Req, Resp, CHUNK, ZSTD_LEVEL};
use crate::rewrite::LineRewriter;
use crate::server::read_full;
use crate::xfer::Inbox;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

/// Non-interactive, and give up fast when the host is asleep or off the network so the
/// Gradle shim falls back to a local build instead of hanging on the TCP timeout.
const SSH_OPTS: &[&str] = &["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"];

pub struct Session {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    writer: BufWriter<std::process::ChildStdin>,
    pub remote_home: String,
    pub remote_os: String,
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
    pub fn connect(cfg: &Config) -> Result<Self> {
        let mut cmd = Command::new(&cfg.ssh[0]);
        cmd.args(&cfg.ssh[1..])
            .args(SSH_OPTS)
            .arg(&cfg.host)
            .arg(format!("{} serve", cfg.remote_bin));
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("starting ssh")?;
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut writer = BufWriter::new(child.stdin.take().unwrap());
        write_frame(
            &mut writer,
            &Req::Hello {
                version: proto::VERSION.into(),
            },
        )?;
        writer.flush()?;
        let hello: Resp = read_frame(&mut reader).map_err(|e| {
            let _ = child.wait();
            anyhow!(
                "no answer from `{} serve` on {} ({e}). Is the host reachable and mirako installed there? Try `mirako remote-install`.",
                cfg.remote_bin,
                cfg.host
            )
        })?;
        match hello {
            Resp::Hello { version, home, os } => {
                if version != proto::VERSION {
                    bail!(
                        "remote mirako is {version}, local is {}: run `mirako remote-install`",
                        proto::VERSION
                    );
                }
                Ok(Self {
                    child,
                    reader,
                    writer,
                    remote_home: home,
                    remote_os: os,
                })
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
        match read_frame(&mut self.reader)? {
            Resp::Error { msg } => bail!("remote: {msg}"),
            r => Ok(r),
        }
    }

    pub fn manifest(&mut self, dir: &str, exclude: &[String]) -> Result<(String, Vec<Entry>)> {
        match self.call(&Req::Manifest {
            dir: dir.into(),
            exclude: exclude.to_vec(),
        })? {
            Resp::Manifest { root, entries } => Ok((root, entries)),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    fn flush(&mut self) -> Result<Vec<String>> {
        match self.call(&Req::Flush)? {
            Resp::Ack { failed } => Ok(failed),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Upload: make the remote copy of the upload scope identical to the local one.
    pub fn push(&mut self, root: &Path, index: &mut Index, remote_dir: &str, exclude: &[String]) -> Result<Stats> {
        let matcher = Matcher::new(exclude)?;
        let local = index.scan(root, &matcher)?;
        let (_remote_root, remote) = self.manifest(remote_dir, exclude)?;
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
        for e in &to_send {
            match &e.kind {
                Kind::Symlink { target } => self.send(&Req::Symlink {
                    path: e.path.clone(),
                    target: target.clone(),
                })?,
                Kind::File => {
                    if let Some(sig) = sigs.get(&e.path) {
                        stats.wire += self.send_delta(root, e, sig)?;
                        stats.deltas += 1;
                    } else {
                        stats.wire += self.send_file(root, e)?;
                    }
                    stats.bytes += e.size;
                }
            }
            stats.files += 1;
        }
        let failed = self.flush()?;
        if !failed.is_empty() {
            // a delta rebuilt to the wrong hash on the remote: resend those files whole
            let by_path: HashMap<&str, &Entry> = to_send.iter().map(|e| (e.path.as_str(), *e)).collect();
            for p in &failed {
                if let Some(e) = by_path.get(p.as_str()) {
                    stats.wire += self.send_file(root, e)?;
                }
            }
            let again = self.flush()?;
            if !again.is_empty() {
                bail!("remote could not store {}", again.join(", "));
            }
        }
        Ok(stats)
    }

    fn send_file(&mut self, root: &Path, e: &Entry) -> Result<u64> {
        let full = root.join(&e.path);
        let mut f = fs::File::open(&full).with_context(|| format!("reading {}", full.display()))?;
        let mut offset = 0u64;
        let mut wire = 0u64;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = read_full(&mut f, &mut buf)?;
            let last = offset + n as u64 >= e.size || n == 0;
            let data = zstd::encode_all(&buf[..n], ZSTD_LEVEL)?;
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
            if last {
                break;
            }
        }
        Ok(wire)
    }

    fn send_delta(&mut self, root: &Path, e: &Entry, sig: &proto::Signature) -> Result<u64> {
        let data = fs::read(root.join(&e.path))?;
        let hash = *blake3::hash(&data).as_bytes();
        let mut wire = 0u64;
        let mut ops = Vec::new();
        let mut pending = 0usize;
        let mut frames: Vec<DeltaChunk> = Vec::new();
        delta::delta(&data, sig, |op| {
            pending += match &op {
                proto::Op::Data(d) => d.len(),
                _ => 8,
            };
            ops.push(op);
            if pending >= CHUNK {
                frames.push(DeltaChunk {
                    path: e.path.clone(),
                    mode: e.mode,
                    mtime_ns: e.mtime_ns,
                    size: e.size,
                    hash,
                    ops: std::mem::take(&mut ops),
                    last: false,
                });
                pending = 0;
            }
            Ok(())
        })?;
        frames.push(DeltaChunk {
            path: e.path.clone(),
            mode: e.mode,
            mtime_ns: e.mtime_ns,
            size: e.size,
            hash,
            ops,
            last: true,
        });
        for f in frames {
            wire += f
                .ops
                .iter()
                .map(|o| if let proto::Op::Data(d) = o { d.len() as u64 } else { 8 })
                .sum::<u64>();
            self.send(&Req::Delta(f))?;
        }
        Ok(wire)
    }

    /// Run `cmd` in the remote project dir, streaming output with paths rewritten. Returns the exit code.
    pub fn exec(&mut self, remote_dir: &str, remote_abs: &str, local_root: &Path, cmd: &[String]) -> Result<i32> {
        self.send(&Req::Exec {
            dir: remote_dir.into(),
            cmd: cmd.to_vec(),
        })?;
        self.writer.flush()?;
        let local = local_root.to_string_lossy().into_owned();
        let mut out_rw = LineRewriter::new(remote_abs, &local);
        let mut err_rw = LineRewriter::new(remote_abs, &local);
        let stdout = io::stdout();
        let stderr = io::stderr();
        loop {
            match self.recv()? {
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

    /// Download: bring every remote file of the download scope that differs from the local copy.
    /// Never deletes anything locally. Big files the local side already has come as deltas.
    pub fn pull(&mut self, root: &Path, index: &mut Index, remote_dir: &str, exclude: &[String]) -> Result<Stats> {
        let matcher = Matcher::new(exclude)?;
        let local = index.scan(root, &matcher)?;
        let local_map: HashMap<&str, &Entry> = local.iter().map(|e| (e.path.as_str(), e)).collect();
        let (_r, remote) = self.manifest(remote_dir, exclude)?;
        let wanted: Vec<&Entry> = remote
            .iter()
            .filter(|e| match local_map.get(e.path.as_str()) {
                Some(l) => l.hash != e.hash || l.kind != e.kind,
                None => true,
            })
            .collect();
        let mut stats = Stats::default();
        if wanted.is_empty() {
            return Ok(stats);
        }
        let mut sigs = Vec::new();
        for e in &wanted {
            if e.kind == Kind::File && delta::delta_worthwhile(e.size) {
                if let Some(l) = local_map.get(e.path.as_str()) {
                    if l.kind == Kind::File {
                        sigs.push((e.path.clone(), delta::signature(&root.join(&e.path))?));
                    }
                }
            }
        }
        let paths: Vec<String> = wanted.iter().map(|e| e.path.clone()).collect();
        let mut retry = self.fetch(root, index, remote_dir, paths, sigs, &mut stats)?;
        if !retry.is_empty() {
            // deltas that rebuilt to the wrong hash: fetch those whole
            retry = self.fetch(root, index, remote_dir, retry, Vec::new(), &mut stats)?;
            if !retry.is_empty() {
                bail!("could not download {}", retry.join(", "));
            }
        }
        Ok(stats)
    }

    fn fetch(
        &mut self,
        root: &Path,
        index: &mut Index,
        remote_dir: &str,
        paths: Vec<String>,
        sigs: Vec<(String, proto::Signature)>,
        stats: &mut Stats,
    ) -> Result<Vec<String>> {
        self.send(&Req::Fetch {
            dir: remote_dir.into(),
            paths,
            sigs,
        })?;
        self.writer.flush()?;
        let mut inbox = Inbox::default();
        let mut failed = Vec::new();
        loop {
            match self.recv()? {
                Resp::Put(chunk) => {
                    stats.wire += chunk.data.len() as u64;
                    if let Some(f) = inbox.put(root, &chunk)? {
                        stats.files += 1;
                        stats.bytes += f.size;
                        index.remember(&f.path, f.size, f.mtime_ns, f.hash);
                    }
                }
                Resp::Delta(chunk) => {
                    stats.wire += chunk
                        .ops
                        .iter()
                        .map(|o| if let proto::Op::Data(d) = o { d.len() as u64 } else { 8 })
                        .sum::<u64>();
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
                Resp::Symlink { path, target } => {
                    inbox.symlink(root, &path, &target)?;
                    stats.files += 1;
                }
                Resp::End => break,
                other => bail!("unexpected reply {other:?}"),
            }
        }
        Ok(failed)
    }

    pub fn close(mut self) {
        let _ = self.send(&Req::Bye);
        let _ = self.writer.flush();
        let _ = self.child.wait();
    }
}

pub struct RunOptions {
    pub push: bool,
    pub pull: bool,
    pub quiet: bool,
}

fn human(bytes: u64) -> String {
    if bytes >= 1 << 20 {
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

/// The whole round trip. Returns the exit code to end the process with.
pub fn run(root: &Path, cfg: &Config, cmd: &[String], opts: &RunOptions) -> Result<i32> {
    let start = Instant::now();
    let remote_dir = cfg.remote_dir(root);
    let mut session = match Session::connect(cfg) {
        Ok(s) => s,
        Err(e) if cfg.fallback && !cmd.is_empty() => {
            eprintln!("mirako: {e:#}");
            eprintln!("mirako: running locally instead");
            return run_local(root, cmd);
        }
        Err(e) => return Err(e),
    };
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

    // the remote project path for output rewriting (`~` expanded with the agent's home)
    let remote_abs = match remote_dir.strip_prefix("~/") {
        Some(r) => format!("{}/{r}", session.remote_home),
        None => remote_dir.clone(),
    };

    if opts.push {
        let t = Instant::now();
        let s = session.push(root, &mut index, &remote_dir, &cfg.upload_excludes())?;
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
    }

    let mut code = 0;
    if !cmd.is_empty() {
        let t = Instant::now();
        code = session.exec(&remote_dir, &remote_abs, root, cmd)?;
        say(format!("exec   exit {code}, {}", secs(t)));
    }

    if opts.pull {
        let t = Instant::now();
        let s = session.pull(root, &mut index, &remote_dir, &cfg.download_excludes())?;
        index.save();
        say(format!(
            "pull   {} files ({} as delta), {} → {} on the wire, {}",
            s.files,
            s.deltas,
            human(s.bytes),
            human(s.wire),
            secs(t)
        ));
    }
    session.close();
    say(format!("total  {}", secs(start)));
    Ok(code)
}

pub fn run_local(root: &Path, cmd: &[String]) -> Result<i32> {
    // tell the Gradle shim not to try the remote again
    let status = Command::new(&cmd[0])
        .args(&cmd[1..])
        .env("MIRAKO_LOCAL", "1")
        .current_dir(root)
        .status()
        .with_context(|| format!("running {}", cmd[0]))?;
    Ok(status.code().unwrap_or(-1))
}

/// Quick reachability + version handshake; used by the Gradle shim before hijacking a build.
pub fn check(cfg: &Config) -> Result<()> {
    let s = Session::connect(cfg)?;
    let os = s.remote_os.clone();
    s.close();
    println!("mirako {}: {} ok ({os})", proto::VERSION, cfg.host);
    Ok(())
}

/// Copy this binary to `remote_bin` on the host when OS/arch match.
pub fn remote_install(cfg: &Config) -> Result<()> {
    let uname = |host: Option<&str>| -> Result<String> {
        let out = match host {
            Some(h) => Command::new(&cfg.ssh[0])
                .args(&cfg.ssh[1..])
                .args(SSH_OPTS)
                .arg(h)
                .arg("uname -sm")
                .output()?,
            None => Command::new("uname").arg("-sm").output()?,
        };
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let local = uname(None)?;
    let remote = uname(Some(&cfg.host))?;
    if remote.is_empty() {
        bail!("cannot ssh to {}", cfg.host);
    }
    if local != remote {
        bail!("{} is `{remote}` but this binary is for `{local}`. Build mirako there instead: `cargo install --git https://github.com/Nam0101/mirako`", cfg.host);
    }
    let me = std::env::current_exe()?;
    let bytes = fs::read(&me)?;
    let dest = cfg.remote_bin.clone();
    let script =
        format!("mkdir -p \"$(dirname {dest})\" && cat > {dest}.tmp && chmod +x {dest}.tmp && mv {dest}.tmp {dest} && {dest} --version");
    let mut child = Command::new(&cfg.ssh[0])
        .args(&cfg.ssh[1..])
        .args(SSH_OPTS)
        .arg(&cfg.host)
        .arg(script)
        .stdin(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(&bytes)?;
    let status = child.wait()?;
    if !status.success() {
        bail!("install on {} failed", cfg.host);
    }
    println!("installed {} ({}) on {}", dest, human(bytes.len() as u64), cfg.host);
    Ok(())
}

pub fn project_root_for(path: Option<&PathBuf>) -> Result<PathBuf> {
    let start = match path {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };
    crate::config::find_project_root(&start)
}
