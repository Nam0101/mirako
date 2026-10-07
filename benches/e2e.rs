//! One `mirako run -- true` through the real binary over a loopback "ssh" (`sh -c "exec mirako
//! serve"`): process start, handshake, scan, push, exec and pull of a 2 000-file project.
//! HOME and GRADLE_USER_HOME point into the scratch dir, so index caches and gc stay there.

mod common;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_mirako");

struct Setup {
    _dir: tempfile::TempDir,
    scratch: PathBuf,
    proj: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().canonicalize().unwrap();
        let proj = scratch.join("proj");
        for d in ["proj", "home", "gradle"] {
            fs::create_dir_all(scratch.join(d)).unwrap();
        }
        common::android_tree(&proj);
        let toml = format!(
            r#"host = "loop"
remote_folder = "{remote}"
remote_bin = "{BIN}"
ssh = ["sh", "-c", "exec {BIN} serve"]
fallback = false
gc_days = 0
gc_after_pull = []
exclude_local = ["build"]
exclude_remote = ["src"]
exclude_common = [".git", "mirako.toml", ".DS_Store"]
"#,
            remote = scratch.join("remote").display()
        );
        fs::write(proj.join("mirako.toml"), toml).unwrap();
        Self { _dir: dir, scratch, proj }
    }

    fn run(&self) {
        let out = Command::new(BIN)
            .args(["run", "-p"])
            .arg(&self.proj)
            .args(["--", "true"])
            .env("HOME", self.scratch.join("home"))
            .env("GRADLE_USER_HOME", self.scratch.join("gradle"))
            .current_dir(&self.scratch)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "mirako run failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Forget both sides: no remote copy, no index caches.
    fn reset(&self) {
        remove(&self.scratch.join("remote"));
        remove(&self.scratch.join("home/Library/Caches"));
        remove(&self.scratch.join("home/.cache"));
    }
}

fn remove(p: &Path) {
    if p.exists() {
        fs::remove_dir_all(p).unwrap();
    }
}

fn benches(c: &mut Criterion) {
    let s = Setup::new();
    let mut g = c.benchmark_group("e2e");
    g.sample_size(10)
        .measurement_time(Duration::from_secs(5))
        .warm_up_time(Duration::from_secs(1));
    g.throughput(Throughput::Elements((common::SMALL_FILES + common::BIG_FILES) as u64));

    g.bench_function("run/cold", |b| b.iter_batched(|| s.reset(), |()| s.run(), BatchSize::PerIteration));

    s.run();
    g.bench_function("run/warm", |b| b.iter(|| s.run()));

    let changed = s.proj.join("mod3/src/pkg4/F7.kt");
    let mut n = 0u64;
    g.bench_function("run/one_changed_file", |b| {
        b.iter_batched(
            || {
                n += 1;
                fs::write(&changed, common::compressible(1024, 1_000_000 + n)).unwrap();
            },
            |()| s.run(),
            BatchSize::PerIteration,
        )
    });
    g.finish();
}

criterion_group!(group, benches);
criterion_main!(group);
