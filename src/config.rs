//! `~/.config/mirako/config.toml` (global) overlaid by `<project>/mirako.toml`.

use crate::xfer::canonical;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub host: Option<String>,
    pub remote_folder: Option<String>,
    pub remote_bin: Option<String>,
    pub ssh: Option<Vec<String>>,
    pub fallback: Option<bool>,
    pub exclude_local: Option<Vec<String>>,
    pub exclude_remote: Option<Vec<String>>,
    pub exclude_common: Option<Vec<String>>,
    /// appended to the (possibly default) lists above
    pub exclude_local_extra: Option<Vec<String>>,
    pub exclude_remote_extra: Option<Vec<String>>,
    pub exclude_common_extra: Option<Vec<String>>,
    pub gc_days: Option<u32>,
    pub gc_after_pull: Option<Vec<String>>,
    /// global only: whether the Gradle init script runs `mirako check` before taking a build (default true)
    pub shim_check: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub remote_folder: String,
    pub remote_bin: String,
    pub ssh: Vec<String>,
    pub fallback: bool,
    pub exclude_local: Vec<String>,
    pub exclude_remote: Vec<String>,
    pub exclude_common: Vec<String>,
    /// a project's copy on the remote unused for this long is removed after a run; 0 = never
    pub gc_days: u32,
    /// deleted on the remote after every pull (rsync-like patterns)
    pub gc_after_pull: Vec<String>,
}

pub const DEFAULT_EXCLUDE_LOCAL: &[&str] = &["build"];
pub const DEFAULT_EXCLUDE_REMOTE: &[&str] = &["src"];
pub const DEFAULT_EXCLUDE_COMMON: &[&str] = &[
    ".gradle",
    ".idea",
    ".git",
    ".kotlin",
    ".mirako",
    "local.properties",
    "mirako.toml",
    ".DS_Store",
];

/// `~/.config/mirako/config.toml` on every OS (macOS's Application Support is not where people look).
pub fn global_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("mirako")
        .join("config.toml")
}

fn read(path: &Path) -> Result<FileConfig> {
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let text = fs::read_to_string(path)?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

impl Config {
    pub fn load(project_root: &Path, host_override: Option<&str>) -> Result<Self> {
        Self::load_from(&global_config_path(), project_root, host_override)
    }

    /// `load` with the global config file named explicitly (tests).
    pub fn load_from(global: &Path, project_root: &Path, host_override: Option<&str>) -> Result<Self> {
        let g = read(global)?;
        let p = read(&project_root.join("mirako.toml"))?;
        if p.shim_check.is_some() {
            bail!(
                "`shim_check` belongs in {} (the init script is one per machine), not in {}",
                global.display(),
                project_root.join("mirako.toml").display()
            );
        }
        let pick = |a: Option<String>, b: Option<String>| b.or(a);
        let list = |def: &[&str], a: Option<Vec<String>>, b: Option<Vec<String>>, ea: Option<Vec<String>>, eb: Option<Vec<String>>| {
            let mut v = b.or(a).unwrap_or_else(|| def.iter().map(|s| s.to_string()).collect());
            v.extend(ea.unwrap_or_default());
            v.extend(eb.unwrap_or_default());
            v
        };
        let host = host_override.map(str::to_string).or_else(|| pick(g.host.clone(), p.host.clone()));
        let Some(host) = host else {
            bail!(
                "no host configured: pass --host, or set `host = \"...\"` in {} or {}",
                global.display(),
                project_root.join("mirako.toml").display()
            );
        };
        Ok(Config {
            host,
            remote_folder: pick(g.remote_folder.clone(), p.remote_folder.clone()).unwrap_or_else(|| "~/mirako".into()),
            remote_bin: pick(g.remote_bin.clone(), p.remote_bin.clone()).unwrap_or_else(|| "~/.local/bin/mirako".into()),
            ssh: p.ssh.clone().or(g.ssh.clone()).unwrap_or_else(|| vec!["ssh".into()]),
            fallback: p.fallback.or(g.fallback).unwrap_or(false),
            exclude_local: list(
                DEFAULT_EXCLUDE_LOCAL,
                g.exclude_local.clone(),
                p.exclude_local.clone(),
                g.exclude_local_extra.clone(),
                p.exclude_local_extra.clone(),
            ),
            exclude_remote: list(
                DEFAULT_EXCLUDE_REMOTE,
                g.exclude_remote.clone(),
                p.exclude_remote.clone(),
                g.exclude_remote_extra.clone(),
                p.exclude_remote_extra.clone(),
            ),
            exclude_common: list(
                DEFAULT_EXCLUDE_COMMON,
                g.exclude_common.clone(),
                p.exclude_common.clone(),
                g.exclude_common_extra.clone(),
                p.exclude_common_extra.clone(),
            ),
            gc_days: p.gc_days.or(g.gc_days).unwrap_or(7),
            gc_after_pull: p.gc_after_pull.clone().or(g.gc_after_pull.clone()).unwrap_or_default(),
        })
    }

    pub fn upload_excludes(&self) -> Vec<String> {
        self.exclude_local.iter().chain(&self.exclude_common).cloned().collect()
    }

    pub fn download_excludes(&self) -> Vec<String> {
        self.exclude_remote.iter().chain(&self.exclude_common).cloned().collect()
    }

    /// `~/mirako/<project name>` on the remote, still with the `~` for the remote shell/agent to expand.
    pub fn remote_dir(&self, project_root: &Path) -> String {
        let name = project_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".into());
        format!("{}/{}", self.remote_folder.trim_end_matches('/'), name)
    }
}

/// `shim_check` of the global config (no host needed): whether the Gradle init script handshakes first.
pub fn shim_check() -> Result<bool> {
    shim_check_from(&global_config_path())
}

pub fn shim_check_from(global: &Path) -> Result<bool> {
    Ok(read(global)?.shim_check.unwrap_or(true))
}

/// `SAMPLE_GLOBAL_TOML` with `host` filled in.
pub fn sample_global(host: &str) -> String {
    SAMPLE_GLOBAL_TOML.replacen("host = \"m4\"", &format!("host = \"{host}\""), 1)
}

/// Nearest ancestor of `start` holding `mirako.toml`, `gradlew` or `.git`.
pub fn find_project_root(start: &Path) -> Result<PathBuf> {
    let start = canonical(start).with_context(|| format!("{} does not exist", start.display()))?;
    let mut dir = Some(start.as_path());
    while let Some(d) = dir {
        for marker in ["mirako.toml", "gradlew", ".git"] {
            if d.join(marker).exists() {
                return Ok(d.to_path_buf());
            }
        }
        dir = d.parent();
    }
    bail!(
        "no project root found above {} (looked for mirako.toml, gradlew or .git)",
        start.display()
    )
}

pub const SAMPLE_PROJECT_TOML: &str = r#"# mirako.toml — per-project settings (overrides ~/.config/mirako/config.toml)
# host = "m4"
# remote_folder = "~/mirako"
# fallback = true

# Android: the IDE needs build/outputs, build/generated (navigation) and, to deploy, the
# apk_ide_redirect_file plus the build/intermediates/apk it points at
exclude_remote_extra = ["build/intermediates", "!build/intermediates/apk_ide_redirect_file", "!build/intermediates/apk", "build/tmp", "build/kotlin", "build/kspCaches"]

# Free the remote's disk after every pull at the price of a clean build next time:
# gc_after_pull = ["build/intermediates", "build/tmp", "build/kotlin", "build/kspCaches"]
"#;

pub const SAMPLE_GLOBAL_TOML: &str = r#"# ~/.config/mirako/config.toml
host = "m4"                      # ssh host or ~/.ssh/config alias
remote_folder = "~/mirako"       # one sub-folder per project on the remote
remote_bin = "~/.local/bin/mirako"
fallback = true                  # run locally when the host is unreachable
gc_days = 7                      # remove a project's remote copy unused for this long (Gradle's caches there too); 0 = never
# gc_after_pull = ["build/intermediates", "build/tmp"]   # deleted on the remote after every pull: saves disk, costs a clean build next time
# shim_check = false             # Gradle shim: skip the ~0.1 s handshake before each build (a dead host then fails, or falls back, inside `mirako run`)
# ssh = ["ssh", "-o", "BatchMode=yes"]
# exclude_local  = ["build"]
# exclude_remote = ["src"]
# exclude_common = [".gradle", ".idea", ".git", ".kotlin", ".mirako", "local.properties", "mirako.toml", ".DS_Store"]
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A temp dir with a project root `proj/` and a (not yet written) global config path.
    struct Setup {
        _dir: TempDir,
        global: PathBuf,
        root: PathBuf,
    }

    impl Setup {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let global = dir.path().join("global.toml");
            let root = dir.path().join("proj");
            fs::create_dir(&root).unwrap();
            Self { _dir: dir, global, root }
        }

        fn global(self, text: &str) -> Self {
            fs::write(&self.global, text).unwrap();
            self
        }

        fn project(self, text: &str) -> Self {
            fs::write(self.root.join("mirako.toml"), text).unwrap();
            self
        }

        fn load(&self, host: Option<&str>) -> Result<Config> {
            Config::load_from(&self.global, &self.root, host)
        }
    }

    #[test]
    fn no_config_files_and_a_host_override_give_the_defaults() {
        let s = Setup::new();
        let c = s.load(Some("h")).unwrap();
        assert_eq!(c.host, "h");
        assert_eq!(c.remote_folder, "~/mirako");
        assert_eq!(c.remote_bin, "~/.local/bin/mirako");
        assert_eq!(c.ssh, strings(&["ssh"]));
        assert!(!c.fallback);
        assert_eq!(c.gc_days, 7);
        assert!(c.gc_after_pull.is_empty());
        assert_eq!(c.exclude_local, strings(DEFAULT_EXCLUDE_LOCAL));
        assert_eq!(c.exclude_remote, strings(DEFAULT_EXCLUDE_REMOTE));
        assert_eq!(c.exclude_common, strings(DEFAULT_EXCLUDE_COMMON));
    }

    #[test]
    fn no_host_anywhere_is_an_error_naming_the_flag_and_both_files() {
        let s = Setup::new().global("fallback = true\n");
        let msg = format!("{:#}", s.load(None).unwrap_err());
        assert!(msg.contains("--host"), "{msg}");
        assert!(msg.contains(&s.root.join("mirako.toml").display().to_string()), "{msg}");
        assert!(msg.contains(&s.global.display().to_string()), "{msg}");
    }

    #[test]
    fn project_overrides_global_key_by_key_and_the_host_flag_beats_both() {
        let s = Setup::new()
            .global("host = \"g\"\nremote_folder = \"/srv/m\"\nfallback = true\ngc_days = 3\nssh = [\"ssh\", \"-p\", \"22\"]\n")
            .project("host = \"p\"\ngc_days = 0\n");
        let c = s.load(None).unwrap();
        assert_eq!(c.host, "p");
        assert_eq!(c.gc_days, 0);
        assert_eq!(c.remote_folder, "/srv/m");
        assert!(c.fallback);
        assert_eq!(c.ssh, strings(&["ssh", "-p", "22"]));
        assert_eq!(c.remote_bin, "~/.local/bin/mirako");
        assert_eq!(s.load(Some("flag")).unwrap().host, "flag");
    }

    #[test]
    fn global_host_is_used_when_the_project_has_none() {
        let s = Setup::new().global("host = \"g\"\n").project("fallback = true\n");
        let c = s.load(None).unwrap();
        assert_eq!(c.host, "g");
        assert!(c.fallback);
    }

    #[test]
    fn a_global_list_replaces_the_default_and_extras_append_global_then_project() {
        let s = Setup::new()
            .global("exclude_local = [\"x\"]\nexclude_local_extra = [\"g\"]\n")
            .project("exclude_local_extra = [\"y\"]\n");
        let c = s.load(Some("h")).unwrap();
        assert_eq!(c.exclude_local, strings(&["x", "g", "y"]));
        assert_eq!(c.exclude_remote, strings(DEFAULT_EXCLUDE_REMOTE));
    }

    #[test]
    fn extras_append_to_the_default_list_when_no_base_is_set() {
        let s = Setup::new()
            .global("exclude_remote_extra = [\"g\"]\n")
            .project("exclude_remote_extra = [\"p1\", \"p2\"]\n");
        let c = s.load(Some("h")).unwrap();
        let mut want = strings(DEFAULT_EXCLUDE_REMOTE);
        want.extend(strings(&["g", "p1", "p2"]));
        assert_eq!(c.exclude_remote, want);
    }

    #[test]
    fn a_project_list_replaces_the_global_one_entirely_while_extras_still_append() {
        let s = Setup::new()
            .global("exclude_local = [\"x\", \"x2\"]\nexclude_local_extra = [\"g\"]\nexclude_common = [\"gc\"]\n")
            .project(
                "exclude_local = [\"p\"]\nexclude_local_extra = [\"y\"]\nexclude_common = [\"pc\"]\nexclude_common_extra = [\"pe\"]\n",
            );
        let c = s.load(Some("h")).unwrap();
        assert_eq!(c.exclude_local, strings(&["p", "g", "y"]));
        assert_eq!(c.exclude_common, strings(&["pc", "pe"]));
    }

    #[test]
    fn an_empty_project_list_clears_the_base() {
        let s = Setup::new().global("exclude_remote = [\"a\"]\n").project("exclude_remote = []\n");
        assert!(s.load(Some("h")).unwrap().exclude_remote.is_empty());
    }

    #[test]
    fn gc_after_pull_in_the_project_replaces_the_global_one() {
        let s = Setup::new().global("gc_after_pull = [\"build/tmp\"]\n");
        assert_eq!(s.load(Some("h")).unwrap().gc_after_pull, strings(&["build/tmp"]));
        let s = s.project("gc_after_pull = [\"build/kotlin\"]\n");
        assert_eq!(s.load(Some("h")).unwrap().gc_after_pull, strings(&["build/kotlin"]));
    }

    #[test]
    fn an_unknown_key_in_the_global_file_is_an_error_naming_it() {
        let s = Setup::new().global("hots = \"typo\"\n");
        let msg = format!("{:#}", s.load(Some("h")).unwrap_err());
        assert!(msg.contains(&format!("parsing {}", s.global.display())), "{msg}");
    }

    #[test]
    fn an_unknown_key_in_the_project_file_is_an_error_naming_it() {
        let s = Setup::new().project("exclude = [\"x\"]\n");
        let msg = format!("{:#}", s.load(Some("h")).unwrap_err());
        assert!(msg.contains(&format!("parsing {}", s.root.join("mirako.toml").display())), "{msg}");
    }

    #[test]
    fn a_value_of_the_wrong_type_is_an_error() {
        let s = Setup::new().project("gc_days = \"seven\"\n");
        assert!(s.load(Some("h")).is_err());
    }

    #[test]
    fn upload_and_download_excludes_append_the_common_list() {
        let s = Setup::new().project("exclude_local = [\"l\"]\nexclude_remote = [\"r\"]\nexclude_common = [\"c1\", \"c2\"]\n");
        let c = s.load(Some("h")).unwrap();
        assert_eq!(c.upload_excludes(), strings(&["l", "c1", "c2"]));
        assert_eq!(c.download_excludes(), strings(&["r", "c1", "c2"]));
    }

    #[test]
    fn remote_dir_is_the_remote_folder_plus_the_project_name() {
        let s = Setup::new();
        let mut c = s.load(Some("h")).unwrap();
        assert_eq!(c.remote_dir(Path::new("/a/b/proj")), "~/mirako/proj");
        c.remote_folder = "/srv/builds//".into();
        assert_eq!(c.remote_dir(Path::new("/a/b/proj")), "/srv/builds/proj");
        assert_eq!(c.remote_dir(Path::new("/")), "/srv/builds/project");
    }

    /// A canonical temp dir: on macOS temp dirs live under /private/var, `find_project_root` canonicalizes.
    fn canon_tempdir() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canon = canonical(dir.path()).unwrap();
        (dir, canon)
    }

    #[test]
    fn project_root_is_found_by_mirako_toml_from_a_nested_subdir() {
        let (_d, t) = canon_tempdir();
        fs::create_dir_all(t.join("p/a/b")).unwrap();
        fs::write(t.join("p/mirako.toml"), "").unwrap();
        assert_eq!(find_project_root(&t.join("p/a/b")).unwrap(), t.join("p"));
        assert_eq!(find_project_root(&t.join("p")).unwrap(), t.join("p"));
    }

    #[test]
    fn project_root_is_found_by_gradlew() {
        let (_d, t) = canon_tempdir();
        fs::create_dir_all(t.join("p/app/src")).unwrap();
        fs::write(t.join("p/gradlew"), "").unwrap();
        assert_eq!(find_project_root(&t.join("p/app/src")).unwrap(), t.join("p"));
    }

    #[test]
    fn project_root_is_found_by_a_git_directory() {
        let (_d, t) = canon_tempdir();
        fs::create_dir_all(t.join("p/.git")).unwrap();
        fs::create_dir_all(t.join("p/x")).unwrap();
        assert_eq!(find_project_root(&t.join("p/x")).unwrap(), t.join("p"));
    }

    #[test]
    fn project_root_search_can_start_from_a_file() {
        let (_d, t) = canon_tempdir();
        fs::create_dir_all(t.join("p/a")).unwrap();
        fs::write(t.join("p/gradlew"), "").unwrap();
        fs::write(t.join("p/a/file.kt"), "").unwrap();
        assert_eq!(find_project_root(&t.join("p/a/file.kt")).unwrap(), t.join("p"));
    }

    #[test]
    fn the_nearest_marker_wins_over_an_outer_one() {
        let (_d, t) = canon_tempdir();
        fs::create_dir_all(t.join("p/.git")).unwrap();
        fs::create_dir_all(t.join("p/sub/x")).unwrap();
        fs::write(t.join("p/sub/gradlew"), "").unwrap();
        assert_eq!(find_project_root(&t.join("p/sub/x")).unwrap(), t.join("p/sub"));
    }

    #[test]
    fn no_marker_up_to_the_filesystem_root_is_an_error() {
        let (_d, t) = canon_tempdir();
        fs::create_dir_all(t.join("a/b")).unwrap();
        let msg = format!("{:#}", find_project_root(&t.join("a/b")).unwrap_err());
        assert!(msg.contains("no project root found"), "{msg}");
    }

    #[test]
    fn a_nonexistent_start_is_an_error() {
        let (_d, t) = canon_tempdir();
        let msg = format!("{:#}", find_project_root(&t.join("missing")).unwrap_err());
        assert!(msg.contains("does not exist"), "{msg}");
    }

    #[test]
    fn the_sample_configs_parse() {
        toml::from_str::<FileConfig>(SAMPLE_PROJECT_TOML).unwrap();
        let g = toml::from_str::<FileConfig>(SAMPLE_GLOBAL_TOML).unwrap();
        assert_eq!(g.host.as_deref(), Some("m4"));
        assert_eq!(g.gc_days, Some(7));
        assert_eq!(g.fallback, Some(true));
    }

    #[test]
    fn shim_check_defaults_to_true_and_comes_from_the_global_file_only() {
        let s = Setup::new();
        assert!(shim_check_from(&s.global).unwrap());
        let s = s.global("shim_check = false\n");
        assert!(!shim_check_from(&s.global).unwrap());
        assert!(s.load(Some("h")).is_ok());
        let s = s.project("shim_check = true\n");
        let msg = format!("{:#}", s.load(Some("h")).unwrap_err());
        assert!(msg.contains("shim_check") && msg.contains(&s.global.display().to_string()), "{msg}");
    }

    #[test]
    fn sample_global_fills_in_the_host() {
        let g = toml::from_str::<FileConfig>(&sample_global("buildbox")).unwrap();
        assert_eq!(g.host.as_deref(), Some("buildbox"));
        assert_eq!(g.gc_days, Some(7));
        assert_eq!(sample_global("m4"), SAMPLE_GLOBAL_TOML);
    }

    #[test]
    fn global_config_path_is_under_dot_config() {
        assert!(global_config_path().ends_with(".config/mirako/config.toml"));
    }
}
