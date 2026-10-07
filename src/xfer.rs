//! Receiving side of file transfers, shared by the client (pull) and the agent (push):
//! whole-file chunks and delta chunks land in a temp file next to the destination and are
//! renamed into place when complete, so a broken connection never leaves a half-written file.
//! Nothing is fsync'ed: that costs ~4 ms per file on macOS and the next sync repairs whatever a
//! power cut might lose.

use crate::delta;
use crate::proto::{self, Chunk, DeltaChunk};
use anyhow::{bail, Context, Result};
use filetime::FileTime;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|c| c == "..") {
        bail!("refusing path `{rel}`");
    }
    Ok(root.join(rel))
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

#[derive(Default)]
pub struct Inbox {
    open: HashMap<String, Inbound>,
}

impl Inbox {
    fn start(&mut self, root: &Path, rel: &str, with_old: bool) -> Result<()> {
        let dest = safe_join(root, rel)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_file_name(format!(".mirako.{}.tmp", dest.file_name().unwrap_or_default().to_string_lossy()));
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
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        let _ = fs::remove_file(&dest);
        fs::rename(&tmp, &dest)?;
        let ft = FileTime::from_unix_time(mtime_ns.div_euclid(1_000_000_000), mtime_ns.rem_euclid(1_000_000_000) as u32);
        filetime::set_file_mtime(&dest, ft)?;
        Ok(Finished {
            path: rel.into(),
            size,
            mtime_ns,
            hash,
            ok: true,
        })
    }

    pub fn put(&mut self, root: &Path, c: &Chunk) -> Result<Option<Finished>> {
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
        let p = safe_join(root, rel)?;
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        let _ = fs::remove_file(&p);
        std::os::unix::fs::symlink(target, &p)?;
        Ok(())
    }
}
