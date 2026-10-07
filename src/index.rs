//! Project scanning with a persistent hash cache: a file is re-hashed only when its size or
//! mtime changed, so a warm scan of a few thousand files takes milliseconds.

use crate::patterns::Matcher;
use crate::proto::{Entry, Kind};
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    // path -> (size, mtime_ns, hash)
    files: HashMap<String, (u64, i64, [u8; 32])>,
}

pub struct Index {
    cache_file: PathBuf,
    cache: Cache,
    dirty: bool,
}

impl Index {
    /// One cache per project root, under the OS cache dir (`~/Library/Caches/mirako` on macOS).
    pub fn open(root: &Path) -> Self {
        let dir = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("mirako");
        let _ = fs::create_dir_all(&dir);
        let key = blake3::hash(root.to_string_lossy().as_bytes()).to_hex();
        let cache_file = dir.join(format!("{}.idx", &key[..16]));
        let cache = fs::read(&cache_file)
            .ok()
            .and_then(|b| bincode::deserialize(&b).ok())
            .unwrap_or_default();
        Self {
            cache_file,
            cache,
            dirty: false,
        }
    }

    /// Scan `root`, skipping what `exclude` matches. Hashes in parallel, using the cache.
    pub fn scan(&mut self, root: &Path, exclude: &Matcher) -> Result<Vec<Entry>> {
        let mut files: Vec<(String, fs::Metadata)> = Vec::new();
        let mut links: Vec<Entry> = Vec::new();

        let walker = WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let rel = rel_path(root, e.path());
            !exclude.skip_subtree(&rel)
        });
        for e in walker {
            let e = match e {
                Ok(e) => e,
                Err(err) => {
                    eprintln!("mirako: skipping unreadable entry: {err}");
                    continue;
                }
            };
            if e.depth() == 0 {
                continue;
            }
            let rel = rel_path(root, e.path());
            if exclude.excluded(&rel) {
                continue; // an excluded parent on the way to a `!include`
            }
            let ft = e.file_type();
            if ft.is_symlink() {
                let target = fs::read_link(e.path())?.to_string_lossy().into_owned();
                let md = e.path().symlink_metadata()?;
                links.push(Entry {
                    path: rel,
                    kind: Kind::Symlink { target: target.clone() },
                    size: 0,
                    mtime_ns: mtime_ns(&md),
                    mode: md.permissions().mode() & 0o7777,
                    hash: *blake3::hash(target.as_bytes()).as_bytes(),
                });
            } else if ft.is_file() {
                files.push((rel, e.metadata()?));
            }
        }

        // split into cached / to-hash
        let mut entries: Vec<Entry> = Vec::with_capacity(files.len() + links.len());
        let mut to_hash: Vec<(String, u64, i64, u32)> = Vec::new();
        for (rel, md) in files {
            let size = md.len();
            let mt = mtime_ns(&md);
            let mode = md.permissions().mode() & 0o7777;
            match self.cache.files.get(&rel) {
                Some(&(csize, cmt, hash)) if csize == size && cmt == mt => {
                    entries.push(Entry {
                        path: rel,
                        kind: Kind::File,
                        size,
                        mtime_ns: mt,
                        mode,
                        hash,
                    });
                }
                _ => to_hash.push((rel, size, mt, mode)),
            }
        }

        let hashed: Vec<Result<Entry>> = to_hash
            .into_par_iter()
            .map(|(rel, size, mt, mode)| {
                let hash = hash_file(&root.join(&rel)).with_context(|| format!("hashing {rel}"))?;
                Ok(Entry {
                    path: rel,
                    kind: Kind::File,
                    size,
                    mtime_ns: mt,
                    mode,
                    hash,
                })
            })
            .collect();
        for h in hashed {
            let e = h?;
            self.cache.files.insert(e.path.clone(), (e.size, e.mtime_ns, e.hash));
            self.dirty = true;
            entries.push(e);
        }
        entries.extend(links);
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    /// Record a file we just wrote so the next scan does not re-hash it.
    pub fn remember(&mut self, rel: &str, size: u64, mtime_ns: i64, hash: [u8; 32]) {
        self.cache.files.insert(rel.to_string(), (size, mtime_ns, hash));
        self.dirty = true;
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        if let Ok(bytes) = bincode::serialize(&self.cache) {
            let tmp = self.cache_file.with_extension("tmp");
            if fs::write(&tmp, bytes).is_ok() {
                let _ = fs::rename(&tmp, &self.cache_file);
            }
        }
        self.dirty = false;
    }
}

pub fn rel_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn mtime_ns(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or_else(|| md.mtime() * 1_000_000_000)
}

pub fn hash_file(path: &Path) -> Result<[u8; 32]> {
    let mut h = blake3::Hasher::new();
    h.update_reader(fs::File::open(path)?)?;
    Ok(*h.finalize().as_bytes())
}
