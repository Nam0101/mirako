//! Housekeeping on the build host, run by the agent: project mirrors under `remote_folder`
//! that have not been synced for a while, build intermediates the client does not keep, and
//! Gradle's own cache retention. A mistake here costs one full re-upload, never local data.

use crate::index::{self, Index};
use crate::patterns::Matcher;
use crate::proto::{GcReport, GcReq, Mirror};
use crate::server::expand_home;
use crate::xfer::canonical;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

const DAY: u64 = 86_400;

pub fn collect(req: &GcReq) -> Result<GcReport> {
    let folder = expand_home(&req.folder);
    let current = req.current.as_deref().map(expand_home).and_then(|p| canonical(&p).ok());
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
        let last = canonical(&path)
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
        if let Some(home) = gradle_home() {
            gradle_retention(&home, req.gradle_days)?;
        }
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

/// `$GRADLE_USER_HOME`, else `~/.gradle`.
fn gradle_home() -> Option<PathBuf> {
    match std::env::var_os("GRADLE_USER_HOME") {
        Some(h) => Some(PathBuf::from(h)),
        None => dirs::home_dir().map(|h| h.join(".gradle")),
    }
}

/// Gradle cleans `~/.gradle/caches` and `wrapper/dists` itself, in the background after a build
/// and at most daily; this only sets how long an unused entry survives.
pub fn gradle_retention(home: &Path, days: u32) -> Result<()> {
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
#[cfg(unix)]
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

/// Unknown on Windows, which is a client: the builds, and so this housekeeping, run on a Unix host.
#[cfg(not(unix))]
fn free_space(_: &Path) -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::FileTime;
    use std::sync::OnceLock;
    use std::time::Duration;
    use tempfile::TempDir;

    /// `collect` with `dry_run: false` writes Gradle's retention script into `gradle_home()`:
    /// point it at one shared scratch dir (env vars are process-wide, tests run in parallel).
    fn isolate_gradle() {
        static GRADLE: OnceLock<TempDir> = OnceLock::new();
        let dir = GRADLE.get_or_init(|| tempfile::tempdir().unwrap());
        std::env::set_var("GRADLE_USER_HOME", dir.path());
    }

    fn write(path: &Path, len: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; len]).unwrap();
    }

    fn canon_tempdir() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canon = canonical(dir.path()).unwrap();
        (dir, canon)
    }

    fn req(folder: &Path) -> GcReq {
        GcReq {
            folder: folder.to_string_lossy().into_owned(),
            keep_days: None,
            current: None,
            build: Vec::new(),
            gradle_days: 0,
            dry_run: true,
            sizes: false,
        }
    }

    /// Marks `dir` as synced (an index cache in the real OS cache dir) and removes that cache on drop.
    struct Synced {
        idx: PathBuf,
    }

    impl Synced {
        fn new(dir: &Path, idle_days: u64) -> Self {
            let canon = canonical(dir).unwrap();
            Index::open(&canon).save();
            let idx = Index::cache_path(&canon);
            assert!(idx.exists());
            if idle_days > 0 {
                let then = SystemTime::now() - Duration::from_secs(idle_days * DAY);
                filetime::set_file_mtime(&idx, FileTime::from_system_time(then)).unwrap();
            }
            Self { idx }
        }
    }

    impl Drop for Synced {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.idx);
        }
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    #[cfg(unix)]
    fn dir_size_sums_files_recursively_without_following_symlinks() {
        let (_d, t) = canon_tempdir();
        write(&t.join("d/a"), 10);
        write(&t.join("d/sub/b"), 20);
        write(&t.join("d/sub/deeper/c"), 30);
        write(&t.join("big"), 1000);
        std::os::unix::fs::symlink(t.join("big"), t.join("d/link")).unwrap();
        std::os::unix::fs::symlink(&t, t.join("d/sub/loop")).unwrap();
        assert_eq!(dir_size(&t.join("d")), 60);
        assert_eq!(dir_size(&t.join("missing")), 0);
    }

    /// app/build/intermediates/{foo/a 100, foo/b 50, keep/x 7, keep/note.log 3}, app/build/outputs/y 11,
    /// app/src/z 13, app/src/debug.log 5, top.log 2
    fn android_layout(root: &Path) {
        write(&root.join("app/build/intermediates/foo/a"), 100);
        write(&root.join("app/build/intermediates/foo/b"), 50);
        write(&root.join("app/build/intermediates/keep/x"), 7);
        write(&root.join("app/build/intermediates/keep/note.log"), 3);
        write(&root.join("app/build/outputs/y"), 11);
        write(&root.join("app/src/z"), 13);
        write(&root.join("app/src/debug.log"), 5);
        write(&root.join("top.log"), 2);
    }

    const LAYOUT_REMOVED: u64 = 100 + 50 + 5 + 2;

    fn layout_patterns() -> Vec<String> {
        strings(&["build/intermediates", "!build/intermediates/keep", "*.log"])
    }

    #[test]
    fn remove_matching_dry_run_counts_the_bytes_and_deletes_nothing() {
        let (_d, t) = canon_tempdir();
        android_layout(&t);
        assert_eq!(remove_matching(&t, &layout_patterns(), true).unwrap(), LAYOUT_REMOVED);
        assert!(t.join("app/build/intermediates/foo/a").exists());
        assert!(t.join("app/src/debug.log").exists());
        assert!(t.join("top.log").exists());
    }

    #[test]
    fn remove_matching_deletes_what_matches_and_keeps_includes() {
        let (_d, t) = canon_tempdir();
        android_layout(&t);
        assert_eq!(remove_matching(&t, &layout_patterns(), false).unwrap(), LAYOUT_REMOVED);
        assert!(!t.join("app/build/intermediates/foo").exists());
        assert!(!t.join("app/src/debug.log").exists());
        assert!(!t.join("top.log").exists());
        assert!(t.join("app/build/intermediates/keep/x").exists());
        assert!(t.join("app/build/intermediates/keep/note.log").exists());
        assert!(t.join("app/build/outputs/y").exists());
        assert!(t.join("app/src/z").exists());
    }

    #[test]
    fn remove_matching_with_no_patterns_removes_nothing() {
        let (_d, t) = canon_tempdir();
        android_layout(&t);
        assert_eq!(remove_matching(&t, &[], false).unwrap(), 0);
        assert_eq!(dir_size(&t), LAYOUT_REMOVED + 7 + 3 + 11 + 13);
    }

    #[test]
    fn remove_matching_removes_a_matched_directory_as_a_whole_and_counts_it_once() {
        let (_d, t) = canon_tempdir();
        write(&t.join("app/build/a"), 10);
        write(&t.join("app/build/b/c"), 20);
        write(&t.join("app/src/d"), 1);
        assert_eq!(remove_matching(&t, &strings(&["build"]), false).unwrap(), 30);
        assert!(!t.join("app/build").exists());
        assert!(t.join("app/src/d").exists());
    }

    #[test]
    fn remove_matching_rejects_a_bad_pattern() {
        let (_d, t) = canon_tempdir();
        assert!(remove_matching(&t, &strings(&["a[b"]), true).is_err());
    }

    fn retention_script(home: &Path) -> PathBuf {
        home.join("init.d/mirako-gc.gradle")
    }

    #[test]
    fn gradle_retention_does_nothing_without_a_gradle_home() {
        let (_d, t) = canon_tempdir();
        let home = t.join("gradle");
        gradle_retention(&home, 7).unwrap();
        gradle_retention(&home, 0).unwrap();
        assert!(!home.exists());
    }

    #[test]
    fn gradle_retention_writes_rewrites_and_removes_the_init_script() {
        let (_d, home) = canon_tempdir();
        let script = retention_script(&home);

        gradle_retention(&home, 7).unwrap();
        let text = fs::read_to_string(&script).unwrap();
        assert!(text.contains("setRemoveUnusedEntriesAfterDays(7)"));
        assert!(!text.contains("__DAYS__"));
        let mtime = fs::metadata(&script).unwrap().modified().unwrap();

        std::thread::sleep(Duration::from_millis(20));
        gradle_retention(&home, 7).unwrap();
        assert_eq!(
            fs::metadata(&script).unwrap().modified().unwrap(),
            mtime,
            "unchanged script rewritten"
        );

        gradle_retention(&home, 3).unwrap();
        let text = fs::read_to_string(&script).unwrap();
        assert!(text.contains("setRemoveUnusedEntriesAfterDays(3)"));
        assert!(!text.contains("setRemoveUnusedEntriesAfterDays(7)"));

        gradle_retention(&home, 0).unwrap();
        assert!(!script.exists());
        gradle_retention(&home, 0).unwrap();
    }

    #[test]
    fn the_gradle_init_template_has_a_days_placeholder() {
        assert!(GRADLE_INIT.contains("__DAYS__"));
    }

    #[test]
    #[cfg(unix)]
    fn free_space_is_positive_for_a_real_dir_and_zero_for_a_missing_one() {
        let (_d, t) = canon_tempdir();
        assert!(free_space(&t) > 0);
        assert_eq!(free_space(&t.join("missing")), 0);
    }

    #[test]
    fn collect_on_a_missing_folder_reports_no_mirrors() {
        let (_d, t) = canon_tempdir();
        let r = collect(&req(&t.join("missing"))).unwrap();
        assert!(r.mirrors.is_empty());
        assert_eq!(r.build_bytes, 0);
        assert_eq!(r.free, 0);
    }

    #[test]
    fn a_never_synced_dir_is_listed_unmeasured_and_kept() {
        let (_d, t) = canon_tempdir();
        write(&t.join("other/f"), 100);
        let r = collect(&GcReq {
            keep_days: Some(0),
            sizes: true,
            ..req(&t)
        })
        .unwrap();
        assert_eq!(r.mirrors.len(), 1);
        let m = &r.mirrors[0];
        assert_eq!(m.name, "other");
        assert_eq!(m.idle_days, None);
        assert!(!m.removed);
        // current behaviour: unsynced dirs are never measured, even with `sizes`
        assert_eq!(m.bytes, 0);
    }

    #[test]
    fn a_freshly_synced_mirror_is_kept_and_measured_only_on_request() {
        let (_d, t) = canon_tempdir();
        write(&t.join("p/f"), 42);
        let _s = Synced::new(&t.join("p"), 0);
        let r = collect(&GcReq {
            keep_days: Some(7),
            ..req(&t)
        })
        .unwrap();
        let m = &r.mirrors[0];
        assert_eq!((m.idle_days, m.removed, m.bytes), (Some(0), false, 0));
        let r = collect(&GcReq {
            keep_days: Some(7),
            sizes: true,
            ..req(&t)
        })
        .unwrap();
        let m = &r.mirrors[0];
        assert_eq!((m.idle_days, m.removed, m.bytes), (Some(0), false, 42));
        #[cfg(unix)]
        assert!(r.free > 0);
    }

    #[test]
    fn a_stale_mirror_dry_run_is_reported_and_measured_but_left_in_place() {
        let (_d, t) = canon_tempdir();
        write(&t.join("p/f"), 42);
        let s = Synced::new(&t.join("p"), 10);
        let r = collect(&GcReq {
            keep_days: Some(7),
            ..req(&t)
        })
        .unwrap();
        let m = &r.mirrors[0];
        assert_eq!((m.idle_days, m.removed, m.bytes), (Some(10), true, 42));
        assert!(t.join("p/f").exists());
        assert!(s.idx.exists());
    }

    #[test]
    fn a_stale_mirror_is_removed_with_its_index_cache() {
        isolate_gradle();
        let (_d, t) = canon_tempdir();
        write(&t.join("p/f"), 42);
        let s = Synced::new(&t.join("p"), 10);
        let r = collect(&GcReq {
            keep_days: Some(7),
            dry_run: false,
            ..req(&t)
        })
        .unwrap();
        let m = &r.mirrors[0];
        assert_eq!((m.removed, m.bytes), (true, 42));
        assert!(!t.join("p").exists());
        assert!(!s.idx.exists());
    }

    #[test]
    fn the_current_project_is_never_removed() {
        isolate_gradle();
        let (_d, t) = canon_tempdir();
        write(&t.join("p/f"), 42);
        let _s = Synced::new(&t.join("p"), 10);
        let r = collect(&GcReq {
            keep_days: Some(7),
            current: Some(t.join("p").to_string_lossy().into_owned()),
            dry_run: false,
            ..req(&t)
        })
        .unwrap();
        let m = &r.mirrors[0];
        assert_eq!((m.idle_days, m.removed), (Some(10), false));
        assert!(t.join("p/f").exists());
    }

    #[test]
    fn without_keep_days_a_stale_mirror_is_listed_and_kept() {
        let (_d, t) = canon_tempdir();
        write(&t.join("p/f"), 42);
        let _s = Synced::new(&t.join("p"), 10);
        let r = collect(&req(&t)).unwrap();
        let m = &r.mirrors[0];
        assert_eq!((m.idle_days, m.removed, m.bytes), (Some(10), false, 0));
    }

    #[test]
    #[cfg(unix)]
    fn files_and_symlinked_dirs_in_the_folder_are_ignored() {
        isolate_gradle();
        let (_d, t) = canon_tempdir();
        let folder = t.join("folder");
        write(&folder.join("loose-file"), 5);
        write(&t.join("outside/f"), 42);
        // the link's target is a stale synced mirror: still not reachable through the link
        let _s = Synced::new(&t.join("outside"), 10);
        std::os::unix::fs::symlink(t.join("outside"), folder.join("link")).unwrap();
        let r = collect(&GcReq {
            keep_days: Some(7),
            dry_run: false,
            ..req(&folder)
        })
        .unwrap();
        assert!(r.mirrors.is_empty());
        assert!(folder.join("loose-file").exists());
        assert!(t.join("outside/f").exists());
    }

    #[test]
    fn build_patterns_are_applied_inside_the_current_project_only() {
        isolate_gradle();
        let (_d, t) = canon_tempdir();
        android_layout(&t.join("cur"));
        android_layout(&t.join("sib"));
        let r = collect(&GcReq {
            current: Some(t.join("cur").to_string_lossy().into_owned()),
            build: layout_patterns(),
            dry_run: false,
            ..req(&t)
        })
        .unwrap();
        assert_eq!(r.build_bytes, LAYOUT_REMOVED);
        assert!(!t.join("cur/app/build/intermediates/foo").exists());
        assert!(!t.join("cur/top.log").exists());
        assert!(t.join("cur/app/build/intermediates/keep/x").exists());
        assert!(t.join("cur/app/src/z").exists());
        assert!(t.join("sib/app/build/intermediates/foo/a").exists());
        assert!(t.join("sib/top.log").exists());
    }

    #[test]
    fn build_patterns_dry_run_reports_the_same_bytes_as_remove_matching() {
        let (_d, t) = canon_tempdir();
        android_layout(&t.join("cur"));
        let r = collect(&GcReq {
            current: Some(t.join("cur").to_string_lossy().into_owned()),
            build: layout_patterns(),
            ..req(&t)
        })
        .unwrap();
        assert_eq!(r.build_bytes, remove_matching(&t.join("cur"), &layout_patterns(), true).unwrap());
        assert!(t.join("cur/top.log").exists());
    }

    #[test]
    fn build_patterns_without_a_current_project_do_nothing() {
        isolate_gradle();
        let (_d, t) = canon_tempdir();
        android_layout(&t.join("cur"));
        let r = collect(&GcReq {
            build: layout_patterns(),
            dry_run: false,
            ..req(&t)
        })
        .unwrap();
        assert_eq!(r.build_bytes, 0);
        assert!(t.join("cur/top.log").exists());
        assert!(t.join("cur/app/build/intermediates/foo/a").exists());
    }

    #[test]
    fn mirrors_are_reported_sorted_by_name() {
        let (_d, t) = canon_tempdir();
        for name in ["zeta", "alpha", "mid", "Beta"] {
            fs::create_dir(t.join(name)).unwrap();
        }
        let r = collect(&req(&t)).unwrap();
        let names: Vec<&str> = r.mirrors.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Beta", "alpha", "mid", "zeta"]);
        #[cfg(unix)]
        assert!(r.free > 0);
    }
}
