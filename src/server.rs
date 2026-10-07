//! `mirako serve`: the agent the client starts over ssh. Speaks the protocol on stdin/stdout,
//! logs to stderr only.

use crate::delta;
use crate::index::{self, Index};
use crate::patterns::Matcher;
use crate::proto::{self, read_frame, write_frame, Chunk, DeltaChunk, Req, Resp, CHUNK, ZSTD_LEVEL};
use crate::xfer::{safe_join, Inbox};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

type Out = Arc<Mutex<BufWriter<io::Stdout>>>;

fn send(out: &Out, resp: &Resp) -> Result<()> {
    let mut w = out.lock().unwrap();
    write_frame(&mut *w, resp)?;
    w.flush()?;
    Ok(())
}

pub fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

pub fn serve() -> Result<()> {
    let mut input = BufReader::new(io::stdin());
    let out: Out = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let mut inbox = Inbox::default();
    let mut index: Option<(PathBuf, Index)> = None;
    let mut failed_deltas: Vec<String> = Vec::new();

    loop {
        let req: Req = match read_frame(&mut input) {
            Ok(r) => r,
            Err(_) => return Ok(()), // client went away
        };
        let result: Result<()> = (|| match req {
            Req::Hello { version } => {
                if version != proto::VERSION {
                    eprintln!("mirako serve {}: client is {version}", proto::VERSION);
                }
                send(
                    &out,
                    &Resp::Hello {
                        version: proto::VERSION.into(),
                        home: dirs::home_dir().unwrap_or_default().to_string_lossy().into_owned(),
                        os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
                    },
                )
            }
            Req::Manifest { dir, exclude } => {
                let root = expand_home(&dir);
                fs::create_dir_all(&root)?;
                let root = root.canonicalize()?;
                let matcher = Matcher::new(&exclude)?;
                if index.as_ref().map(|(r, _)| r != &root).unwrap_or(true) {
                    index = Some((root.clone(), Index::open(&root)));
                }
                let idx = &mut index.as_mut().unwrap().1;
                let entries = idx.scan(&root, &matcher)?;
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
                if let Some(f) = inbox.put(root, &chunk)? {
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
                inbox.symlink(root, &path, &target)
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
                let root = expand_home(&dir).canonicalize()?;
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
                send(&out, &Resp::Ack { failed })
            }
            Req::Exec { dir, cmd } => exec(&out, &expand_home(&dir), &cmd),
            Req::Fetch { dir, paths, sigs } => {
                let root = expand_home(&dir).canonicalize()?;
                let sigs: HashMap<String, _> = sigs.into_iter().collect();
                for p in paths {
                    let full = safe_join(&root, &p)?;
                    let md = fs::symlink_metadata(&full)?;
                    if md.file_type().is_symlink() {
                        let target = fs::read_link(&full)?.to_string_lossy().into_owned();
                        send(&out, &Resp::Symlink { path: p, target })?;
                    } else if let Some(sig) = sigs.get(&p).filter(|_| delta::delta_worthwhile(md.len())) {
                        send_delta(&out, &full, &p, &md, sig)?;
                    } else {
                        send_file(&out, &full, &p, &md)?;
                    }
                }
                send(&out, &Resp::End)
            }
            Req::Bye => std::process::exit(0),
        })();
        if let Err(e) = result {
            eprintln!("mirako serve: {e:#}");
            send(&out, &Resp::Error { msg: format!("{e:#}") })?;
        }
    }
}

/// Stream one file as `Resp::Put` chunks.
fn send_file(out: &Out, full: &Path, rel: &str, md: &fs::Metadata) -> Result<()> {
    let mut f = fs::File::open(full)?;
    let size = md.len();
    let mode = md.permissions().mode() & 0o7777;
    let mtime_ns = index::mtime_ns(md);
    let mut offset = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = read_full(&mut f, &mut buf)?;
        let last = offset + n as u64 >= size || n == 0;
        let data = zstd::encode_all(&buf[..n], ZSTD_LEVEL)?;
        send(
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

/// Stream one file as `Resp::Delta` chunks against the client's signature.
fn send_delta(out: &Out, full: &Path, rel: &str, md: &fs::Metadata, sig: &proto::Signature) -> Result<()> {
    let data = fs::read(full)?;
    let hash = *blake3::hash(&data).as_bytes();
    let mode = md.permissions().mode() & 0o7777;
    let mtime_ns = index::mtime_ns(md);
    let size = data.len() as u64;
    let mut ops = Vec::new();
    let mut pending = 0usize;
    delta::delta(&data, sig, |op| {
        pending += match &op {
            proto::Op::Data(d) => d.len(),
            _ => 8,
        };
        ops.push(op);
        if pending >= CHUNK {
            let batch = std::mem::take(&mut ops);
            pending = 0;
            send(
                out,
                &Resp::Delta(DeltaChunk {
                    path: rel.into(),
                    mode,
                    mtime_ns,
                    size,
                    hash,
                    ops: batch,
                    last: false,
                }),
            )?;
        }
        Ok(())
    })?;
    send(
        out,
        &Resp::Delta(DeltaChunk {
            path: rel.into(),
            mode,
            mtime_ns,
            size,
            hash,
            ops,
            last: true,
        }),
    )
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

fn exec(out: &Out, dir: &Path, cmd: &[String]) -> Result<()> {
    let Some((prog, args)) = cmd.split_first() else {
        bail!("empty command")
    };
    let mut child = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .env("MIRAKO_REMOTE", "1")
        .env("LANG", "C.UTF-8")
        .env("LC_CTYPE", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting `{prog}` in {}", dir.display()))?;
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
    let _ = t1.join();
    let _ = t2.join();
    send(
        out,
        &Resp::Exit {
            code: status.code().unwrap_or(-1),
        },
    )
}
