//! Housekeeping on the build host, run by the agent: project mirrors under `remote_folder`
//! that have not been synced for a while, build intermediates the client does not keep, and
//! Gradle's own cache retention. A mistake here costs one full re-upload, never local data.

use crate::index::{self, Index};
use crate::patterns::Matcher;
use crate::proto::{GcReport, GcReq, Mirror};
use crate::server::expand_home;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

const DAY: u64 = 86_400;

pub fn collect(req: &GcReq) -> Result<GcReport> {
    let folder = expand_home(&req.folder);
    let current = req.current.as_deref().map(expand_home).and_then(|p| p.canonicalize().ok());
    let mut report = GcReport::default();

    let mut dirs: Vec<PathBuf> = match fs::read_dir(&folder) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            // `file_type` does not follow symlinks: a link to a directory is never removed through
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .collect(),
        Err(_) => Vec::new(),
    };
    dirs.sort();
    for path in dirs {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        // the agent's index cache for a root exists iff mirako has synced it, and its mtime is
        // the last sync (`Index::save` touches it even when nothing changed)
        let last = path
            .canonicalize()
            .ok()
            .map(|canon| (Index::cache_path(&canon), canon))
            .and_then(|(idx, canon)| fs::metadata(&idx).and_then(|m| m.modified()).ok().map(|t| (idx, canon, t)));
        let Some((idx, canon, last)) = last else {
            report.mirrors.push(Mirror {
                name,
                bytes: 0,
                idle_days: None,
                removed: false,
            });
            continue;
        };
        let idle = SystemTime::now().duration_since(last).unwrap_or_default().as_secs();
        let stale = current.as_ref() != Some(&canon) && req.keep_days.is_some_and(|d| idle > u64::from(d) * DAY);
        let bytes = if stale || req.sizes { dir_size(&path) } else { 0 };
        let mut removed = stale;
        if stale && !req.dry_run {
            match fs::remove_dir_all(&path) {
                Ok(()) => {
                    let _ = fs::remove_file(&idx);
                }
                Err(e) => {
                    eprintln!("mirako serve: gc: removing {}: {e}", path.display());
                    removed = false;
                }
            }
        }
        report.mirrors.push(Mirror {
            name,
            bytes,
            idle_days: Some((idle / DAY) as u32),
            removed,
        });
    }

    if let Some(cur) = current.filter(|_| !req.build.is_empty()) {
        report.build_bytes = remove_matching(&cur, &req.build, req.dry_run)?;
    }
    if !req.dry_run {
        gradle_retention(req.gradle_days)?;
    }
    report.free = free_space(&folder);
    Ok(report)
}

fn dir_size(dir: &Path) -> u64 {
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Deletes what `patterns` match under `root` with the sync's exclude semantics (a `!include`
/// inside a removed directory survives); returns the bytes they held.
fn remove_matching(root: &Path, patterns: &[String], dry_run: bool) -> Result<u64> {
    let matcher = Matcher::new(patterns)?;
    let mut bytes = 0;
    let mut walk = WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = walk.next() {
        let Ok(e) = entry else { continue };
        if e.depth() == 0 {
            continue;
        }
        let rel = index::rel_path(root, e.path());
        if !matcher.excluded(&rel) {
            continue;
        }
        let dir = e.file_type().is_dir();
        if dir && !matcher.skip_subtree(&rel) {
            continue; // something under it is included: decide its children one by one
        }
        if dir {
            walk.skip_current_dir();
            bytes += dir_size(e.path());
        } else {
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
        if dry_run {
            continue;
        }
        let r = if dir {
            fs::remove_dir_all(e.path())
        } else {
            fs::remove_file(e.path())
        };
        if let Err(err) = r {
            eprintln!("mirako serve: gc: removing {}: {err}", e.path().display());
        }
    }
    Ok(bytes)
}

const GRADLE_INIT: &str = r#"// ~/.gradle/init.d/mirako-gc.gradle — written by mirako (`gc_days = __DAYS__` on the client).
// Gradle removes cache entries and wrapper distributions unused for this long (its defaults
// are 30 days, 7 for snapshots). Delete this file or set `gc_days = 0` to get them back.
import org.gradle.util.GradleVersion

if (GradleVersion.current() >= GradleVersion.version("8.0")) {
    beforeSettings { settings ->
        settings.caches {
            releasedWrappers.setRemoveUnusedEntriesAfterDays(__DAYS__)
            snapshotWrappers.setRemoveUnusedEntriesAfterDays(__DAYS__)
            downloadedResources.setRemoveUnusedEntriesAfterDays(__DAYS__)
            createdResources.setRemoveUnusedEntriesAfterDays(__DAYS__)
            buildCache.setRemoveUnusedEntriesAfterDays(__DAYS__)
        }
    }
}
"#;

/// Gradle cleans `~/.gradle/caches` and `wrapper/dists` itself, in the background after a build
/// and at most daily; this only sets how long an unused entry survives. Honours `GRADLE_USER_HOME`.
fn gradle_retention(days: u32) -> Result<()> {
    let home = match std::env::var_os("GRADLE_USER_HOME") {
        Some(h) => PathBuf::from(h),
        None => match dirs::home_dir() {
            Some(h) => h.join(".gradle"),
            None => return Ok(()),
        },
    };
    if !home.is_dir() {
        return Ok(()); // no Gradle on this host
    }
    let path = home.join("init.d").join("mirako-gc.gradle");
    if days == 0 {
        if path.exists() {
            fs::remove_file(&path)?;
        }
        return Ok(());
    }
    let script = GRADLE_INIT.replace("__DAYS__", &days.to_string());
    if fs::read_to_string(&path).ok().as_deref() != Some(script.as_str()) {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, script).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Free bytes on the volume holding `p` (0 when unknown).
#[allow(clippy::unnecessary_cast)] // the field types differ between macOS and Linux
fn free_space(p: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else {
        return 0;
    };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a NUL-terminated path and `st` a correctly sized out-parameter
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return 0;
    }
    st.f_bavail as u64 * st.f_frsize as u64
}
