//! `mirako serve`: the agent the client starts over ssh. Speaks the protocol on stdin/stdout,
//! logs to stderr only.

use crate::delta;
use crate::gc;
use crate::index::{self, Index};
use crate::patterns::Matcher;
use crate::proto::{self, read_frame, write_frame, Chunk, DeltaChunk, Entry, Kind, Req, Resp, Signature, CHUNK};
use crate::xfer::{canonical, safe_join, Inbox};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

type Out = Arc<Mutex<BufWriter<io::Stdout>>>;

/// Requests read ahead of the one being served. Few, so a push still meets back-pressure, and
/// enough for what the client queues behind an `Exec` (`Pull`, `Gc`): the reader thread is then
/// back in `read` and sees the client go away while the command runs.
const READ_AHEAD: usize = 4;

/// What a cancelled command gets between SIGTERM and SIGKILL.
const GRACE: Duration = Duration::from_secs(2);

/// Shared by the reader thread and `exec`: whether the client hung up, and the process group of
/// the command that has to stop when it does (0: none running).
#[derive(Default)]
struct Hangup {
    gone: AtomicBool,
    group: AtomicI32,
    /// Windows: the job object holding the running command and every process it started, which a
    /// hang-up terminates. Behind a lock, so that it is never used and closed at once.
    #[cfg(windows)]
    job: Mutex<Option<job::Job>>,
}

impl Hangup {
    /// The client went away (a cancelled build): SIGTERM to the running command and everything
    /// it started, SIGKILL after `GRACE` to a command that is still the one running then.
    fn stop(&self) {
        let group = self.group.load(Ordering::SeqCst);
        if group == 0 {
            return;
        }
        self.signal(group, false);
        thread::sleep(GRACE);
        // unchanged: `exec` is still on it, so the command or something it started lives, and while
        // one of them does the id cannot have gone to another group
        if self.group.load(Ordering::SeqCst) == group {
            self.signal(group, true);
        }
    }

    /// SIGTERM, or with `force` SIGKILL, to every process of the group.
    #[cfg(unix)]
    fn signal(&self, group: i32, force: bool) {
        // SAFETY: a plain syscall; a group that is gone already answers ESRCH
        unsafe { libc::killpg(group, if force { libc::SIGKILL } else { libc::SIGTERM }) };
    }

    /// Windows has no SIGTERM to send a console program it shares no console with: the job of the
    /// command is terminated at the first signal, every process in it.
    #[cfg(windows)]
    fn signal(&self, _: i32, _: bool) {
        if let Some(job) = self.job.lock().unwrap().as_ref() {
            job.terminate();
        }
    }
}

/// A job object: a command and the processes it starts from then on, all of which
/// `TerminateJobObject` stops. Closing it leaves them running (no `KILL_ON_JOB_CLOSE`), so a build
/// that ends leaves its Gradle daemon up for the next one.
#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject};

    pub struct Job(HANDLE);

    // SAFETY: a job handle can be used, and closed, from any thread
    unsafe impl Send for Job {}

    impl Job {
        /// A job holding `child`, `None` when Windows refuses one (the command then runs to its end
        /// on a hang-up). What `child` starts in the moment before it joins stays outside: nothing,
        /// for a `cmd.exe` that has a batch file to read first.
        pub fn new(child: &std::process::Child) -> Option<Self> {
            // SAFETY: plain calls; the handle belongs to the `Job` from its creation on
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                let job = Job(job);
                (AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) != 0).then_some(job)
            }
        }

        pub fn terminate(&self) {
            // SAFETY: `self.0` is open until `drop`
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closed once, here
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn send(out: &Out, resp: &Resp) -> Result<()> {
    let mut w = out.lock().unwrap();
    write_frame(&mut *w, resp)?;
    w.flush()?;
    Ok(())
}

/// A frame the client does not wait for (the pieces of a `Fetch`): leave it in the buffer so
/// small files coalesce into fewer writes; `Resp::End` flushes.
fn stream(out: &Out, resp: &Resp) -> Result<()> {
    write_frame(&mut *out.lock().unwrap(), resp)
}

pub fn expand_home(p: &str) -> PathBuf {
    let path = match (p.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(p),
    };
    // the wire's `/` as Windows writes it: the root goes into the build's output that way
    if cfg!(windows) {
        path.components().collect()
    } else {
        path
    }
}

/// The program of a command as this machine starts it in `dir`. Commands are spelled for a Unix
/// host (`./gradlew`), and Windows looks a relative path up from the directory of the process
/// that starts it, not the command's: there such a path is made absolute under `dir`, and without
/// an extension it is the `.exe`, `.bat` or `.cmd` of that name (the `gradlew.bat` beside
/// `gradlew`). A bare name that `PATH` has only as a `.bat` or `.cmd` (`gradle`) is that file:
/// Windows itself looks for an `.exe` alone.
pub fn program_in(dir: &Path, prog: &str) -> OsString {
    if cfg!(windows) {
        windows_program(dir, prog, std::env::var_os("PATH").as_deref())
    } else {
        prog.into()
    }
}

fn windows_program(dir: &Path, prog: &str, path_var: Option<&OsStr>) -> OsString {
    if prog.contains(['/', '\\']) {
        let full: PathBuf = dir.join(prog).components().collect();
        if full.extension().is_none() {
            for ext in ["exe", "bat", "cmd"] {
                let it = full.with_extension(ext);
                if it.is_file() {
                    return it.into();
                }
            }
        }
        return full.into();
    }
    if Path::new(prog).extension().is_none() {
        for d in path_var.map(std::env::split_paths).into_iter().flatten() {
            if d.join(format!("{prog}.exe")).is_file() {
                break; // the one Windows finds
            }
            for ext in ["bat", "cmd"] {
                let it = d.join(format!("{prog}.{ext}"));
                if it.is_file() {
                    return it.into();
                }
            }
        }
    }
    prog.into()
}

pub fn serve() -> Result<()> {
    let out: Out = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    // requests are read on a thread of their own: the loop below is busy while a command runs,
    // and a client that closes the connection then (a cancelled build) has to stop that command
    let hangup = Arc::new(Hangup::default());
    let (queue, requests) = mpsc::sync_channel::<Req>(READ_AHEAD);
    thread::spawn({
        let hangup = hangup.clone();
        move || {
            let mut input = BufReader::new(io::stdin());
            while let Ok(req) = read_frame(&mut input) {
                if queue.send(req).is_err() {
                    return;
                }
            }
            hangup.gone.store(true, Ordering::SeqCst);
            hangup.stop();
        }
    });
    let mut inbox = Inbox::mirror();
    let mut index: Option<(PathBuf, Index)> = None;
    let mut failed_deltas: Vec<String> = Vec::new();
    // an `Ack` reported failed deltas: the client resends them, so refuse to run anything until
    // a clean `Flush` says the tree is whole again
    let mut blocked = false;

    loop {
        let Ok(req) = requests.recv() else {
            return Ok(()); // client went away
        };
        if hangup.gone.load(Ordering::SeqCst) {
            // and so it did, with requests read ahead: nobody waits for their answers, and its next
            // run may be at work in this tree already
            return Ok(());
        }
        // the pool writes of a push end before anything else is served: a `Flush` saves the index
        // they update, an `Exec` builds the files they write. A failed one is this request's reply.
        if !matches!(req, Req::Put(_)) {
            match inbox.settle() {
                Ok(written) => {
                    if let Some((_, idx)) = index.as_mut() {
                        for f in written {
                            idx.remember(&f.path, f.size, f.mtime_ns, f.hash);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("mirako serve: {e:#}");
                    send(&out, &Resp::Error { msg: format!("{e:#}") })?;
                    continue;
                }
            }
        }
        let result: Result<()> = (|| match req {
            Req::Hello { version } => {
                send(
                    &out,
                    &Resp::Hello {
                        version: proto::VERSION.into(),
                        home: dirs::home_dir().unwrap_or_default().to_string_lossy().into_owned(),
                        os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
                    },
                )?;
                if version != proto::VERSION {
                    // the client bails on that reply; whatever it queued behind its `Hello` was
                    // written for another protocol, so do not read on
                    eprintln!("mirako serve {}: client is {version}", proto::VERSION);
                    std::process::exit(0);
                }
                Ok(())
            }
            Req::Manifest { dir, exclude } => {
                let (root, idx) = open_root(&mut index, &dir)?;
                let entries = idx.scan(root, &Matcher::new(&exclude)?)?;
                idx.save();
                send(
                    &out,
                    &Resp::Manifest {
                        root: root.to_string_lossy().into_owned(),
                        entries,
                    },
                )
            }
            Req::Put(chunk) => {
                let (root, idx) = index.as_mut().context("Put before Manifest")?;
                if let Some(f) = inbox.put(root, chunk)? {
                    idx.remember(&f.path, f.size, f.mtime_ns, f.hash);
                }
                Ok(())
            }
            Req::Delta(chunk) => {
                let (root, idx) = index.as_mut().context("Delta before Manifest")?;
                if let Some(f) = inbox.delta(root, &chunk, delta::BLOCK as u32)? {
                    if f.ok {
                        idx.remember(&f.path, f.size, f.mtime_ns, f.hash);
                    } else {
                        failed_deltas.push(f.path);
                    }
                }
                Ok(())
            }
            Req::Symlink { path, target } => {
                let (root, _) = index.as_ref().context("Symlink before Manifest")?;
                match inbox.symlink(root, &path, &target) {
                    // Windows makes no links (see `xfer::make_symlink`): said, and the build runs without it
                    Err(e) if cfg!(windows) => {
                        eprintln!("mirako serve: {e:#}");
                        Ok(())
                    }
                    r => r,
                }
            }
            Req::Delete { paths } => {
                let (root, _) = index.as_ref().context("Delete before Manifest")?;
                for p in paths {
                    let full = safe_join(root, &p)?;
                    match fs::symlink_metadata(&full) {
                        Ok(md) if md.is_dir() => fs::remove_dir_all(&full)?,
                        Ok(_) => fs::remove_file(&full)?,
                        Err(_) => {}
                    }
                }
                Ok(())
            }
            Req::Sigs { dir, paths } => {
                let root = canonical(&expand_home(&dir))?;
                let mut sigs = Vec::new();
                for p in paths {
                    let full = safe_join(&root, &p)?;
                    if let Ok(md) = full.metadata() {
                        if md.is_file() && delta::delta_worthwhile(md.len()) {
                            sigs.push((p, delta::signature(&full)?));
                        }
                    }
                }
                send(&out, &Resp::Sigs(sigs))
            }
            Req::Flush => {
                if let Some((_, idx)) = index.as_mut() {
                    idx.save();
                }
                let failed = std::mem::take(&mut failed_deltas);
                blocked = !failed.is_empty();
                send(&out, &Resp::Ack { failed })
            }
            Req::Exec { dir, cmd, env } => {
                if blocked {
                    bail!("a delta push did not rebuild; waiting for the resend before running anything");
                }
                exec(&out, &hangup, &expand_home(&dir), &cmd, env)
            }
            Req::Pull { dir, exclude, have, sigs } => {
                let (root, idx) = open_root(&mut index, &dir)?;
                let entries = idx.scan(root, &Matcher::new(&exclude)?)?;
                idx.save();
                let have: HashMap<&str, &Entry> = have.iter().map(|e| (e.path.as_str(), e)).collect();
                let sigs: HashMap<String, Signature> = sigs.into_iter().collect();
                for e in &entries {
                    let differs = have
                        .get(e.path.as_str())
                        .map(|l| l.hash != e.hash || l.kind != e.kind)
                        .unwrap_or(true);
                    if differs {
                        send_entry(&out, root, e, sigs.get(&e.path))?;
                    }
                }
                send(&out, &Resp::End)
            }
            Req::Fetch { dir, paths } => {
                let root = canonical(&expand_home(&dir))?;
                for p in paths {
                    let full = safe_join(&root, &p)?;
                    let md = fs::symlink_metadata(&full)?;
                    if md.file_type().is_symlink() {
                        let target = fs::read_link(&full)?.to_string_lossy().into_owned();
                        stream(&out, &Resp::Symlink { path: p, target })?;
                    } else {
                        send_file(&out, &full, &p, &md)?;
                    }
                }
                send(&out, &Resp::End)
            }
            Req::Gc(req) => send(&out, &Resp::Gc(gc::collect(&req)?)),
            Req::Bye => std::process::exit(0),
        })();
        if let Err(e) = result {
            eprintln!("mirako serve: {e:#}");
            send(&out, &Resp::Error { msg: format!("{e:#}") })?;
        }
    }
}

/// The mirror `dir` (created if missing) and the agent's index of it, kept across requests.
fn open_root<'a>(index: &'a mut Option<(PathBuf, Index)>, dir: &str) -> Result<(&'a Path, &'a mut Index)> {
    let root = expand_home(dir);
    fs::create_dir_all(&root)?;
    let root = canonical(&root)?;
    if index.as_ref().map(|(r, _)| r != &root).unwrap_or(true) {
        *index = Some((root.clone(), Index::open(&root)));
    }
    let (root, idx) = index.as_mut().unwrap();
    Ok((root.as_path(), idx))
}

/// Stream one entry of the download scope: a symlink as is, a big file the client holds a
/// signature of as a delta, anything else whole.
fn send_entry(out: &Out, root: &Path, e: &Entry, sig: Option<&Signature>) -> Result<()> {
    match &e.kind {
        Kind::Symlink { target } => stream(
            out,
            &Resp::Symlink {
                path: e.path.clone(),
                target: target.clone(),
            },
        ),
        Kind::File => {
            let full = root.join(&e.path);
            let md = fs::metadata(&full)?;
            match sig.filter(|_| delta::delta_worthwhile(e.size)) {
                Some(sig) => send_delta(out, &full, &e.path, &md, e.hash, sig),
                None => send_file(out, &full, &e.path, &md),
            }
        }
    }
}

/// Stream one file as `Resp::Put` chunks.
fn send_file(out: &Out, full: &Path, rel: &str, md: &fs::Metadata) -> Result<()> {
    let mut f = fs::File::open(full)?;
    let size = md.len();
    let mode = index::mode(md);
    let mtime_ns = index::mtime_ns(md);
    let mut offset = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = read_full(&mut f, &mut buf)?;
        let last = offset + n as u64 >= size || n == 0;
        let data = proto::compress(&buf[..n])?;
        stream(
            out,
            &Resp::Put(Chunk {
                path: rel.to_string(),
                mode,
                mtime_ns,
                size,
                offset,
                last,
                data,
            }),
        )?;
        offset += n as u64;
        if last {
            break;
        }
    }
    Ok(())
}

/// Stream one file as `Resp::Delta` chunks against the client's signature. `hash` is the file's
/// blake3 from the scan that found it changed; the client checks the rebuilt file against it.
fn send_delta(out: &Out, full: &Path, rel: &str, md: &fs::Metadata, hash: [u8; 32], sig: &Signature) -> Result<()> {
    let data = fs::read(full)?;
    let head = DeltaChunk {
        path: rel.into(),
        mode: index::mode(md),
        mtime_ns: index::mtime_ns(md),
        size: data.len() as u64,
        hash,
        ops: Vec::new(),
        last: false,
    };
    delta::stream(&data, sig, &head, |frame| stream(out, &Resp::Delta(frame)))?;
    Ok(())
}

pub fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn exec(out: &Out, hangup: &Arc<Hangup>, dir: &Path, cmd: &[String], env: Vec<(String, String)>) -> Result<()> {
    let Some((prog, args)) = cmd.split_first() else {
        bail!("empty command")
    };
    let mut command = Command::new(program_in(dir, prog));
    command
        .args(args)
        .current_dir(dir)
        .envs(env)
        .env("MIRAKO_REMOTE", "1")
        .env("LANG", "C.UTF-8")
        .env("LC_CTYPE", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // a process group of its own, so that a hang-up stops what the command started as well
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    // a console of its own that shows no window, whatever the agent has
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let mut child = command.spawn().with_context(|| format!("starting `{prog}` in {}", dir.display()))?;
    // and on Windows a job, before the group: a hang-up seeing the one finds the other
    #[cfg(windows)]
    {
        *hangup.job.lock().unwrap() = job::Job::new(&child);
    }
    let group = child.id() as i32;
    hangup.group.store(group, Ordering::SeqCst);
    if hangup.gone.load(Ordering::SeqCst) {
        // the client left between its `Exec` and this spawn: the reader thread found nothing to stop
        let hangup = hangup.clone();
        thread::spawn(move || hangup.stop());
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let pump = |mut src: Box<dyn Read + Send>, is_err: bool, out: Out| {
        thread::spawn(move || {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match src.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if send(
                            &out,
                            &Resp::Output {
                                stderr: is_err,
                                data: buf[..n].to_vec(),
                            },
                        )
                        .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        })
    };
    let t1 = pump(Box::new(stdout), false, out.clone());
    let t2 = pump(Box::new(stderr), true, out.clone());
    let status = child.wait()?;
    if hangup.gone.load(Ordering::SeqCst) {
        // whatever of its group outlived the command
        hangup.signal(group, true);
    }
    // the group stays registered while the pumps run: what the command left behind holds their
    // pipes open, and a hang-up from here on has that to stop
    let _ = t1.join();
    let _ = t2.join();
    hangup.group.store(0, Ordering::SeqCst);
    #[cfg(windows)]
    {
        *hangup.job.lock().unwrap() = None;
    }
    send(
        out,
        &Resp::Exit {
            code: status.code().unwrap_or(-1),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Hands out 1, 2, 3, 1, 2, 3, … bytes per `read`, like a pipe under load.
    struct Trickle {
        data: Vec<u8>,
        pos: usize,
        step: usize,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.step = self.step % 3 + 1;
            let n = self.step.min(buf.len()).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn a_relative_program_is_found_under_the_dir_with_the_extension_windows_runs() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::write(d.join("gradlew"), "").unwrap();
        fs::write(d.join("gradlew.bat"), "").unwrap();
        fs::create_dir_all(d.join("tools")).unwrap();
        fs::write(d.join("tools/x.cmd"), "").unwrap();
        assert_eq!(PathBuf::from(windows_program(d, "./gradlew", None)), d.join("gradlew.bat"));
        assert_eq!(PathBuf::from(windows_program(d, "tools/x", None)), d.join("tools/x.cmd"));
        // an extension is kept, a file that is not there named as it is
        assert_eq!(PathBuf::from(windows_program(d, "./gradlew.bat", None)), d.join("gradlew.bat"));
        assert_eq!(PathBuf::from(windows_program(d, "./missing", None)), d.join("missing"));
        // on this side of Windows a relative program is left to the command's own directory
        if !cfg!(windows) {
            assert_eq!(program_in(d, "./gradlew"), OsString::from("./gradlew"));
        }
    }

    #[test]
    fn a_bare_program_is_the_bat_or_cmd_of_path_unless_an_exe_comes_first() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("gradle.bat"), "").unwrap();
        fs::write(a.join("java.exe"), "").unwrap();
        fs::write(b.join("java.cmd"), "").unwrap();
        let path = std::env::join_paths([&a, &b]).unwrap();
        let find = |p: &str| windows_program(dir.path(), p, Some(&path));
        assert_eq!(PathBuf::from(find("gradle")), b.join("gradle.bat"));
        assert_eq!(find("java"), OsString::from("java"));
        assert_eq!(find("echo"), OsString::from("echo"));
        assert_eq!(find("gradle.bat"), OsString::from("gradle.bat"));
        assert_eq!(windows_program(dir.path(), "gradle", None), OsString::from("gradle"));
    }

    #[test]
    fn expand_home_only_expands_a_tilde_slash_prefix() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home("~/x/y"), home.join("x/y"));
        assert_eq!(expand_home("/abs/p"), PathBuf::from("/abs/p"));
        assert_eq!(expand_home("rel/p"), PathBuf::from("rel/p"));
        // current behaviour: a bare `~` or `~user/` is left alone
        assert_eq!(expand_home("~"), PathBuf::from("~"));
        assert_eq!(expand_home("~other/x"), PathBuf::from("~other/x"));
    }

    #[test]
    fn read_full_fills_the_buffer_across_short_reads() {
        let data: Vec<u8> = (0..100u8).collect();
        let mut r = Trickle {
            data: data.clone(),
            pos: 0,
            step: 0,
        };
        let mut buf = [0u8; 40];
        assert_eq!(read_full(&mut r, &mut buf).unwrap(), 40);
        assert_eq!(&buf[..], &data[..40]);
        assert_eq!(read_full(&mut r, &mut buf).unwrap(), 40);
        assert_eq!(&buf[..], &data[40..80]);
        // EOF in the middle: a short count
        assert_eq!(read_full(&mut r, &mut buf).unwrap(), 20);
        assert_eq!(&buf[..20], &data[80..]);
        assert_eq!(read_full(&mut r, &mut buf).unwrap(), 0);
    }

    #[test]
    fn read_full_on_an_empty_reader_returns_zero() {
        let mut buf = [0u8; 8];
        assert_eq!(read_full(&mut Cursor::new(Vec::new()), &mut buf).unwrap(), 0);
        assert_eq!(read_full(&mut Cursor::new(vec![1, 2]), &mut []).unwrap(), 0);
    }
}
