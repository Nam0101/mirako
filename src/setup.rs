//! `mirako setup`: everything a new machine pair needs, each step idempotent and reported: the
//! global config, password-less ssh to the host, what the agent's shell there sees, the Gradle
//! init script, and the handshake that puts this binary on the host as the agent.

use crate::client::{self, human, ssh};
use crate::config::{self, Config};
use crate::shim;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn setup(host: Option<&str>) -> Result<()> {
    let global = config::global_config_path();
    if global.exists() {
        println!("{} exists", global.display());
    } else {
        let Some(host) = host else {
            bail!("no {} yet: pass --host <ssh host or alias> the first time", global.display());
        };
        fs::create_dir_all(global.parent().unwrap())?;
        fs::write(&global, config::sample_global(host))?;
        println!("wrote {} (host = {host})", global.display());
    }
    // works outside a project too: the global config names the host
    let root = client::project_root_for(None).unwrap_or_else(|_| PathBuf::from("."));
    let cfg = Config::load(&root, host)?;
    ensure_ssh_access(&cfg)?;
    report_host(&cfg)?;
    println!("wrote {}", shim::install()?.display());
    // the handshake installs or updates the agent on the host when needed
    client::check(&cfg)
}

/// `ssh host true` in batch mode. On a refusal, when `ssh` really is ssh, sets up key
/// authentication interactively (`ssh-keygen` if there is no key yet, then `ssh-copy-id`, which
/// asks for the password once) and probes again.
fn ensure_ssh_access(cfg: &Config) -> Result<()> {
    let probe = || -> Result<Option<String>> {
        let out = ssh(cfg).arg("true").stdin(Stdio::null()).output().context("running ssh")?;
        Ok((!out.status.success()).then(|| String::from_utf8_lossy(&out.stderr).trim().to_string()))
    };
    let Some(err) = probe()? else {
        println!("ssh {}: ok", cfg.host);
        return Ok(());
    };
    let is_ssh = Path::new(&cfg.ssh[0]).file_name().is_some_and(|n| n == "ssh");
    let fixable = err.contains("Permission denied") || err.contains("Host key verification failed");
    if !is_ssh || !fixable {
        bail!("ssh {} failed: {err}", cfg.host);
    }
    println!("ssh {}: {err}", cfg.host);
    println!(
        "setting up key authentication: ssh-copy-id asks for the password of {} once",
        cfg.host
    );
    let key = ensure_key()?;
    let status = Command::new("ssh-copy-id")
        .arg("-i")
        .arg(&key)
        .args(&cfg.ssh[1..])
        .arg(&cfg.host)
        .status()
        .context("running ssh-copy-id")?;
    if !status.success() {
        bail!("ssh-copy-id {} failed", cfg.host);
    }
    if let Some(err) = probe()? {
        bail!("ssh {} still fails after ssh-copy-id: {err}", cfg.host);
    }
    println!("ssh {}: ok", cfg.host);
    Ok(())
}

/// The default identity, generated (ed25519, no passphrase) when there is none.
fn ensure_key() -> Result<PathBuf> {
    let dir = dirs::home_dir().context("no home dir")?.join(".ssh");
    for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
        let key = dir.join(name);
        if key.exists() {
            return Ok(key);
        }
    }
    let key = dir.join("id_ed25519");
    println!("no ssh key yet: generating {}", key.display());
    fs::create_dir_all(&dir)?;
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .context("running ssh-keygen")?;
    if !status.success() {
        bail!("ssh-keygen failed");
    }
    Ok(key)
}

/// Runs in the host's non-interactive shell, the one the agent runs commands in.
const PROBE: &str = r#"printf 'os=%s\n' "$(uname -sm)"; printf 'java=%s\n' "$(command -v java)"; printf 'java_version=%s\n' "$(java -version 2>&1 | head -1)"; printf 'java_home=%s\n' "$JAVA_HOME"; printf 'android=%s\n' "${ANDROID_HOME:-$ANDROID_SDK_ROOT}"; printf 'free_kb=%s\n' "$(df -Pk "$HOME" | awk 'NR==2 {print $4}')""#;

const LOW_DISK_KB: u64 = 10 << 20; // 10 GiB

/// What a build on the host would find, with a warning for each missing piece.
fn report_host(cfg: &Config) -> Result<()> {
    let out = ssh(cfg).arg(PROBE).stdin(Stdio::null()).output().context("running ssh")?;
    if !out.status.success() {
        bail!("probing {} failed: {}", cfg.host, String::from_utf8_lossy(&out.stderr).trim());
    }
    for line in describe(&cfg.host, &parse(&String::from_utf8_lossy(&out.stdout))) {
        println!("{line}");
    }
    Ok(())
}

fn parse(text: &str) -> HashMap<&str, &str> {
    text.lines().filter_map(|l| l.split_once('=')).collect()
}

/// One summary line, then a `warning:` line per missing piece.
fn describe(host: &str, facts: &HashMap<&str, &str>) -> Vec<String> {
    let get = |k: &str| facts.get(k).copied().unwrap_or("").trim();
    let free_kb: u64 = get("free_kb").parse().unwrap_or(0);
    let java = if get("java").is_empty() {
        "no java".to_string()
    } else {
        format!("{} at {}", get("java_version"), get("java"))
    };
    let android = if get("android").is_empty() {
        "no ANDROID_HOME".to_string()
    } else {
        format!("ANDROID_HOME={}", get("android"))
    };
    let free = if free_kb == 0 {
        "free space unknown".to_string()
    } else {
        format!("{} free", human(free_kb * 1024))
    };
    let mut lines = vec![format!("{host}: {}; {java}; {android}; {free}", get("os"))];
    if get("java").is_empty() {
        lines.push(format!(
            "warning: no `java` in the non-interactive shell on {host}: Gradle builds fail there. Set JAVA_HOME and PATH in ~/.zshenv (macOS) or ~/.bashrc (Linux), the files that shell reads"
        ));
    }
    if get("android").is_empty() {
        lines.push(format!(
            "warning: ANDROID_HOME (or ANDROID_SDK_ROOT) is unset on {host}: Android builds need it there, the `sdk.dir` of local.properties stays on this machine"
        ));
    }
    if free_kb > 0 && free_kb < LOW_DISK_KB {
        lines.push(format!(
            "warning: only {} free on {host}: `gc_days` and `gc_after_pull` in the config reclaim space",
            human(free_kb * 1024)
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(probe_output: &str) -> Vec<String> {
        describe("h", &parse(probe_output))
    }

    #[test]
    fn a_complete_host_is_one_line_without_warnings() {
        let l = lines("os=Darwin arm64\njava=/usr/bin/java\njava_version=openjdk version \"21.0.1\"\njava_home=/jdk\nandroid=/sdk\nfree_kb=26096536\n");
        assert_eq!(
            l,
            vec!["h: Darwin arm64; openjdk version \"21.0.1\" at /usr/bin/java; ANDROID_HOME=/sdk; 24.9 GB free"]
        );
    }

    #[test]
    fn missing_java_sdk_and_disk_each_warn() {
        let l = lines("os=Linux x86_64\njava=\njava_version=sh: java: not found\njava_home=\nandroid=\nfree_kb=1048576\n");
        assert_eq!(l.len(), 4, "{l:?}");
        assert_eq!(l[0], "h: Linux x86_64; no java; no ANDROID_HOME; 1.0 GB free");
        assert!(l[1].contains("JAVA_HOME") && l[1].contains(".zshenv"), "{}", l[1]);
        assert!(l[2].contains("ANDROID_HOME"), "{}", l[2]);
        assert!(l[3].contains("gc_days"), "{}", l[3]);
    }

    #[test]
    fn unknown_free_space_is_said_not_warned_about() {
        let l = lines("os=Linux x86_64\njava=/j\njava_version=v\nandroid=/s\nfree_kb=\n");
        assert_eq!(l, vec!["h: Linux x86_64; v at /j; ANDROID_HOME=/s; free space unknown"]);
    }

    #[test]
    fn the_probe_prints_every_key_the_report_reads() {
        for key in ["os=", "java=", "java_version=", "java_home=", "android=", "free_kb="] {
            assert!(PROBE.contains(key), "missing {key}");
        }
    }
}
