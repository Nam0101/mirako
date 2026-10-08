//! End-to-end tests of the real binary over the real protocol, without ssh ("loopback"): the
//! project's `mirako.toml` sets `ssh = ["sh", "-c", "exec <bin> serve"]`, so the client starts the
//! agent as a local child and the ssh arguments it appends become `$0..$n` of the script.
//!
//! Hermetic: every child runs with `HOME` and `GRADLE_USER_HOME` inside the test's scratch dir, so
//! the global config, both index caches and the Gradle retention script never touch the real ones.

#![cfg(unix)] // the loopback "ssh" is `sh`, and the sandbox is `HOME`

use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_mirako");

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// A scratch dir holding a fake `home`, the shared `remote` folder and any number of projects.
struct Scratch {
    _dir: TempDir,
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // canonical, or the remote path the client rewrites (/var/…) differs from the agent's $PWD (/private/var/…)
        let path = dir.path().canonicalize().unwrap();
        fs::create_dir_all(path.join("home")).unwrap();
        Self { _dir: dir, path }
    }

    fn home(&self) -> PathBuf {
        self.path.join("home")
    }

    fn remote_folder(&self) -> PathBuf {
        self.path.join("remote")
    }

    fn gradle_home(&self) -> PathBuf {
        self.path.join("gradle")
    }

    /// The agent's index cache for the mirror `canon` (`Index::cache_path` under the fake HOME).
    fn agent_idx(&self, canon: &Path) -> PathBuf {
        let key = blake3::hash(canon.to_string_lossy().as_bytes()).to_hex();
        let cache = if cfg!(target_os = "macos") {
            self.home().join("Library/Caches")
        } else {
            self.home().join(".cache")
        };
        cache.join("mirako").join(format!("{}.idx", &key[..16]))
    }

    /// A project `<scratch>/<name>` with the loopback `mirako.toml` plus `extra` lines.
    fn project(&self, name: &str, extra: &str) -> Proj<'_> {
        let root = self.path.join(name);
        fs::create_dir_all(&root).unwrap();
        let p = Proj {
            scratch: self,
            remote: self.remote_folder().join(name),
            root,
        };
        p.config(extra);
        p
    }

    /// `mirako <args…>` inside the sandbox, not started yet.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .current_dir(&self.path)
            .env("HOME", self.home())
            .env("GRADLE_USER_HOME", self.gradle_home())
            // on Linux the cache dir follows this one before `HOME`
            .env_remove("XDG_CACHE_HOME")
            .env_remove("MIRAKO_LOCAL")
            .env_remove("MIRAKO_REMOTE")
            .env_remove("PWD");
        cmd
    }

    fn mirako(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
}

struct Proj<'a> {
    scratch: &'a Scratch,
    root: PathBuf,
    /// the mirror on the "remote": `remote_folder/<name of root>`
    remote: PathBuf,
}

impl Proj<'_> {
    /// Rewrites `mirako.toml`; a key in `extra` replaces the default one of the same name.
    fn config(&self, extra: &str) {
        let defaults = [
            ("host", "\"loop\"".to_string()),
            ("remote_folder", format!("{:?}", self.scratch.remote_folder().to_str().unwrap())),
            ("remote_bin", format!("{BIN:?}")),
            ("ssh", format!("[\"sh\", \"-c\", {:?}]", format!("exec {BIN} serve"))),
            ("fallback", "false".into()),
            ("gc_days", "0".into()),
            ("gc_after_pull", "[]".into()),
            ("exclude_local", "[\"build\"]".into()),
            ("exclude_remote", "[\"src\"]".into()),
            ("exclude_common", "[\".git\", \"mirako.toml\", \".DS_Store\"]".into()),
        ];
        let overridden = |k: &str| extra.lines().any(|l| l.split('=').next().map(str::trim) == Some(k));
        let mut text: String = defaults
            .iter()
            .filter(|(k, _)| !overridden(k))
            .map(|(k, v)| format!("{k} = {v}\n"))
            .collect();
        text.push_str(extra);
        text.push('\n');
        fs::write(self.root.join("mirako.toml"), text).unwrap();
    }

    fn mirako(&self, args: &[&str]) -> Output {
        self.scratch.mirako(args)
    }

    /// `mirako <sub> --project <root> [rest…]`
    fn sub(&self, sub: &str, rest: &[&str]) -> Output {
        let root = self.root.to_str().unwrap();
        let mut args = vec![sub, "--project", root];
        args.extend_from_slice(rest);
        self.mirako(&args)
    }

    /// `mirako run --project <root> [flags…] -- <cmd…>`
    fn run(&self, flags: &[&str], cmd: &[&str]) -> Output {
        let mut rest = flags.to_vec();
        rest.push("--");
        rest.extend_from_slice(cmd);
        self.sub("run", &rest)
    }

    /// `run` that must exit 0; returns stdout.
    fn run_ok(&self, flags: &[&str], cmd: &[&str]) -> String {
        let out = self.run(flags, cmd);
        assert_eq!(out.status.code(), Some(0), "{}", show(&out));
        stdout(&out)
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        write(&self.root, rel, bytes)
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn show(o: &Output) -> String {
    format!("status {:?}\n--- stdout\n{}--- stderr\n{}", o.status, stdout(o), stderr(o))
}

/// The summary line starting with `prefix` (`push`, `pull`, `exec`, `gc`, `total`).
fn line<'a>(out: &'a str, prefix: &str) -> &'a str {
    out.lines()
        .find(|l| l.split_whitespace().next() == Some(prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` line in\n{out}"))
}

/// `push   {files} files ({deltas} as delta), {bytes} ({wire} on the wire), …` → (files, deltas, wire);
/// a line that names no files (`push   up to date, …`) counts none
fn counts(l: &str) -> (usize, usize, String) {
    let w: Vec<&str> = l.split_whitespace().collect();
    if !w[2].starts_with("file") {
        return (0, 0, String::new());
    }
    let deltas = if w[4] == "as" {
        w[3].trim_start_matches('(').parse().unwrap()
    } else {
        0
    };
    let on = w.iter().position(|s| *s == "on").unwrap_or_else(|| panic!("{l}"));
    (
        w[1].parse().unwrap(),
        deltas,
        format!("{} {}", w[on - 2].trim_start_matches('('), w[on - 1]),
    )
}

#[test]
fn cold_run_pushes_sources_runs_the_command_and_pulls_outputs() {
    let s = Scratch::new();
    let p = s.project("app", "");
    p.write("src/a.txt", b"alpha\n");
    p.write("src/sub/b.txt", b"beta\n");
    p.write("build/old.bin", b"stale");
    p.write("gradlew", b"#!/bin/sh\n");
    fs::set_permissions(p.root.join("gradlew"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("a.txt", p.root.join("src/link")).unwrap();

    let out = p.run_ok(
        &[],
        &[
            "sh",
            "-c",
            "mkdir -p build/outputs && cat src/a.txt src/sub/b.txt > build/outputs/out.txt && echo \"built in $PWD\"",
        ],
    );
    assert!(out.contains(&format!("built in {}\n", p.root.display())), "{out}");
    assert!(!out.contains(p.remote.to_str().unwrap()), "remote path not rewritten:\n{out}");
    for prefix in ["push", "pull", "total"] {
        line(&out, prefix);
    }
    assert!(line(&out, "exec").starts_with("exec   exit 0"), "{out}");

    assert_eq!(read(&p.remote.join("src/a.txt")), b"alpha\n");
    assert_eq!(read(&p.remote.join("src/sub/b.txt")), b"beta\n");
    assert_eq!(fs::read_link(p.remote.join("src/link")).unwrap(), Path::new("a.txt"));
    let mode = fs::metadata(p.remote.join("gradlew")).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o755);
    assert!(!p.remote.join("build/old.bin").exists());
    assert!(!p.remote.join("mirako.toml").exists());

    assert_eq!(read(&p.root.join("build/outputs/out.txt")), b"alpha\nbeta\n");
    assert_eq!(read(&p.root.join("build/old.bin")), b"stale", "pull never deletes locally");
}

#[test]
fn warm_run_sends_nothing() {
    let s = Scratch::new();
    let p = s.project("app", "");
    p.write("src/a.txt", b"alpha\n");
    p.run_ok(&[], &["sh", "-c", "mkdir -p build && cp src/a.txt build/a.out"]);

    let out = p.run_ok(&[], &["true"]);
    assert!(line(&out, "push").starts_with("push   up to date, "), "{out}");
    assert!(line(&out, "pull").starts_with("pull   up to date, "), "{out}");
}

#[test]
fn changes_and_deletes_are_mirrored_and_the_exit_code_propagates() {
    let s = Scratch::new();
    let p = s.project("app", "");
    p.write("src/a.txt", b"alpha\n");
    p.write("src/sub/b.txt", b"beta\n");
    p.run_ok(&[], &["true"]);

    p.write("src/a.txt", b"alpha v2\n");
    fs::remove_file(p.root.join("src/sub/b.txt")).unwrap();
    p.write("src/c.txt", b"gamma\n");
    let o = p.run(&[], &["sh", "-c", "exit 3"]);
    assert_eq!(o.status.code(), Some(3), "{}", show(&o));
    let out = stdout(&o);
    let push = line(&out, "push");
    assert_eq!(counts(push).0, 2, "{out}");
    assert!(push.contains(", 1 deleted"), "{out}");
    assert!(line(&out, "exec").starts_with("exec   exit 3"), "{out}");

    assert_eq!(read(&p.remote.join("src/a.txt")), b"alpha v2\n");
    assert_eq!(read(&p.remote.join("src/c.txt")), b"gamma\n");
    assert!(!p.remote.join("src/sub/b.txt").exists());
}

#[test]
fn big_changed_file_goes_as_a_delta() {
    let s = Scratch::new();
    let p = s.project("app", "");
    let mut big = noise(2 << 20, 7);
    p.write("src/big.bin", &big);
    p.run_ok(&[], &["true"]);

    big[1 << 20..(1 << 20) + 1024].copy_from_slice(&noise(1024, 99));
    p.write("src/big.bin", &big);
    let out = p.run_ok(&[], &["true"]);
    let (files, deltas, wire) = counts(line(&out, "push"));
    assert_eq!((files, deltas), (1, 1), "{out}");
    assert!(
        wire.ends_with(" KB") || wire.ends_with(" B"),
        "wire {wire} is not under 1 MB:\n{out}"
    );
    if let Some(kb) = wire.strip_suffix(" KB") {
        assert!(kb.parse::<u64>().unwrap() < 500, "{out}");
    }
    assert!(read(&p.remote.join("src/big.bin")) == big, "remote copy differs");
}

#[test]
fn pull_uses_a_delta_when_the_local_copy_is_close() {
    let s = Scratch::new();
    let p = s.project("app", "");
    let seed = noise(2 << 20, 3);
    let mut seed2 = seed.clone();
    seed2[1 << 20..(1 << 20) + 1024].copy_from_slice(&noise(1024, 5));
    p.write("src/seed.bin", &seed);
    p.write("src/seed2.bin", &seed2);

    let out = p.run_ok(&[], &["sh", "-c", "mkdir -p build && cp src/seed.bin build/out.bin"]);
    assert_eq!(counts(line(&out, "pull")).0, 1, "{out}");
    assert!(read(&p.root.join("build/out.bin")) == seed);

    let out = p.run_ok(&[], &["sh", "-c", "cp src/seed2.bin build/out.bin"]);
    let (files, deltas, wire) = counts(line(&out, "pull"));
    assert_eq!((files, deltas), (1, 1), "{out}");
    assert!(wire.ends_with(" KB") || wire.ends_with(" B"), "{out}");
    assert!(read(&p.root.join("build/out.bin")) == seed2, "local copy differs");
}

#[test]
fn no_push_and_no_pull_flags() {
    let s = Scratch::new();
    let p = s.project("app", "");
    p.write("src/a.txt", b"v1");
    p.run_ok(&[], &["true"]);

    p.write("src/a.txt", b"v2");
    let out = p.run_ok(&["--no-push"], &["sh", "-c", "mkdir -p build && cp src/a.txt build/seen.txt"]);
    assert!(!out.lines().any(|l| l.starts_with("push")), "{out}");
    assert_eq!(read(&p.remote.join("src/a.txt")), b"v1", "--no-push uploaded");
    assert_eq!(read(&p.root.join("build/seen.txt")), b"v1");

    let out = p.run_ok(
        &["--no-pull"],
        &["sh", "-c", "cp src/a.txt build/seen.txt && echo x > build/new.txt"],
    );
    assert!(!out.lines().any(|l| l.starts_with("pull")), "{out}");
    assert_eq!(read(&p.remote.join("src/a.txt")), b"v2");
    assert_eq!(read(&p.root.join("build/seen.txt")), b"v1", "--no-pull downloaded");
    assert!(!p.root.join("build/new.txt").exists());

    p.write("src/a.txt", b"v3");
    let o = p.sub("push", &[]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    assert!(!stdout(&o).lines().any(|l| l.starts_with("pull")), "{}", show(&o));
    assert_eq!(read(&p.remote.join("src/a.txt")), b"v3");
    assert!(!p.root.join("build/new.txt").exists());

    p.write("src/a.txt", b"v4");
    let o = p.sub("pull", &[]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    assert!(!stdout(&o).lines().any(|l| l.starts_with("push")), "{}", show(&o));
    assert_eq!(read(&p.remote.join("src/a.txt")), b"v3", "pull uploaded");
    assert_eq!(read(&p.root.join("build/new.txt")), b"x\n");
    assert_eq!(read(&p.root.join("build/seen.txt")), b"v2");
}

fn backdate(path: &Path, by: Duration) {
    let t = filetime::FileTime::from_system_time(SystemTime::now() - by);
    filetime::set_file_mtime(path, t).unwrap();
}

#[test]
fn gc_lists_and_removes_stale_mirrors() {
    let s = Scratch::new();
    let alpha = s.project("alpha", "");
    let beta = s.project("beta", "");
    for p in [&alpha, &beta] {
        p.write("src/a.txt", b"a");
        p.run_ok(&[], &["true"]);
    }
    fs::create_dir_all(s.remote_folder().join("stray")).unwrap();
    let alpha_idx = s.agent_idx(&alpha.remote.canonicalize().unwrap());
    assert!(alpha_idx.exists(), "no agent index at {}", alpha_idx.display());
    backdate(&alpha_idx, Duration::from_secs(10 * 86_400));

    let o = beta.sub("gc", &["--dry-run", "--days", "7"]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    let out = stdout(&o);
    assert!(line(&out, "alpha").ends_with("unused 10 d, would be removed"), "{out}");
    assert!(line(&out, "beta").ends_with("used today"), "{out}");
    assert!(line(&out, "stray").ends_with("not synced by mirako, left alone"), "{out}");
    assert!(alpha.remote.exists() && beta.remote.exists());

    let o = beta.sub("gc", &["--days", "7"]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    let out = stdout(&o);
    assert!(line(&out, "alpha").ends_with("unused 10 d, removed"), "{out}");
    assert!(!alpha.remote.exists(), "{out}");
    assert!(!alpha_idx.exists(), "the removed mirror's index stays behind");
    assert!(beta.remote.exists());
    assert!(s.remote_folder().join("stray").exists());

    // `--days 0` removes every copy idle for at least a whole second: the check is `idle > 0 days`
    // in whole seconds, so one synced in the same second as the gc survives
    let beta_idx = s.agent_idx(&beta.remote.canonicalize().unwrap());
    backdate(&beta_idx, Duration::from_secs(5));
    let o = beta.sub("gc", &["--days", "0"]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    let out = stdout(&o);
    assert!(line(&out, "beta").ends_with("unused 0 d, removed"), "{out}");
    assert!(!beta.remote.exists());
    assert!(line(&out, "stray").ends_with("left alone"), "{out}");
    assert!(s.remote_folder().join("stray").exists());

    beta.run_ok(&[], &["true"]);
    let o = beta.sub("gc", &["--days", "0"]);
    let out = stdout(&o);
    // a copy synced moments ago is idle for 0 s; kept unless the second ticked over in between
    if !line(&out, "beta").ends_with("removed") {
        assert!(line(&out, "beta").ends_with("used today"), "{out}");
        assert!(beta.remote.exists());
    }
}

#[test]
fn gc_after_pull_deletes_intermediates_on_the_remote() {
    let s = Scratch::new();
    let p = s.project("app", "gc_after_pull = [\"build/intermediates\", \"!build/intermediates/keep\"]");
    p.write("src/a.txt", b"a");
    let out = p.run_ok(
        &[],
        &[
            "sh",
            "-c",
            "mkdir -p build/intermediates/keep build/outputs && head -c 5000 /dev/zero > build/intermediates/junk.bin \
             && echo k > build/intermediates/keep/k.txt && echo o > build/outputs/o.txt",
        ],
    );
    assert!(line(&out, "gc").contains("of intermediates deleted"), "{out}");
    assert!(!p.remote.join("build/intermediates/junk.bin").exists());
    assert!(p.remote.join("build/intermediates/keep/k.txt").exists());
    assert!(p.remote.join("build/outputs/o.txt").exists());
    assert_eq!(read(&p.root.join("build/intermediates/junk.bin")).len(), 5000);
    assert_eq!(read(&p.root.join("build/intermediates/keep/k.txt")), b"k\n");
    assert_eq!(read(&p.root.join("build/outputs/o.txt")), b"o\n");
}

#[test]
fn gc_days_writes_the_gradle_retention_script_on_the_host() {
    let s = Scratch::new();
    fs::create_dir_all(s.gradle_home()).unwrap();
    let script = s.gradle_home().join("init.d/mirako-gc.gradle");
    let p = s.project("app", "gc_days = 5");
    p.write("src/a.txt", b"a");
    p.run_ok(&[], &["true"]);
    let text = String::from_utf8(read(&script)).unwrap();
    assert!(text.contains("setRemoveUnusedEntriesAfterDays(5)"), "{text}");

    // `mirako gc` with `gc_days = 0` takes it away again
    p.config("gc_days = 0");
    let o = p.sub("gc", &[]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    assert!(!script.exists());
}

/// README: the script is "removed again with `gc_days = 0`": the `Gc` request still rides behind
/// the `Pull` of such a run, and nothing else in it applies.
#[test]
fn a_run_with_gc_days_0_removes_the_gradle_retention_script() {
    let s = Scratch::new();
    fs::create_dir_all(s.gradle_home()).unwrap();
    let script = s.gradle_home().join("init.d/mirako-gc.gradle");
    let p = s.project("app", "gc_days = 5");
    p.write("src/a.txt", b"a");
    p.run_ok(&[], &["true"]);
    assert!(script.exists());
    p.config("gc_days = 0");
    p.run_ok(&[], &["true"]);
    assert!(!script.exists(), "still there after a run with gc_days = 0");
}

#[test]
fn fallback_runs_locally_when_the_host_is_unreachable() {
    let s = Scratch::new();
    let p = s.project("app", "ssh = [\"sh\", \"-c\", \"exit 1\"]\nfallback = true");
    let cmd = ["sh", "-c", "echo local=$MIRAKO_LOCAL; pwd"];
    let o = p.run(&[], &cmd);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    assert!(stdout(&o).contains("local=1\n"), "{}", show(&o));
    assert!(stdout(&o).contains(&format!("{}\n", p.root.display())), "{}", show(&o));
    assert!(stderr(&o).contains("running locally"), "{}", show(&o));

    p.config("ssh = [\"sh\", \"-c\", \"exit 1\"]\nfallback = false");
    let o = p.run(&[], &cmd);
    assert_eq!(o.status.code(), Some(2), "{}", show(&o));
    assert!(stderr(&o).contains("no answer from"), "{}", show(&o));
    assert!(!stdout(&o).contains("local=1"), "{}", show(&o));
}

#[test]
fn check_reports_the_agent_version() {
    let s = Scratch::new();
    let p = s.project("app", "");
    let o = p.sub("check", &[]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    assert!(stdout(&o).contains(" ok ("), "{}", show(&o));

    p.config("ssh = [\"sh\", \"-c\", \"exec /nonexistent/mirako serve\"]");
    let o = p.sub("check", &[]);
    assert_ne!(o.status.code(), Some(0), "{}", show(&o));
    assert!(stderr(&o).contains("remote-install"), "{}", show(&o));
}

#[test]
fn exec_output_streams_stderr_and_stdout_separately() {
    let s = Scratch::new();
    let p = s.project("app", "");
    let o = p.run(&[], &["sh", "-c", "echo out; echo err 1>&2"]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    assert!(stdout(&o).lines().any(|l| l == "out"), "{}", show(&o));
    assert!(!stdout(&o).lines().any(|l| l == "err"), "{}", show(&o));
    assert!(stderr(&o).lines().any(|l| l == "err"), "{}", show(&o));
    assert!(!stderr(&o).lines().any(|l| l == "out"), "{}", show(&o));
}

#[test]
fn local_properties_goes_up_without_its_machine_paths_and_never_comes_back() {
    let s = Scratch::new();
    let p = s.project("app", "");
    let local = b"sdk.dir=/Users/me/Library/Android/sdk\nMAPS_API_KEY=abc\n";
    p.write("local.properties", local);
    p.write("src/a.txt", b"alpha\n");

    let out = p.run_ok(&[], &["cat", "local.properties"]);
    assert!(out.lines().any(|l| l == "MAPS_API_KEY=abc"), "{out}");
    assert_eq!(read(&p.remote.join("local.properties")), b"MAPS_API_KEY=abc\n");
    assert_eq!(
        read(&p.root.join("local.properties")),
        local,
        "the host's copy came back over the local one"
    );

    // the host's copy counts as in sync with the local one
    let out = p.run_ok(&[], &["true"]);
    assert_eq!(counts(line(&out, "push")).0, 0, "{out}");
    assert_eq!(counts(line(&out, "pull")).0, 0, "{out}");
}

/// What the Gradle shim passes for a test run of the IDE: the init script that reports the tests is
/// written into the project, goes up although `.gradle` does not, and the command gets the flags that load it.
#[test]
fn test_events_uploads_the_init_script_and_passes_it_to_the_command() {
    let s = Scratch::new();
    let common = "exclude_common = [\".gradle\", \".git\", \"mirako.toml\"]";
    let p = s.project("app", common);
    p.write("src/a.txt", b"alpha\n");
    p.write(".gradle/9.8/fileHashes.bin", b"state of the local Gradle");
    // the appended flags are `$2…` of this script
    let cmd = ["sh", "-c", "echo \"args=$*\"; head -1 \"$3\"", "gradlew", ":app:test"];
    let args = "args=:app:test --init-script .gradle/mirako-test-events.gradle --no-configuration-cache";
    let first = "// .gradle/mirako-test-events.gradle";

    let out = p.run_ok(&["--test-events"], &cmd);
    assert!(out.lines().any(|l| l == args), "{out}");
    assert!(out.lines().any(|l| l.starts_with(first)), "{out}");
    let script = read(&p.root.join(".gradle/mirako-test-events.gradle"));
    assert!(script.starts_with(first.as_bytes()));
    assert_eq!(read(&p.remote.join(".gradle/mirako-test-events.gradle")), script);
    assert!(!p.remote.join(".gradle/9.8").exists(), "the rest of .gradle went up");

    // the script is sent once
    let out = p.run_ok(&["--test-events"], &cmd);
    assert_eq!(counts(line(&out, "push")).0, 0, "{out}");

    // a run that falls back finds it in the project itself
    p.config(&format!("{common}\nssh = [\"sh\", \"-c\", \"exit 1\"]\nfallback = true"));
    let out = p.run_ok(&["--test-events"], &cmd);
    assert!(out.lines().any(|l| l == args), "{out}");
    assert!(out.lines().any(|l| l.starts_with(first)), "{out}");
}

#[test]
fn only_the_variables_the_env_key_names_reach_the_remote_command() {
    let s = Scratch::new();
    // an "ssh" that, like the real one, hands none of the client's variables on to the agent
    let ssh = format!(
        "ssh = [\"env\", \"-u\", \"MK_ONE\", \"-u\", \"MK_PRE_X\", \"-u\", \"MK_OTHER\", \"sh\", \"-c\", {:?}]",
        format!("exec {BIN} serve")
    );
    let p = s.project("app", &format!("{ssh}\nenv = [\"MK_ONE\", \"MK_PRE_*\"]"));
    let script = "echo \"one=$MK_ONE pre=$MK_PRE_X other=$MK_OTHER remote=$MIRAKO_REMOTE\"";
    let o = s
        .command(&["run", "--project", p.root.to_str().unwrap(), "--", "sh", "-c", script])
        .env("MK_ONE", "1")
        .env("MK_PRE_X", "2")
        .env("MK_OTHER", "3")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    assert!(stdout(&o).lines().any(|l| l == "one=1 pre=2 other= remote=1"), "{}", show(&o));
}

#[test]
fn a_second_run_of_the_project_waits_for_the_first() {
    let s = Scratch::new();
    let p = s.project("app", "");
    p.write("src/a.txt", b"alpha\n");
    let log = p.remote.join("build/log");
    let first = "mkdir -p build && echo A-start >> build/log && sleep 1 && echo A-end >> build/log";
    thread::scope(|scope| {
        let a = scope.spawn(|| p.run(&[], &["sh", "-c", first]));
        // the first run has the project once its command is running
        let deadline = Instant::now() + Duration::from_secs(20);
        while !log.exists() {
            assert!(Instant::now() < deadline, "the first run never started its command");
            thread::sleep(Duration::from_millis(10));
        }
        let b = p.run(&[], &["sh", "-c", "echo B-start >> build/log"]);
        let a = a.join().unwrap();
        assert_eq!(a.status.code(), Some(0), "{}", show(&a));
        assert_eq!(b.status.code(), Some(0), "{}", show(&b));
        assert!(stderr(&b).contains("another run of this project"), "{}", show(&b));
    });
    assert_eq!(read(&log), b"A-start\nA-end\nB-start\n");
}

/// Runs `script` on the remote, kills the client once the script has printed `started`, and says
/// `wait` later whether the script got as far as its `touch finished`.
fn finishes_after_its_client_is_killed(script: &str, wait: Duration) -> bool {
    let s = Scratch::new();
    let p = s.project("app", "");
    p.write("src/a.txt", b"alpha\n");
    let mut client = s
        .command(&["run", "--project", p.root.to_str().unwrap(), "--", "sh", "-c", script])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // the command is running once its first line is here
    let mut lines = BufReader::new(client.stdout.take().unwrap()).lines();
    assert!(lines.any(|l| l.unwrap() == "started"));
    client.kill().unwrap();
    client.wait().unwrap();

    thread::sleep(wait);
    p.remote.join("finished").exists()
}

#[test]
fn killing_the_client_stops_the_command_on_the_remote() {
    // the marker is written by a child of the command's shell: its whole process group has to stop
    let script = "echo started; (sleep 1; touch finished) & wait";
    assert!(
        !finishes_after_its_client_is_killed(script, Duration::from_millis(1500)),
        "the command outlived its client"
    );
}

#[test]
fn killing_the_client_stops_what_the_command_left_running() {
    // the shell is gone at once; its child holds the output open, so the run is not over
    let script = "(sleep 1; touch finished) & echo started";
    assert!(
        !finishes_after_its_client_is_killed(script, Duration::from_millis(1500)),
        "the child outlived the client"
    );
}

#[test]
fn a_command_that_ignores_the_first_signal_is_killed_after_the_grace_period() {
    let script = "trap '' TERM; echo started; sleep 3; touch finished";
    assert!(
        !finishes_after_its_client_is_killed(script, Duration::from_millis(3500)),
        "SIGTERM was the last word"
    );
}

#[test]
fn a_project_without_host_fails_clearly() {
    let s = Scratch::new();
    let root = s.path.join("app");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("mirako.toml"), "fallback = false\n").unwrap();
    let o = s.mirako(&["run", "--project", root.to_str().unwrap(), "--", "true"]);
    assert_eq!(o.status.code(), Some(2), "{}", show(&o));
    assert!(stderr(&o).contains("no host configured"), "{}", show(&o));
}

/// A stand-in for `ssh` whose last argument is the remote command, like ssh's: `uname -sm` and
/// the install script run locally, `<bin> serve` is exec'd (exit 126/127 while `<bin>` is missing).
fn fake_ssh(s: &Scratch) -> PathBuf {
    let path = s.path.join("fakessh.sh");
    fs::write(
        &path,
        "#!/bin/sh\nfor last; do :; done\ncase \"$last\" in\n  \"uname -sm\") uname -sm ;;\n  *\" serve\") exec $last ;;\n  *) exec sh -c \"$last\" ;;\nesac\n",
    )
    .unwrap();
    path
}

#[test]
fn a_missing_agent_is_installed_on_the_first_handshake() {
    let s = Scratch::new();
    let fake = fake_ssh(&s);
    let agent = s.path.join("agent/mirako");
    let p = s.project(
        "app",
        &format!(
            "ssh = [\"sh\", {:?}]\nremote_bin = {:?}",
            fake.to_str().unwrap(),
            agent.to_str().unwrap()
        ),
    );
    let o = p.sub("check", &[]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    assert!(stderr(&o).contains("installing mirako"), "{}", show(&o));
    assert!(stdout(&o).contains(" ok ("), "{}", show(&o));
    assert_eq!(fs::read(&agent).unwrap(), fs::read(BIN).unwrap());

    // installed: the next handshake is silent, and a run goes through
    let o = p.sub("check", &[]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    assert!(!stderr(&o).contains("installing"), "{}", show(&o));
    p.run_ok(&[], &["true"]);
}

/// An agent from before the frames were postcard cannot decode the `Hello` and exits 0 in silence.
#[test]
fn an_agent_that_leaves_without_answering_is_replaced() {
    let s = Scratch::new();
    let fake = fake_ssh(&s);
    let agent = s.path.join("agent/mirako");
    fs::create_dir_all(agent.parent().unwrap()).unwrap();
    fs::write(&agent, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
    let p = s.project(
        "app",
        &format!(
            "ssh = [\"sh\", {:?}]\nremote_bin = {:?}",
            fake.to_str().unwrap(),
            agent.to_str().unwrap()
        ),
    );
    let o = p.sub("check", &[]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    assert!(stderr(&o).contains("does not answer the handshake"), "{}", show(&o));
    assert!(stdout(&o).contains(" ok ("), "{}", show(&o));
    assert_eq!(fs::read(&agent).unwrap(), fs::read(BIN).unwrap());
}

#[test]
fn the_installed_init_script_follows_this_binary_and_shim_check() {
    let s = Scratch::new();
    let p = s.project("app", "");
    let script = s.home().join(".gradle/init.d/mirako.gradle");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    let current = stdout(&s.mirako(&["gradle-shim", "print"]));
    assert!(current.contains("\"check\"") && current.contains(BIN), "{current}");

    // a stale script of this binary is rewritten by the next check or run
    fs::write(&script, current.replace("def args = []", "def args = [] // stale")).unwrap();
    let o = p.sub("check", &[]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    assert!(stderr(&o).contains("updated"), "{}", show(&o));
    assert_eq!(fs::read_to_string(&script).unwrap(), current);

    // `shim_check = false` in the global config drops the handshake from it
    let global = s.home().join(".config/mirako/config.toml");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, "shim_check = false\n").unwrap();
    p.run_ok(&[], &["true"]);
    let now = fs::read_to_string(&script).unwrap();
    assert!(!now.contains("\"check\"") && now.contains("shim_check = false"), "{now}");

    // another binary's script, stale or not, is left alone
    let other = current.replace(BIN, "/elsewhere/mirako").replace("def args = []", "// stale");
    fs::write(&script, &other).unwrap();
    let o = p.sub("check", &[]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    assert!(!stderr(&o).contains("updated"), "{}", show(&o));
    assert_eq!(fs::read_to_string(&script).unwrap(), other);
}

#[test]
fn setup_checks_ssh_reports_the_host_and_installs_shim_and_agent() {
    let s = Scratch::new();
    let fake = fake_ssh(&s);
    let agent = s.path.join("agent/mirako");
    let global = s.home().join(".config/mirako/config.toml");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(
        &global,
        format!(
            "host = \"loop\"\nssh = [\"sh\", {:?}]\nremote_bin = {:?}\nfallback = false\n",
            fake.to_str().unwrap(),
            agent.to_str().unwrap()
        ),
    )
    .unwrap();
    let o = s.mirako(&["setup"]);
    assert_eq!(o.status.code(), Some(0), "{}", show(&o));
    let out = stdout(&o);
    for want in ["config.toml exists", "ssh loop: ok", "\nloop: ", "init.d/mirako.gradle", " ok ("] {
        assert!(out.contains(want), "missing {want:?} in {}", show(&o));
    }
    assert!(stderr(&o).contains("installing mirako"), "{}", show(&o));
    assert_eq!(fs::read(&agent).unwrap(), fs::read(BIN).unwrap());
    assert!(fs::read_to_string(s.home().join(".gradle/init.d/mirako.gradle"))
        .unwrap()
        .contains(BIN));

    // a refusal from something that is not ssh is reported, not repaired
    fs::write(
        &global,
        "host = \"loop\"\nssh = [\"sh\", \"-c\", \"echo 'Permission denied (publickey).' >&2; exit 255\"]\n",
    )
    .unwrap();
    let o = s.mirako(&["setup"]);
    assert_eq!(o.status.code(), Some(2), "{}", show(&o));
    assert!(stderr(&o).contains("ssh loop failed: Permission denied"), "{}", show(&o));
}

// Properties of the sync: random sequences of changes, each round synced by the real binary.

/// Paths a push round changes. `a`, `a/b` and `c` are files at times and directories at others,
/// so a round can turn one into the other (the remote deletes before it writes).
const PUSH_PATHS: &[&str] = &["a", "a/f", "a/b", "a/b/g", "c", "c/h.txt", "top"];
/// Paths a pull round changes on the host: files only, under directories that stay directories,
/// since a pull never deletes here and so cannot put a file where a directory still is.
const PULL_PATHS: &[&str] = &["a/f", "a/b/g", "c/h.txt", "top", "d/e/i"];
const LINK_TARGETS: &[&str] = &["a/f", "nowhere", "../top"];

#[derive(Debug, Clone)]
enum Op {
    /// `len` from 260 000 up makes a file the next round sends as a delta once it is edited
    Write {
        path: &'static str,
        len: usize,
        seed: u64,
    },
    /// overwrite 100 bytes, or insert 700, at `at / 65536` of a regular file
    Edit {
        path: &'static str,
        at: u16,
        insert: bool,
        seed: u64,
    },
    Delete {
        path: &'static str,
    },
    Chmod {
        path: &'static str,
        exec: bool,
    },
    Symlink {
        path: &'static str,
        target: &'static str,
    },
    Rename {
        from: &'static str,
        to: &'static str,
    },
}

fn op(paths: &'static [&'static str]) -> impl Strategy<Value = Op> {
    let path = || proptest::sample::select(paths);
    let len = prop_oneof![3 => 0..2_000usize, 1 => 260_000..400_000usize];
    prop_oneof![
        4 => (path(), len, any::<u64>()).prop_map(|(path, len, seed)| Op::Write { path, len, seed }),
        3 => (path(), any::<u16>(), any::<bool>(), any::<u64>())
            .prop_map(|(path, at, insert, seed)| Op::Edit { path, at, insert, seed }),
        2 => path().prop_map(|path| Op::Delete { path }),
        1 => (path(), any::<bool>()).prop_map(|(path, exec)| Op::Chmod { path, exec }),
        1 => (path(), proptest::sample::select(LINK_TARGETS)).prop_map(|(path, target)| Op::Symlink { path, target }),
        1 => (path(), path()).prop_map(|(from, to)| Op::Rename { from, to }),
    ]
}

/// `chmod`: whether rounds change modes. A pull diffs by content and kind only, so a mode changed
/// alone on the host stays there (with the mode in its diff, a Windows client, where every mode
/// reads 0755, would download every file on every pull).
fn rounds(paths: &'static [&'static str], chmod: bool) -> impl Strategy<Value = Vec<Vec<Op>>> {
    let op = op(paths).prop_filter("no chmod", move |o| chmod || !matches!(o, Op::Chmod { .. }));
    proptest::collection::vec(proptest::collection::vec(op, 1..6), 1..5)
}

/// Each case starts a few binaries, so few cases, from a fixed seed: every run checks the same ones.
fn sync_config() -> ProptestConfig {
    ProptestConfig {
        cases: 12,
        rng_seed: RngSeed::Fixed(0x6d69_7261_6b6f),
        max_shrink_iters: 200,
        ..ProptestConfig::default()
    }
}

fn is_regular(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
}

/// Makes room for a file at `rel`: an ancestor that is not a directory goes, and so does
/// whatever is at `rel` itself.
fn clear_for(root: &Path, rel: &str) {
    let mut at = root.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for dir in &parts[..parts.len() - 1] {
        at.push(dir);
        if fs::symlink_metadata(&at).is_ok_and(|m| !m.is_dir()) {
            fs::remove_file(&at).unwrap();
        }
    }
    let p = root.join(rel);
    match fs::symlink_metadata(&p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(&p).unwrap(),
        Ok(_) => fs::remove_file(&p).unwrap(),
        Err(_) => {}
    }
    fs::create_dir_all(p.parent().unwrap()).unwrap();
}

/// A new mtime for every change: the index takes a file with the size and mtime it knows for unchanged.
fn touch(p: &Path, clock: &mut i64) {
    *clock += 1;
    filetime::set_file_mtime(p, filetime::FileTime::from_unix_time(1_600_000_000 + *clock, 0)).unwrap();
}

fn apply(root: &Path, op: &Op, clock: &mut i64) {
    match *op {
        Op::Write { path, len, seed } => {
            clear_for(root, path);
            fs::write(root.join(path), noise(len, seed)).unwrap();
            touch(&root.join(path), clock);
        }
        Op::Edit { path, at, insert, seed } => {
            let p = root.join(path);
            if !is_regular(&p) {
                return;
            }
            let mut data = fs::read(&p).unwrap();
            let pos = at as usize * data.len() / 65_536;
            if insert {
                data.splice(pos..pos, noise(700, seed));
            } else {
                let end = (pos + 100).min(data.len());
                data.splice(pos..end, noise(end - pos, seed));
            }
            fs::write(&p, data).unwrap();
            touch(&p, clock);
        }
        Op::Delete { path } => match fs::symlink_metadata(root.join(path)) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(root.join(path)).unwrap(),
            Ok(_) => fs::remove_file(root.join(path)).unwrap(),
            Err(_) => {}
        },
        Op::Chmod { path, exec } => {
            if is_regular(&root.join(path)) {
                let mode = if exec { 0o755 } else { 0o644 };
                fs::set_permissions(root.join(path), fs::Permissions::from_mode(mode)).unwrap();
            }
        }
        Op::Symlink { path, target } => {
            clear_for(root, path);
            symlink(target, root.join(path)).unwrap();
        }
        Op::Rename { from, to } => {
            let nested = |a: &str, b: &str| b.starts_with(&format!("{a}/"));
            if from == to || nested(from, to) || nested(to, from) || fs::symlink_metadata(root.join(from)).map_or(true, |m| m.is_dir()) {
                return;
            }
            clear_for(root, to);
            fs::rename(root.join(from), root.join(to)).unwrap();
        }
    }
}

#[derive(Debug, PartialEq)]
enum Node {
    File { hash: String, mode: u32, mtime_ns: i64 },
    Link(PathBuf),
}

/// Every file and symlink under `root` (directories are not synced as such).
fn snapshot(root: &Path) -> BTreeMap<String, Node> {
    fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Node>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            let m = fs::symlink_metadata(e.path()).unwrap();
            if m.is_dir() {
                walk(&e.path(), &rel, out);
            } else if m.file_type().is_symlink() {
                out.insert(rel, Node::Link(fs::read_link(e.path()).unwrap()));
            } else {
                let hash = blake3::hash(&read(&e.path())).to_hex()[..16].to_string();
                let mtime_ns = m.mtime() * 1_000_000_000 + m.mtime_nsec();
                out.insert(
                    rel,
                    Node::File {
                        hash,
                        mode: m.mode() & 0o777,
                        mtime_ns,
                    },
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, "", &mut out);
    out
}

proptest! {
    #![proptest_config(sync_config())]

    #[test]
    fn a_push_leaves_the_remote_copy_identical_after_any_changes(rounds in rounds(PUSH_PATHS, true)) {
        let s = Scratch::new();
        let p = s.project("app", "");
        let mut clock = 0;
        for ops in &rounds {
            for op in ops {
                apply(&p.root, op, &mut clock);
            }
            let out = p.sub("push", &[]);
            prop_assert!(out.status.success(), "{}", show(&out));
            let mut local = snapshot(&p.root);
            local.remove("mirako.toml");
            prop_assert_eq!(&local, &snapshot(&p.remote), "after {:?}", ops);
        }
    }

    #[test]
    fn a_pull_brings_back_any_changes_of_the_host_and_deletes_nothing_here(rounds in rounds(PULL_PATHS, false)) {
        let s = Scratch::new();
        let p = s.project("app", "");
        fs::create_dir_all(&p.remote).unwrap();
        let mut clock = 0;
        let mut before = snapshot(&p.root);
        for ops in &rounds {
            for op in ops {
                apply(&p.remote, op, &mut clock);
            }
            let out = p.sub("pull", &[]);
            prop_assert!(out.status.success(), "{}", show(&out));
            let (remote, local) = (snapshot(&p.remote), snapshot(&p.root));
            for (path, node) in &remote {
                prop_assert_eq!(local.get(path), Some(node), "{} after {:?}", path, ops);
            }
            for (path, node) in before.iter().filter(|(path, _)| !remote.contains_key(*path)) {
                prop_assert_eq!(local.get(path), Some(node), "{} kept after {:?}", path, ops);
            }
            before = local;
        }
    }
}
