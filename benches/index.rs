//! `Index::scan` on an Android-like tree: cold (every file hashed by rayon) and warm (all
//! (size, mtime) cache hits, the usual case before a build).

mod common;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use mirako::index::Index;
use mirako::patterns::Matcher;
use std::fs;
use std::hint::black_box;
use std::time::Duration;

fn benches(c: &mut Criterion) {
    let scratch = tempfile::tempdir().unwrap();
    // the index cache lives under the OS cache dir, which follows $HOME: keep it in the scratch
    // dir so the real ~/Library/Caches/mirako is never touched (no other thread runs yet)
    std::env::set_var("HOME", scratch.path().join("home"));
    fs::create_dir_all(scratch.path().join("proj")).unwrap();
    let root = scratch.path().join("proj").canonicalize().unwrap();
    common::android_tree(&root);
    let cache = Index::cache_path(&root);
    assert!(
        cache.starts_with(scratch.path().join("home")),
        "index cache escaped the scratch dir: {}",
        cache.display()
    );
    let exclude = Matcher::new(&["build".to_string(), ".gradle".to_string()]).unwrap();
    let files = (common::SMALL_FILES + common::BIG_FILES) as u64;

    let mut g = c.benchmark_group("index");
    g.sample_size(10)
        .measurement_time(Duration::from_secs(3))
        .warm_up_time(Duration::from_secs(1));
    g.throughput(Throughput::Elements(files));
    g.bench_function("scan/cold", |b| {
        b.iter_batched(
            || {
                let _ = fs::remove_file(&cache);
                Index::open(&root)
            },
            |mut idx| {
                let entries = idx.scan(black_box(&root), &exclude).unwrap();
                assert_eq!(entries.len() as u64, files);
                entries
            },
            BatchSize::PerIteration,
        )
    });
    let mut idx = Index::open(&root);
    idx.scan(&root, &exclude).unwrap();
    g.bench_function("scan/warm", |b| b.iter(|| idx.scan(black_box(&root), &exclude).unwrap()));
    g.finish();
    let _ = fs::remove_file(&cache);
}

criterion_group!(group, benches);
criterion_main!(group);
