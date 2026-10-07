//! `gc::collect` as a dry run over a `remote_folder` with one synced mirror: the mirror walk,
//! `dir_size` and the `build` matcher with an include inside the deleted directory.

mod common;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use mirako::gc;
use mirako::index::Index;
use mirako::proto::GcReq;
use std::fs;
use std::hint::black_box;
use std::time::Duration;

fn benches(c: &mut Criterion) {
    let scratch = tempfile::tempdir().unwrap();
    // `Index::cache_path` follows $HOME: the mirror's idx goes to the scratch dir, not the real
    // ~/Library/Caches/mirako (no other thread runs yet)
    std::env::set_var("HOME", scratch.path().join("home"));
    let folder = scratch.path().join("remote");
    let mirror = folder.join("proj");
    fs::create_dir_all(&mirror).unwrap();
    common::android_tree(&mirror);
    let canon = mirror.canonicalize().unwrap();
    let idx = Index::cache_path(&canon);
    assert!(
        idx.starts_with(scratch.path().join("home")),
        "index cache escaped the scratch dir: {}",
        idx.display()
    );
    Index::open(&canon).save();
    assert!(idx.exists());

    let req = GcReq {
        folder: folder.to_string_lossy().into_owned(),
        keep_days: Some(7),
        current: Some(mirror.to_string_lossy().into_owned()),
        build: vec!["build/intermediates".into(), "!build/intermediates/keep".into()],
        gradle_days: 0,
        dry_run: true,
        sizes: true,
    };
    let report = gc::collect(&req).unwrap();
    assert!(report.build_bytes > 0 && report.mirrors.len() == 1 && !report.mirrors[0].removed);

    let mut g = c.benchmark_group("gc");
    g.sample_size(10)
        .measurement_time(Duration::from_secs(3))
        .warm_up_time(Duration::from_secs(1));
    // every file of the mirror is visited by dir_size; the build/ ones a second time by the matcher walk
    g.throughput(Throughput::Elements((common::SMALL_FILES + common::BIG_FILES) as u64));
    g.bench_function("collect/dry_run_one_mirror", |b| b.iter(|| gc::collect(black_box(&req)).unwrap()));
    g.finish();
    let _ = fs::remove_file(&idx);
}

criterion_group!(group, benches);
criterion_main!(group);
