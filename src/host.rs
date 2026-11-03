//! The host's shell. Every command the client sends over ssh runs in it, the agent's `serve`
//! included, and it is not always `sh`: the OpenSSH server of Windows runs a command in its
//! `DefaultShell`, `cmd.exe` unless that was set to PowerShell. `probe` finds out which one a host
//! has, `known`/`remember` keep the answer per host next to the index caches (so a run spells its
//! `serve` right the first time), and the scripts that put the agent on a host come in each
//! spelling: sh as it is, and for Windows a PowerShell script, which both of its shells start.

use crate::client::{ssh, REPO};
use crate::config::Config;
use crate::proto;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    /// macOS, Linux
    Sh,
    /// Windows, OpenSSH's default
    Cmd,
    /// Windows with `DefaultShell` set to `powershell.exe` or `pwsh.exe`
    PowerShell,
}

/// What `probe` finds on a host: its shell, and its platform as `uname -sm` spells it
/// (`Darwin arm64`, `Linux x86_64`), or `Windows AMD64` / `Windows ARM64`.
#[derive(Debug, PartialEq, Eq)]
pub struct Probe {
    pub shell: Shell,
    pub platform: String,
}

/// One line for every shell: `%…%` is a variable to cmd only, `$env:…` to PowerShell only, and
/// `;` separates two commands for sh and PowerShell but is more text for cmd's `echo`. sh prints
/// `%OS% %PROCESSOR_ARCHITECTURE% :OS :PROCESSOR_ARCHITECTURE`, then its `uname -sm` on a line of
/// its own; cmd `Windows_NT AMD64 $env:OS $env:PROCESSOR_ARCHITECTURE; uname -sm`; PowerShell
/// `%OS%`, `%PROCESSOR_ARCHITECTURE%`, `Windows_NT` and `AMD64` one per line (and fails on `uname`).
pub const PROBE: &str = "echo %OS% %PROCESSOR_ARCHITECTURE% $env:OS $env:PROCESSOR_ARCHITECTURE; uname -sm";

/// One ssh call: the host's shell and platform.
pub fn probe(cfg: &Config) -> Result<Probe> {
    let out = ssh(cfg).arg(PROBE).stdin(Stdio::null()).output().context("running ssh")?;
    let text = String::from_utf8_lossy(&out.stdout);
    if let Some(p) = parse_probe(&text) {
        return Ok(p);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if out.status.code() == Some(255) {
        // ssh's own failure: unreachable, refused
        bail!("cannot ssh to {}: {}", cfg.host, err.trim());
    }
    bail!(
        "cannot tell what {} runs: `{PROBE}` printed {:?} ({})",
        cfg.host,
        text.trim(),
        err.trim()
    )
}

fn parse_probe(out: &str) -> Option<Probe> {
    let words: Vec<&str> = out.split_whitespace().collect();
    if let Some(i) = words.iter().position(|w| *w == "Windows_NT") {
        // cmd echoes PowerShell's half of the line as it stands, PowerShell prints nothing of cmd's
        let shell = if words.get(i + 2) == Some(&"$env:OS") {
            Shell::Cmd
        } else {
            Shell::PowerShell
        };
        return Some(Probe {
            shell,
            platform: format!("Windows {}", words.get(i + 1)?),
        });
    }
    // sh: `uname -sm` follows the line of the echo (whatever a login script printed before it)
    let mut lines = out.lines().skip_while(|l| !l.starts_with("%OS%")).skip(1);
    let uname = lines.find(|l| !l.trim().is_empty())?.trim();
    Some(Probe {
        shell: Shell::Sh,
        platform: uname.into(),
    })
}

/// Where the shell the last probe found is kept: next to the index caches, a file per host and
/// ssh command, none for `sh`.
fn cache_file(cfg: &Config) -> PathBuf {
    let key = blake3::hash(format!("{}\n{}", cfg.ssh.join(" "), cfg.host).as_bytes()).to_hex();
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mirako")
        .join(format!("{}.host", &key[..16]))
}

/// The shell the last probe of the host found, `sh` when none did.
pub fn known(cfg: &Config) -> Shell {
    read_shell(&cache_file(cfg))
}

pub fn remember(cfg: &Config, shell: Shell) {
    write_shell(&cache_file(cfg), shell)
}

fn read_shell(path: &Path) -> Shell {
    match fs::read_to_string(path).as_deref().map(str::trim) {
        Ok("cmd") => Shell::Cmd,
        Ok("powershell") => Shell::PowerShell,
        _ => Shell::Sh,
    }
}

/// Best effort: without it the next run probes again.
fn write_shell(path: &Path, shell: Shell) {
    let _ = match shell {
        Shell::Sh => fs::remove_file(path),
        Shell::Cmd | Shell::PowerShell => fs::create_dir_all(path.parent().unwrap()).and_then(|()| fs::write(path, shell.name())),
    };
}

impl Shell {
    pub fn name(self) -> &'static str {
        match self {
            Shell::Sh => "sh",
            Shell::Cmd => "cmd",
            Shell::PowerShell => "powershell",
        }
    }

    pub fn windows(self) -> bool {
        self != Shell::Sh
    }

    /// `<remote_bin> serve` in this shell's words, which exit 127 when there is no agent at
    /// `remote_bin`, as sh does: `Session::connect` installs one then.
    pub fn serve(self, remote_bin: &str) -> String {
        match self {
            Shell::Sh => format!("{remote_bin} serve"),
            // starts with `if`, not a quote: `cmd /c` strips the first and last quote of a line that
            // begins with one
            Shell::Cmd => {
                let bin = cmd_path(remote_bin);
                format!("if exist {bin} ({bin} serve) else exit 127")
            }
            Shell::PowerShell => format!(
                "$m = {}; if (Test-Path $m) {{ & $m serve; exit $LASTEXITCODE }} else {{ exit 127 }}",
                ps_path(remote_bin)
            ),
        }
    }

    /// The script that puts the agent at `remote_bin`, its bytes coming from `source`, and then
    /// runs its `--version`. Exit 3: the host has no cargo (`Source::Cargo`); 4: no curl (`Source::Url`, sh).
    pub fn install_script(self, remote_bin: &str, source: Source) -> String {
        if self.windows() {
            powershell(&ps_install(remote_bin, source))
        } else {
            sh_install(remote_bin, source)
        }
    }
}

/// Where the agent an install script puts on the host comes from.
#[derive(Clone, Copy)]
pub enum Source<'a> {
    /// stdin, which the client feeds a copy of its own binary
    Stdin,
    /// a download: the release binary of this version for the host's platform
    Url(&'a str),
    /// `cargo install` of this version's release tag, on the host
    Cargo,
}

fn sh_install(dest: &str, source: Source) -> String {
    let version = proto::VERSION;
    let (check, fetch) = match source {
        Source::Stdin => (String::new(), format!("cat > {dest}.tmp")),
        Source::Url(url) => (
            "command -v curl >/dev/null || exit 4; ".to_string(),
            format!("curl -fsSL -o {dest}.tmp {url}"),
        ),
        Source::Cargo => (
            "C=$(command -v cargo || echo \"$HOME/.cargo/bin/cargo\"); [ -x \"$C\" ] || exit 3; ".to_string(),
            format!(
                "\"$C\" install --git {REPO} --tag v{version} --force --root \"$HOME/.mirako-build\" && mv \"$HOME/.mirako-build/bin/mirako\" {dest}.tmp"
            ),
        ),
    };
    format!("{check}mkdir -p \"$(dirname {dest})\" && {fetch} && chmod +x {dest}.tmp && mv {dest}.tmp {dest} && {dest} --version")
}

fn ps_install(remote_bin: &str, source: Source) -> String {
    let version = proto::VERSION;
    let fetch = match source {
        Source::Stdin => "$in = [Console]::OpenStandardInput()\n\
             $out = [IO.File]::Create($tmp)\n\
             try { $in.CopyTo($out) } finally { $out.Close() }"
            .to_string(),
        Source::Url(url) => format!(
            "[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12\n\
             Invoke-WebRequest -UseBasicParsing -Uri '{url}' -OutFile $tmp -ErrorAction Stop"
        ),
        Source::Cargo => format!(
            "$c = Get-Command cargo -ErrorAction SilentlyContinue\n\
             $cargo = if ($c) {{ $c.Source }} else {{ Join-Path $env:USERPROFILE '.cargo\\bin\\cargo.exe' }}\n\
             if (-not (Test-Path $cargo)) {{ exit 3 }}\n\
             $root = Join-Path $env:USERPROFILE '.mirako-build'\n\
             & $cargo install --git '{REPO}' --tag 'v{version}' --force --root $root\n\
             if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}\n\
             Move-Item -Force (Join-Path $root 'bin\\mirako.exe') $tmp -ErrorAction Stop"
        ),
    };
    // errors stop the script through the trap: `$ErrorActionPreference = 'Stop'` would take the
    // stderr of cargo for some, which Windows PowerShell does when its own stderr is a pipe
    format!(
        "trap {{ [Console]::Error.WriteLine(\"mirako install: $_\"); exit 1 }}\n\
         $ProgressPreference = 'SilentlyContinue'\n\
         $dest = {dest}\n\
         $tmp = \"$dest.tmp\"\n\
         New-Item -ItemType Directory -Force -Path (Split-Path -Parent $dest) -ErrorAction Stop | Out-Null\n\
         {fetch}\n\
         # an agent that is running cannot be replaced, only renamed: it moves aside, and goes once it has ended\n\
         Get-ChildItem -Path \"$dest.old-*\" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue\n\
         if (Test-Path $dest) {{ Move-Item -Force $dest (\"$dest.old-\" + [guid]::NewGuid().ToString('N')) -ErrorAction Stop }}\n\
         Move-Item -Force $tmp $dest -ErrorAction Stop\n\
         & $dest --version\n\
         exit $LASTEXITCODE\n",
        dest = ps_path(remote_bin)
    )
}

/// `remote_bin` on a Windows host: `~/` is the user's profile, `/` is `\`, and a name without an
/// extension is the `.exe` of that name. `(under the profile, path)`.
fn windows_bin(remote_bin: &str) -> (bool, String) {
    let (home, rest) = match remote_bin.strip_prefix("~/") {
        Some(rest) => (true, rest),
        None => (false, remote_bin),
    };
    let mut path = rest.replace('/', "\\");
    if !path.rsplit('\\').next().unwrap_or_default().contains('.') {
        path.push_str(".exe");
    }
    (home, path)
}

/// `remote_bin` for cmd, quoted: `"%USERPROFILE%\.local\bin\mirako.exe"`.
pub fn cmd_path(remote_bin: &str) -> String {
    match windows_bin(remote_bin) {
        (true, rest) => format!("\"%USERPROFILE%\\{rest}\""),
        (false, path) => format!("\"{path}\""),
    }
}

/// `remote_bin` as a PowerShell expression without a double quote: the command line a
/// `DefaultShell` PowerShell gets is split by Windows' rules first, and those take double quotes.
pub fn ps_path(remote_bin: &str) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    match windows_bin(remote_bin) {
        (true, rest) => format!("(Join-Path $env:USERPROFILE {})", quote(&rest)),
        (false, path) => quote(&path),
    }
}

/// A PowerShell script as a command line that cmd and PowerShell both run as it is: the script
/// goes as the base64 of its UTF-16 text, which neither of them quotes or expands. With
/// `-InputFormat None` PowerShell leaves stdin alone, to the script (the upload of the agent).
pub fn powershell(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!(
        "powershell -NoLogo -NoProfile -NonInteractive -InputFormat None -EncodedCommand {}",
        base64(&utf16)
    )
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                ABC[((n >> (18 - 6 * i)) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// This binary's platform, spelled like a host's in `Probe`: a host that matches gets a copy of it.
pub fn this_platform() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => format!("Darwin {}", if arch == "aarch64" { "arm64" } else { arch }),
        "linux" => format!("Linux {arch}"),
        "windows" => format!(
            "Windows {}",
            match arch {
                "x86_64" => "AMD64",
                "aarch64" => "ARM64",
                other => other,
            }
        ),
        os => format!("{os} {arch}"),
    }
}

/// The binary of this version that CI releases for `platform` (the `build` job of ci.yml).
pub fn release_url(platform: &str) -> Option<String> {
    let name = match platform {
        "Darwin arm64" => "macos-arm64",
        "Linux x86_64" => "linux-x86_64",
        "Linux aarch64" => "linux-arm64",
        "Windows AMD64" => "windows-x86_64.exe",
        _ => return None,
    };
    let v = proto::VERSION;
    Some(format!("{REPO}/releases/download/v{v}/mirako-v{v}-{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probed(shell: Shell, platform: &str) -> Option<Probe> {
        Some(Probe {
            shell,
            platform: platform.into(),
        })
    }

    #[test]
    fn the_probe_tells_the_three_shells_apart() {
        assert_eq!(
            parse_probe("%OS% %PROCESSOR_ARCHITECTURE% :OS :PROCESSOR_ARCHITECTURE\nDarwin arm64\n"),
            probed(Shell::Sh, "Darwin arm64")
        );
        assert_eq!(
            parse_probe("Windows_NT AMD64 $env:OS $env:PROCESSOR_ARCHITECTURE; uname -sm\r\n"),
            probed(Shell::Cmd, "Windows AMD64")
        );
        assert_eq!(
            parse_probe("%OS%\r\n%PROCESSOR_ARCHITECTURE%\r\nWindows_NT\r\nARM64\r\n"),
            probed(Shell::PowerShell, "Windows ARM64")
        );
        // what a login script prints first is skipped
        assert_eq!(
            parse_probe("welcome\n%OS% %PROCESSOR_ARCHITECTURE% :OS :PROCESSOR_ARCHITECTURE\n\nLinux x86_64\n"),
            probed(Shell::Sh, "Linux x86_64")
        );
        // no uname, or nothing at all: unknown
        assert_eq!(parse_probe("%OS% %PROCESSOR_ARCHITECTURE% :OS :PROCESSOR_ARCHITECTURE\n"), None);
        assert_eq!(parse_probe(""), None);
        assert_eq!(parse_probe("Windows_NT"), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_probe_line_run_by_sh_gives_unames_answer_and_this_platform_spells_it() {
        let out = std::process::Command::new("sh").arg("-c").arg(PROBE).output().unwrap();
        let p = parse_probe(&String::from_utf8_lossy(&out.stdout)).unwrap();
        let uname = std::process::Command::new("uname").arg("-sm").output().unwrap();
        assert_eq!(p, probed(Shell::Sh, String::from_utf8_lossy(&uname.stdout).trim()).unwrap());
        // a binary built for this machine, as the tests are
        assert_eq!(p.platform, this_platform());
    }

    #[test]
    fn serve_is_spelled_for_each_shell() {
        assert_eq!(Shell::Sh.serve("~/.local/bin/mirako"), "~/.local/bin/mirako serve");
        assert_eq!(
            Shell::Cmd.serve("~/.local/bin/mirako"),
            r#"if exist "%USERPROFILE%\.local\bin\mirako.exe" ("%USERPROFILE%\.local\bin\mirako.exe" serve) else exit 127"#
        );
        assert_eq!(
            Shell::PowerShell.serve("~/.local/bin/mirako"),
            r"$m = (Join-Path $env:USERPROFILE '.local\bin\mirako.exe'); if (Test-Path $m) { & $m serve; exit $LASTEXITCODE } else { exit 127 }"
        );
        assert!(!Shell::PowerShell.serve("C:/Program Files/it's/mirako").contains('"'));
    }

    #[test]
    fn a_windows_remote_bin_gets_backslashes_its_profile_and_an_exe() {
        assert_eq!(cmd_path("~/bin/mirako"), r#""%USERPROFILE%\bin\mirako.exe""#);
        assert_eq!(cmd_path("C:/tools/mirako.exe"), r#""C:\tools\mirako.exe""#);
        assert_eq!(cmd_path(r"D:\m\mirako"), r#""D:\m\mirako.exe""#);
        assert_eq!(
            ps_path("~/.local/bin/mirako"),
            r"(Join-Path $env:USERPROFILE '.local\bin\mirako.exe')"
        );
        assert_eq!(ps_path("C:/Program Files/it's/mirako"), r"'C:\Program Files\it''s\mirako.exe'");
    }

    #[test]
    fn base64_pads_and_powershell_gets_utf16() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), coded);
        }
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
        assert_eq!(
            powershell("exit 0"),
            "powershell -NoLogo -NoProfile -NonInteractive -InputFormat None -EncodedCommand ZQB4AGkAdAAgADAA"
        );
    }

    #[test]
    fn the_sh_install_scripts_end_with_the_version_of_what_they_put_there() {
        let v = proto::VERSION;
        let copy = Shell::Sh.install_script("~/.local/bin/mirako", Source::Stdin);
        assert_eq!(
            copy,
            "mkdir -p \"$(dirname ~/.local/bin/mirako)\" && cat > ~/.local/bin/mirako.tmp && chmod +x ~/.local/bin/mirako.tmp && mv ~/.local/bin/mirako.tmp ~/.local/bin/mirako && ~/.local/bin/mirako --version"
        );
        let get = Shell::Sh.install_script("~/m", Source::Url("https://x/m"));
        assert!(get.starts_with("command -v curl >/dev/null || exit 4; "), "{get}");
        assert!(get.contains("curl -fsSL -o ~/m.tmp https://x/m && chmod +x"), "{get}");
        let build = Shell::Sh.install_script("~/m", Source::Cargo);
        assert!(build.contains("|| exit 3; mkdir"), "{build}");
        assert!(build.contains(&format!("install --git {REPO} --tag v{v} --force")), "{build}");
        for s in [copy, get, build] {
            assert!(s.ends_with(" --version"), "{s}");
        }
    }

    #[test]
    fn the_windows_install_scripts_are_powershell_and_set_the_agent_aside_before_replacing_it() {
        let v = proto::VERSION;
        for source in [Source::Stdin, Source::Url("https://x/m.exe"), Source::Cargo] {
            let s = ps_install("~/.local/bin/mirako", source);
            assert!(s.starts_with("trap { "), "{s}");
            assert!(
                s.contains("$dest = (Join-Path $env:USERPROFILE '.local\\bin\\mirako.exe')\n"),
                "{s}"
            );
            let aside = s.find("Move-Item -Force $dest").unwrap();
            assert!(aside < s.find("Move-Item -Force $tmp $dest").unwrap(), "{s}");
            assert!(s.ends_with("& $dest --version\nexit $LASTEXITCODE\n"), "{s}");
            assert!(Shell::Cmd.install_script("~/m", source).starts_with("powershell "));
            assert_eq!(
                Shell::Cmd.install_script("~/m", source),
                Shell::PowerShell.install_script("~/m", source)
            );
        }
        assert!(ps_install("~/m", Source::Stdin).contains("[Console]::OpenStandardInput()"));
        assert!(ps_install("~/m", Source::Url("https://x/m.exe")).contains("-Uri 'https://x/m.exe' -OutFile $tmp"));
        let build = ps_install("~/m", Source::Cargo);
        assert!(build.contains("{ exit 3 }"), "{build}");
        assert!(build.contains(&format!("install --git '{REPO}' --tag 'v{v}' --force")), "{build}");
    }

    #[test]
    fn the_shell_of_a_host_is_remembered_and_sh_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/h.host");
        assert_eq!(read_shell(&path), Shell::Sh);
        for shell in [Shell::Cmd, Shell::PowerShell, Shell::Sh] {
            write_shell(&path, shell);
            assert_eq!(read_shell(&path), shell);
        }
        assert!(!path.exists(), "sh is the absence of the file");
    }

    #[test]
    fn release_urls_name_the_assets_ci_builds() {
        let v = proto::VERSION;
        assert_eq!(
            release_url("Linux x86_64").unwrap(),
            format!("{REPO}/releases/download/v{v}/mirako-v{v}-linux-x86_64")
        );
        assert!(release_url("Linux aarch64").unwrap().ends_with("-linux-arm64"));
        assert!(release_url("Darwin arm64").unwrap().ends_with("-macos-arm64"));
        assert!(release_url("Windows AMD64").unwrap().ends_with("-windows-x86_64.exe"));
        assert_eq!(release_url("Darwin x86_64"), None);
        assert_eq!(release_url("Windows ARM64"), None);
    }
}
