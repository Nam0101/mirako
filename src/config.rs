//! `~/.config/mirako/config.toml` (global) overlaid by `<project>/mirako.toml`.

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
        let g = read(&global_config_path())?;
        let p = read(&project_root.join("mirako.toml"))?;
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
                global_config_path().display(),
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

/// Nearest ancestor of `start` holding `mirako.toml`, `gradlew` or `.git`.
pub fn find_project_root(start: &Path) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .with_context(|| format!("{} does not exist", start.display()))?;
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

# Android: the IDE only needs build/outputs, build/generated (navigation) and the
# apk_ide_redirect_file that tells Android Studio where the APK is
exclude_remote_extra = ["build/intermediates", "!build/intermediates/apk_ide_redirect_file", "build/tmp", "build/kotlin", "build/kspCaches"]
"#;

pub const SAMPLE_GLOBAL_TOML: &str = r#"# ~/.config/mirako/config.toml
host = "m4"                      # ssh host or ~/.ssh/config alias
remote_folder = "~/mirako"       # one sub-folder per project on the remote
remote_bin = "~/.local/bin/mirako"
fallback = true                  # run locally when the host is unreachable
# ssh = ["ssh", "-o", "BatchMode=yes"]
# exclude_local  = ["build"]
# exclude_remote = ["src"]
# exclude_common = [".gradle", ".idea", ".git", ".kotlin", ".mirako", "local.properties", "mirako.toml", ".DS_Store"]
"#;
