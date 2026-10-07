//! Project scanning with a persistent hash cache: a file is re-hashed only when its size or
//! mtime changed, so a warm scan of a few thousand files takes milliseconds.

use crate::patterns::Matcher;
use crate::proto::{Entry, Kind};
use anyhow::{Context, Result};
use filetime::FileTime;
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
    /// It exists iff the root was synced, and its mtime is the last sync (see `save`): `gc`
    /// recognises the mirrors on the host by it.
    pub fn cache_path(root: &Path) -> PathBuf {
        let key = blake3::hash(root.to_string_lossy().as_bytes()).to_hex();
        dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("mirako")
            .join(format!("{}.idx", &key[..16]))
    }

    pub fn open(root: &Path) -> Self {
        let cache_file = Self::cache_path(root);
        let _ = fs::create_dir_all(cache_file.parent().unwrap());
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
            if ft.is_file() && is_leftover_tmp(&e.file_name().to_string_lossy()) {
                // a transfer the peer never finished (dropped connection); nothing is in flight
                // while a tree is scanned, so it is garbage on either side
                let _ = fs::remove_file(e.path());
                continue;
            }
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

    /// Writes the cache when it changed and leaves its mtime at "now" either way: on the agent
    /// that is the mirror's last-used time `gc` goes by.
    pub fn save(&mut self) {
        if !self.dirty && filetime::set_file_mtime(&self.cache_file, FileTime::now()).is_ok() {
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

/// `.mirako.<name>.tmp`, what `xfer::Inbox` writes before the rename into place.
fn is_leftover_tmp(name: &str) -> bool {
    name.starts_with(".mirako.") && name.ends_with(".tmp")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    /// Removes the index cache this test created under the real OS cache dir, even on panic.
    struct CacheGuard(PathBuf);

    impl Drop for CacheGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
            let _ = fs::remove_file(self.0.with_extension("tmp"));
        }
    }

    fn project() -> (CacheGuard, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        (CacheGuard(Index::cache_path(&root)), dir, root)
    }

    fn write(root: &Path, rel: &str, data: &[u8]) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    fn no_excludes() -> Matcher {
        Matcher::new(&[]).unwrap()
    }

    fn scan(root: &Path, exclude: &Matcher) -> Vec<Entry> {
        Index::open(root).scan(root, exclude).unwrap()
    }

    fn paths(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.path.as_str()).collect()
    }

    fn find<'a>(entries: &'a [Entry], path: &str) -> &'a Entry {
        entries
            .iter()
            .find(|e| e.path == path)
            .unwrap_or_else(|| panic!("{path} not in {:?}", paths(entries)))
    }

    fn bump_mtime(path: &Path, secs: i64) {
        let md = fs::metadata(path).unwrap();
        let t = FileTime::from_last_modification_time(&md);
        filetime::set_file_mtime(path, FileTime::from_unix_time(t.unix_seconds() + secs, t.nanoseconds())).unwrap();
    }

    #[test]
    fn scan_lists_files_sorted_with_slash_paths_size_hash_and_mode() {
        let (_g, _d, root) = project();
        write(&root, "b.txt", b"bee");
        write(&root, "a/z.txt", b"zed!");
        write(&root, "a/b/c.sh", b"#!/bin/sh\n");
        fs::set_permissions(root.join("a/b/c.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(root.join("b.txt"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::create_dir_all(root.join("empty/deeper")).unwrap();

        let entries = scan(&root, &no_excludes());
        assert_eq!(paths(&entries), ["a/b/c.sh", "a/z.txt", "b.txt"]);
        for e in &entries {
            let data = fs::read(root.join(&e.path)).unwrap();
            assert_eq!(e.kind, Kind::File);
            assert_eq!(e.size, data.len() as u64);
            assert_eq!(e.hash, *blake3::hash(&data).as_bytes());
            assert_eq!(e.mtime_ns, mtime_ns(&fs::metadata(root.join(&e.path)).unwrap()));
        }
        assert_eq!(find(&entries, "a/b/c.sh").mode & 0o7777, 0o755);
        assert_eq!(find(&entries, "b.txt").mode & 0o7777, 0o644);
    }

    #[test]
    fn symlinks_are_entries_with_the_hash_of_their_target_even_when_dangling() {
        let (_g, _d, root) = project();
        write(&root, "real.txt", b"content");
        symlink("real.txt", root.join("link")).unwrap();
        symlink("nowhere/at/all", root.join("dangling")).unwrap();

        let entries = scan(&root, &no_excludes());
        assert_eq!(paths(&entries), ["dangling", "link", "real.txt"]);
        for (path, target) in [("link", "real.txt"), ("dangling", "nowhere/at/all")] {
            let e = find(&entries, path);
            assert_eq!(e.kind, Kind::Symlink { target: target.into() });
            assert_eq!(e.size, 0);
            assert_eq!(e.hash, *blake3::hash(target.as_bytes()).as_bytes());
        }
    }

    #[test]
    fn symlink_to_a_directory_is_not_followed() {
        let (_g, _d, root) = project();
        write(&root, "real/inner.txt", b"x");
        symlink("real", root.join("link")).unwrap();

        let entries = scan(&root, &no_excludes());
        assert_eq!(paths(&entries), ["link", "real/inner.txt"]);
        assert_eq!(find(&entries, "link").kind, Kind::Symlink { target: "real".into() });
    }

    #[test]
    fn excludes_skip_subtrees_but_keep_includes_inside_them() {
        let (_g, _d, root) = project();
        write(&root, "app/build/x", b"1");
        write(&root, "app/build/deep/y", b"2");
        write(&root, "app/build/keep/y", b"3");
        write(&root, "app/src/a", b"4");
        write(&root, "build", b"5");
        let m = Matcher::new(&["build".into(), "!build/keep".into()]).unwrap();

        let entries = scan(&root, &m);
        assert_eq!(paths(&entries), ["app/build/keep/y", "app/src/a"]);
    }

    #[test]
    fn leftover_transfer_temp_files_are_deleted_and_not_listed() {
        let (_g, _d, root) = project();
        write(&root, ".mirako.foo.tmp", b"half");
        write(&root, "sub/.mirako.bar.apk.tmp", b"half");
        write(&root, ".mirako.notmp", b"keep");
        write(&root, "foo.tmp", b"keep");

        let entries = scan(&root, &no_excludes());
        assert_eq!(paths(&entries), [".mirako.notmp", "foo.tmp"]);
        assert!(!root.join(".mirako.foo.tmp").exists());
        assert!(!root.join("sub/.mirako.bar.apk.tmp").exists());
        assert!(root.join(".mirako.notmp").exists());
        assert!(root.join("foo.tmp").exists());
    }

    #[test]
    fn a_reopened_cache_serves_unchanged_files_and_rehashes_when_mtime_moves() {
        let (_g, _d, root) = project();
        write(&root, "a.txt", b"aaaa");
        write(&root, "b/c.txt", b"cccc");

        let mut first = Index::open(&root);
        let before = first.scan(&root, &no_excludes()).unwrap();
        assert!(first.dirty);
        first.save();
        assert!(!first.dirty);

        let mut second = Index::open(&root);
        assert_eq!(second.cache.files.len(), 2);
        let again = second.scan(&root, &no_excludes()).unwrap();
        assert!(!second.dirty, "everything should come from the cache");
        assert_eq!(paths(&again), paths(&before));
        for (a, b) in again.iter().zip(&before) {
            assert_eq!((a.size, a.mtime_ns, a.hash), (b.size, b.mtime_ns, b.hash));
        }

        // same size, different bytes, mtime moved: re-hashed
        fs::write(root.join("a.txt"), b"AAAA").unwrap();
        bump_mtime(&root.join("a.txt"), 1);
        let changed = second.scan(&root, &no_excludes()).unwrap();
        assert!(second.dirty);
        assert_eq!(find(&changed, "a.txt").hash, *blake3::hash(b"AAAA").as_bytes());
        assert_eq!(find(&changed, "b/c.txt").hash, *blake3::hash(b"cccc").as_bytes());
    }

    #[test]
    fn same_size_and_mtime_is_served_from_the_cache_even_if_the_content_changed() {
        // Known limitation of the (size, mtime) key: a rewrite that restores the mtime is invisible.
        let (_g, _d, root) = project();
        let p = root.join("a.txt");
        write(&root, "a.txt", b"old!");
        let old_mtime = FileTime::from_last_modification_time(&fs::metadata(&p).unwrap());

        let mut index = Index::open(&root);
        index.scan(&root, &no_excludes()).unwrap();
        fs::write(&p, b"new!").unwrap();
        filetime::set_file_mtime(&p, old_mtime).unwrap();

        let entries = index.scan(&root, &no_excludes()).unwrap();
        assert_eq!(find(&entries, "a.txt").hash, *blake3::hash(b"old!").as_bytes());
    }

    #[test]
    fn remember_makes_the_next_scan_use_the_given_hash_without_reading_the_file() {
        let (_g, _d, root) = project();
        write(&root, "a.bin", b"real bytes");
        let md = fs::metadata(root.join("a.bin")).unwrap();
        let fake = [7u8; 32];

        let mut index = Index::open(&root);
        index.remember("a.bin", md.len(), mtime_ns(&md), fake);
        assert!(index.dirty);
        let entries = index.scan(&root, &no_excludes()).unwrap();
        assert_eq!(find(&entries, "a.bin").hash, fake);
    }

    #[test]
    fn save_when_clean_only_touches_the_cache_file() {
        let (g, _d, root) = project();
        write(&root, "a.txt", b"a");
        let mut index = Index::open(&root);
        index.scan(&root, &no_excludes()).unwrap();
        index.save();
        let bytes = fs::read(&g.0).unwrap();
        let mtime = fs::metadata(&g.0).unwrap().modified().unwrap();

        std::thread::sleep(Duration::from_millis(20));
        index.save();
        assert!(fs::metadata(&g.0).unwrap().modified().unwrap() > mtime);
        assert_eq!(fs::read(&g.0).unwrap(), bytes);
    }

    #[test]
    fn save_when_dirty_writes_the_cache_atomically() {
        let (g, _d, root) = project();
        write(&root, "a.txt", b"a");
        let mut index = Index::open(&root);
        index.scan(&root, &no_excludes()).unwrap();
        index.save();
        assert!(g.0.exists());
        assert!(!g.0.with_extension("tmp").exists());

        index.remember("b.txt", 1, 2, [3; 32]);
        index.save();
        assert!(!g.0.with_extension("tmp").exists());
        let cache: Cache = bincode::deserialize(&fs::read(&g.0).unwrap()).unwrap();
        assert_eq!(cache.files.get("b.txt"), Some(&(1, 2, [3; 32])));
        assert!(cache.files.contains_key("a.txt"));
    }

    #[test]
    fn rel_path_is_slash_separated_and_relative_to_root() {
        let root = Path::new("/some/root");
        assert_eq!(rel_path(root, &root.join("a/b/c")), "a/b/c");
        assert_eq!(rel_path(root, root), "");
        assert_eq!(rel_path(root, Path::new("other/x")), "other/x");
        // outside the root the path is returned whole; the root component joins as an extra `/`
        assert_eq!(rel_path(root, Path::new("/elsewhere/x")), "//elsewhere/x");
    }

    #[test]
    fn mtime_ns_and_hash_file_match_std_and_blake3() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        fs::write(&p, b"hello mirako").unwrap();
        let md = fs::metadata(&p).unwrap();
        let expected = md.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_nanos() as i64;
        assert_eq!(mtime_ns(&md), expected);
        assert_eq!(hash_file(&p).unwrap(), *blake3::hash(b"hello mirako").as_bytes());
        assert!(hash_file(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn an_unreadable_directory_is_skipped_not_an_error() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root reads everything
        }
        let (_g, _d, root) = project();
        write(&root, "ok.txt", b"ok");
        write(&root, "locked/secret.txt", b"s");
        let locked = root.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let result = Index::open(&root).scan(&root, &no_excludes());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(paths(&result.unwrap()), ["ok.txt"]);
    }
}
