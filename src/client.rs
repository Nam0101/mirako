//! The local side: ssh to the remote agent, push the diff, run the command, pull the diff.

use crate::config::Config;
use crate::delta;
use crate::index::Index;
use crate::patterns::Matcher;
use crate::proto::{self, read_frame, write_frame, Chunk, DeltaChunk, Entry, GcReport, GcReq, Kind, Req, Resp, CHUNK};
use crate::rewrite::LineRewriter;
use crate::server::read_full;
use crate::xfer::Inbox;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
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

    /// Asks the remote for its manifest and runs `local` (the local scan) while it is scanning,
    /// so the two scans overlap instead of adding up.
    fn manifests<T>(&mut self, dir: &str, exclude: &[String], local: impl FnOnce() -> Result<T>) -> Result<(T, Vec<Entry>)> {
        self.send(&Req::Manifest {
            dir: dir.into(),
            exclude: exclude.to_vec(),
        })?;
        self.writer.flush()?;
        let mine = local()?;
        match self.recv()? {
            Resp::Manifest { entries, .. } => Ok((mine, entries)),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// `Flush`, then the requests of the phases after the push, in one write: the agent answers
    /// the `Ack` and goes straight on to them, so the push, the command and the pull manifest
    /// cost one round trip between them instead of one each. Returns the failed delta pushes.
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

    /// Upload: make the remote copy of the upload scope identical to the local one. `after` is
    /// queued behind the final `Flush` (see `flush_and`).
    pub fn push(&mut self, root: &Path, index: &mut Index, remote_dir: &str, exclude: &[String], after: &[Req]) -> Result<Stats> {
        let matcher = Matcher::new(exclude)?;
        let (local, remote) = self.manifests(remote_dir, exclude, || index.scan(root, &matcher))?;
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
                    stats.wire += self.send_file(root, e)?;
                }
            }
            let again = self.flush_and(after)?;
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
            if last {
                break;
            }
        }
        Ok(wire)
    }

    fn send_delta(&mut self, root: &Path, e: &Entry, sig: &proto::Signature) -> Result<u64> {
        let data = fs::read(root.join(&e.path))?;
        let head = DeltaChunk {
            path: e.path.clone(),
            mode: e.mode,
            mtime_ns: e.mtime_ns,
            size: e.size,
            hash: *blake3::hash(&data).as_bytes(),
            ops: Vec::new(),
            last: false,
        };
        let writer = &mut self.writer;
        delta::stream(&data, sig, &head, |frame| write_frame(writer, &Req::Delta(frame)))
    }

    /// Streams the output of the `Exec` queued earlier, remote paths rewritten to local ones.
    /// Returns the exit code.
    pub fn exec_output(&mut self, remote_dir: &str, local_root: &Path) -> Result<i32> {
        // the remote project path (`~` expanded with the agent's home) for output rewriting
        let remote_abs = match remote_dir.strip_prefix("~/") {
            Some(r) => format!("{}/{r}", self.remote_home),
            None => remote_dir.to_string(),
        };
        let local = local_root.to_string_lossy().into_owned();
        let mut out_rw = LineRewriter::new(&remote_abs, &local);
        let mut err_rw = LineRewriter::new(&remote_abs, &local);
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
    /// The pull `Manifest` was queued earlier and `local` scanned while the command ran.
    pub fn pull(&mut self, root: &Path, index: &mut Index, remote_dir: &str, local: Vec<Entry>) -> Result<Stats> {
        let local_map: HashMap<&str, &Entry> = local.iter().map(|e| (e.path.as_str(), e)).collect();
        let remote = match self.recv()? {
            Resp::Manifest { entries, .. } => entries,
            other => bail!("unexpected reply {other:?}"),
        };
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

pub struct RunOptions {
    pub push: bool,
    pub pull: bool,
    pub quiet: bool,
}

fn human(bytes: u64) -> String {
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
    let pull_excludes = cfg.download_excludes();

    // the requests of the later phases go out with the push's `Flush`, so the agent starts the
    // command, and then its pull scan, without waiting for another round trip
    let mut after = Vec::new();
    if !cmd.is_empty() {
        after.push(Req::Exec {
            dir: remote_dir.clone(),
            cmd: cmd.to_vec(),
        });
    }
    if opts.pull {
        after.push(Req::Manifest {
            dir: remote_dir.clone(),
            exclude: pull_excludes.clone(),
        });
    }

    if opts.push {
        let t = Instant::now();
        let s = session.push(root, &mut index, &remote_dir, &cfg.upload_excludes(), &after)?;
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

    // the local side of the pull is scanned on its own thread while the command runs remotely
    let mut code = 0;
    let local = thread::scope(|scope| -> Result<Option<Vec<Entry>>> {
        let scan = scope.spawn(|| match opts.pull {
            true => index.scan(root, &Matcher::new(&pull_excludes)?).map(Some),
            false => Ok(None),
        });
        if !cmd.is_empty() {
            let t = Instant::now();
            code = session.exec_output(&remote_dir, root)?;
            say(format!("exec   exit {code}, {}", secs(t)));
        }
        scan.join().expect("scan thread panicked")
    })?;

    if let Some(local) = local {
        let t = Instant::now();
        let s = session.pull(root, &mut index, &remote_dir, local)?;
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

    // housekeeping on the host: stale mirrors, intermediates the client never downloads (only
    // once they are not needed any more, i.e. after a pull), Gradle's retention. Never fails a build.
    if cfg.gc_days > 0 || (opts.pull && !cfg.gc_after_pull.is_empty()) {
        let req = GcReq {
            folder: cfg.remote_folder.clone(),
            keep_days: (cfg.gc_days > 0).then_some(cfg.gc_days),
            current: Some(remote_dir.clone()),
            build: if opts.pull { cfg.gc_after_pull.clone() } else { Vec::new() },
            gradle_days: cfg.gc_days,
            dry_run: false,
            sizes: false,
        };
        match session.gc(req) {
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
        }
    }
    session.close();
    say(format!("total  {}", secs(start)));
    Ok(code)
}

/// `mirako gc`: list the project copies on the host and remove the stale ones.
pub fn gc(cfg: &Config, days: Option<u32>, dry_run: bool) -> Result<()> {
    let keep = days.or((cfg.gc_days > 0).then_some(cfg.gc_days));
    let mut session = Session::connect(cfg)?;
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
