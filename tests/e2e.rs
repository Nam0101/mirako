//! End-to-end tests of the real binary over the real protocol, without ssh ("loopback"): the
//! project's `mirako.toml` sets `ssh = ["sh", "-c", "exec <bin> serve"]`, so the client starts the
//! agent as a local child and the ssh arguments it appends become `$0..$n` of the script.
//!
//! Hermetic: every child runs with `HOME` and `GRADLE_USER_HOME` inside the test's scratch dir, so
//! the global config, both index caches and the Gradle retention script never touch the real ones.

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};
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

    fn mirako(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .current_dir(&self.path)
            .env("HOME", self.home())
            .env("GRADLE_USER_HOME", self.gradle_home())
            .env_remove("MIRAKO_LOCAL")
            .env_remove("MIRAKO_REMOTE")
            .env_remove("PWD")
            .output()
            .unwrap()
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

/// `push   {files} files ({deltas} as delta), {bytes} → {wire} on the wire, …` → (files, deltas, wire)
fn counts(l: &str) -> (usize, usize, String) {
    let w: Vec<&str> = l.split_whitespace().collect();
    assert_eq!(w[2], "files", "{l}");
    let deltas = w[3].trim_start_matches('(').parse().unwrap();
    let arrow = w.iter().position(|s| *s == "→").unwrap_or_else(|| panic!("{l}"));
    (w[1].parse().unwrap(), deltas, format!("{} {}", w[arrow + 1], w[arrow + 2]))
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
        &["sh", "-c", "mkdir -p build/outputs && cat src/a.txt src/sub/b.txt > build/outputs/out.txt && echo \"built in $PWD\""],
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
    let push = line(&out, "push");
    assert_eq!(counts(push).0, 0, "{out}");
    assert!(push.contains(", 0 deleted"), "{out}");
    assert_eq!(counts(line(&out, "pull")).0, 0, "{out}");
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
    assert!(wire.ends_with(" KB") || wire.ends_with(" B"), "wire {wire} is not under 1 MB:\n{out}");
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

    let out = p.run_ok(&["--no-pull"], &["sh", "-c", "cp src/a.txt build/seen.txt && echo x > build/new.txt"]);
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

/// README: the script is "removed again with `gc_days = 0`" after each run. But `client::run` only
/// sends `Req::Gc` when `gc_days > 0` or `gc_after_pull` is set, so a run with `gc_days = 0` never
/// reaches `gc::gradle_retention(…, 0)`; only `mirako gc` removes the file.
#[test]
#[ignore = "suspected bug: a run with gc_days = 0 leaves ~/.gradle/init.d/mirako-gc.gradle behind (src/client.rs, `if cfg.gc_days > 0 || …`)"]
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
fn a_project_without_host_fails_clearly() {
    let s = Scratch::new();
    let root = s.path.join("app");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("mirako.toml"), "fallback = false\n").unwrap();
    let o = s.mirako(&["run", "--project", root.to_str().unwrap(), "--", "true"]);
    assert_eq!(o.status.code(), Some(2), "{}", show(&o));
    assert!(stderr(&o).contains("no host configured"), "{}", show(&o));
}
