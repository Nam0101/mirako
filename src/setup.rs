//! `mirako setup`: everything a new machine pair needs, each step idempotent and reported: the
//! global config, password-less ssh to the host, what the agent's shell there sees, the Gradle
//! init script, and the handshake that puts this binary on the host as the agent.

use crate::client::{self, human, ssh};
use crate::config::{self, Config};
use crate::host;
use crate::shim;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
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

/// `ssh host 'exit 0'` in batch mode (`exit 0` is the same in every shell, `true` is not a command
/// of cmd). On a refusal, when `ssh` really is ssh, sets up key authentication interactively
/// (`ssh-keygen` if there is no key yet, then `ssh-copy-id`, which asks for the password once)
/// and probes again.
fn ensure_ssh_access(cfg: &Config) -> Result<()> {
    let probe = || -> Result<Option<String>> {
        let out = ssh(cfg).arg("exit 0").stdin(Stdio::null()).output().context("running ssh")?;
        Ok((!out.status.success()).then(|| String::from_utf8_lossy(&out.stderr).trim().to_string()))
    };
    let Some(err) = probe()? else {
        println!("ssh {}: ok", cfg.host);
        return Ok(());
    };
    let is_ssh = Path::new(&cfg.ssh[0]).file_stem().is_some_and(|n| n == "ssh");
    let fixable = err.contains("Permission denied") || err.contains("Host key verification failed");
    if !is_ssh || !fixable {
        bail!("ssh {} failed: {err}", cfg.host);
    }
    println!("ssh {}: {err}", cfg.host);
    println!("setting up key authentication: ssh asks for the password of {} once", cfg.host);
    let key = ensure_key()?;
    match Command::new("ssh-copy-id")
        .arg("-i")
        .arg(&key)
        .args(&cfg.ssh[1..])
        .arg(&cfg.host)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(_) => bail!("ssh-copy-id {} failed", cfg.host),
        // Windows has none
        Err(e) if e.kind() == io::ErrorKind::NotFound => append_key(cfg, &key)?,
        Err(e) => return Err(e).context("running ssh-copy-id"),
    }
    if let Some(err) = probe()? {
        bail!("ssh {} still fails after adding the key: {err}", cfg.host);
    }
    println!("ssh {}: ok", cfg.host);
    Ok(())
}

/// Appends to `~/.ssh/authorized_keys` of the host, unless that holds it already, what is
/// on stdin: the line of a public key.
const APPEND_KEY: &str = "umask 077 && mkdir -p ~/.ssh && k=$(cat) && { grep -qxF \"$k\" ~/.ssh/authorized_keys 2>/dev/null || echo \"$k\" >> ~/.ssh/authorized_keys; }";

/// What `ssh-copy-id` does, for a client without it (Windows): the public key of `key` goes into
/// `~/.ssh/authorized_keys` of a Unix host, over an ssh that asks for the password.
fn append_key(cfg: &Config, key: &Path) -> Result<()> {
    let public = key.with_extension("pub");
    let line = fs::read_to_string(&public).with_context(|| format!("reading {}", public.display()))?;
    let mut child = Command::new(&cfg.ssh[0])
        .args(&cfg.ssh[1..])
        .arg(&cfg.host)
        .arg(APPEND_KEY)
        .stdin(Stdio::piped())
        .spawn()
        .context("running ssh")?;
    child.stdin.take().unwrap().write_all(format!("{}\n", line.trim()).as_bytes())?;
    if !child.wait()?.success() {
        bail!(
            "adding {} to ~/.ssh/authorized_keys on {} failed. A Windows host reads the keys of an administrator from C:\\ProgramData\\ssh\\administrators_authorized_keys, those of other users from ~/.ssh/authorized_keys: put the key there yourself",
            public.display(),
            cfg.host
        );
    }
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

/// Runs in the host's non-interactive shell, the one the agent runs commands in (`PROBE_WINDOWS`
/// on a Windows host).
const PROBE: &str = r#"printf 'os=%s\n' "$(uname -sm)"; printf 'java=%s\n' "$(command -v java)"; printf 'java_version=%s\n' "$(java -version 2>&1 | head -1)"; printf 'java_home=%s\n' "$JAVA_HOME"; printf 'android=%s\n' "${ANDROID_HOME:-$ANDROID_SDK_ROOT}"; printf 'free_kb=%s\n' "$(df -Pk "$HOME" | awk 'NR==2 {print $4}')""#;

/// `PROBE` in PowerShell, for a Windows host: what its users' environment gives the agent, which
/// is what it gives the builds.
const PROBE_WINDOWS: &str = r#"try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch {}
$java = Get-Command java -ErrorAction SilentlyContinue
$drive = Get-PSDrive -Name $env:USERPROFILE.Substring(0, 1) -ErrorAction SilentlyContinue
"os=Windows $env:PROCESSOR_ARCHITECTURE"
"java=$(if ($java) { $java.Source })"
"java_version=$(if ($java) { cmd /c 'java -version 2>&1' | Select-Object -First 1 })"
"java_home=$env:JAVA_HOME"
"android=$(if ($env:ANDROID_HOME) { $env:ANDROID_HOME } else { $env:ANDROID_SDK_ROOT })"
"free_kb=$(if ($drive) { [math]::Floor($drive.Free / 1KB) })""#;

const LOW_DISK_KB: u64 = 10 << 20; // 10 GiB

/// What a build on the host would find, with a warning for each missing piece.
fn report_host(cfg: &Config) -> Result<()> {
    let found = host::probe(cfg)?;
    host::remember(cfg, found.shell);
    let windows = found.shell.windows();
    let probe = if windows {
        host::powershell(PROBE_WINDOWS)
    } else {
        PROBE.to_string()
    };
    let out = ssh(cfg).arg(probe).stdin(Stdio::null()).output().context("running ssh")?;
    if !out.status.success() {
        bail!("probing {} failed: {}", cfg.host, String::from_utf8_lossy(&out.stderr).trim());
    }
    for line in describe(&cfg.host, &parse(&String::from_utf8_lossy(&out.stdout)), windows) {
        println!("{line}");
    }
    Ok(())
}

fn parse(text: &str) -> HashMap<&str, &str> {
    text.lines().filter_map(|l| l.split_once('=')).collect()
}

/// One summary line, then a `warning:` line per missing piece.
fn describe(host: &str, facts: &HashMap<&str, &str>, windows: bool) -> Vec<String> {
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
    // where the agent's environment comes from
    let set_there = if windows {
        "as environment variables of the user there (System Properties > Environment Variables, or `setx`); a new ssh session reads them"
    } else {
        "in ~/.zshenv (macOS) or ~/.bashrc (Linux), the files that shell reads"
    };
    if get("java").is_empty() {
        lines.push(format!(
            "warning: no `java` in the non-interactive shell on {host}: Gradle builds fail there. Set JAVA_HOME and PATH {set_there}"
        ));
    }
    if get("android").is_empty() {
        lines.push(format!(
            "warning: ANDROID_HOME (or ANDROID_SDK_ROOT) is unset on {host}: Android builds need it there, the `sdk.dir` of local.properties stays on this machine. Set it {set_there}"
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
        describe("h", &parse(probe_output), false)
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
            assert!(PROBE_WINDOWS.contains(&format!("\"{key}")), "missing {key} on Windows");
        }
    }

    #[test]
    fn a_windows_host_is_told_where_its_environment_comes_from() {
        let facts = parse("os=Windows AMD64\r\njava=\r\njava_version=\r\njava_home=\r\nandroid=C:\\sdk\r\nfree_kb=52428800\r\n");
        let l = describe("w", &facts, true);
        assert_eq!(l[0], "w: Windows AMD64; no java; ANDROID_HOME=C:\\sdk; 50.0 GB free");
        assert_eq!(l.len(), 2, "{l:?}");
        assert!(
            l[1].contains("environment variables of the user") && !l[1].contains(".zshenv"),
            "{}",
            l[1]
        );
    }

    #[test]
    #[cfg(unix)] // the host runs it, in sh
    fn the_key_is_appended_once_to_authorized_keys() {
        let dir = tempfile::tempdir().unwrap();
        let run = |key: &str| {
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(APPEND_KEY)
                .env("HOME", dir.path())
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(key.as_bytes()).unwrap();
            assert!(child.wait().unwrap().success());
        };
        run("ssh-ed25519 AAAA one\n");
        run("ssh-ed25519 AAAA one\n");
        run("ssh-ed25519 BBBB two\n");
        let keys = fs::read_to_string(dir.path().join(".ssh/authorized_keys")).unwrap();
        assert_eq!(keys, "ssh-ed25519 AAAA one\nssh-ed25519 BBBB two\n");
    }
}
