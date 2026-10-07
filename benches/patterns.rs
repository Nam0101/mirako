//! The exclude matcher on the paths of a large Android project, with the default lists plus the
//! sample `exclude_remote_extra` (includes inside excluded dirs, the expensive shape).

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use mirako::config::{DEFAULT_EXCLUDE_COMMON, DEFAULT_EXCLUDE_LOCAL, DEFAULT_EXCLUDE_REMOTE};
use mirako::patterns::Matcher;
use std::hint::black_box;
use std::time::Duration;

const SAMPLE_REMOTE_EXTRA: &[&str] = &[
    "build/intermediates",
    "!build/intermediates/apk_ide_redirect_file",
    "!build/intermediates/apk",
    "build/tmp",
    "build/kotlin",
    "build/kspCaches",
];

fn paths(n: usize) -> Vec<String> {
    let shapes: &[&dyn Fn(usize) -> String] = &[
        &|i| format!("feature{}/src/main/java/com/example/feature{}/ui/Screen{i}.kt", i % 30, i % 30),
        &|i| format!("feature{}/src/main/res/drawable-xxhdpi/ic_{i}.png", i % 30),
        &|i| format!("feature{}/build/intermediates/javac/debug/classes/com/example/C{i}.class", i % 30),
        &|i| format!("app/build/intermediates/apk/debug/app-debug-{i}.apk"),
        &|i| format!("app/build/intermediates/apk_ide_redirect_file/debug/f{i}"),
        &|i| format!("feature{}/build/outputs/aar/f{i}.aar", i % 30),
        &|i| format!("feature{}/build/generated/source/navigation-args/Args{i}.kt", i % 30),
        &|i| format!("feature{}/build/tmp/kotlin-classes/debug/K{i}.class", i % 30),
        &|i| format!(".gradle/8.9/executionHistory/h{i}.bin"),
        &|i| format!("feature{}/src/test/kotlin/com/example/T{i}Test.kt", i % 30),
    ];
    (0..n).map(|i| shapes[i % shapes.len()](i)).collect()
}

fn benches(c: &mut Criterion) {
    let mut patterns: Vec<String> = DEFAULT_EXCLUDE_REMOTE
        .iter()
        .chain(DEFAULT_EXCLUDE_COMMON)
        .map(|s| s.to_string())
        .collect();
    patterns.extend(SAMPLE_REMOTE_EXTRA.iter().map(|s| s.to_string()));
    let local: Vec<String> = DEFAULT_EXCLUDE_LOCAL
        .iter()
        .chain(DEFAULT_EXCLUDE_COMMON)
        .map(|s| s.to_string())
        .collect();
    let paths = paths(10_000);

    let mut g = c.benchmark_group("patterns");
    g.measurement_time(Duration::from_secs(2)).warm_up_time(Duration::from_secs(1));
    g.bench_function("new/remote_with_includes", |b| {
        b.iter(|| Matcher::new(black_box(&patterns)).unwrap())
    });
    g.throughput(Throughput::Elements(paths.len() as u64));
    for (name, pats) in [("remote_with_includes", &patterns), ("local_defaults", &local)] {
        let m = Matcher::new(pats).unwrap();
        g.bench_function(format!("match_10k/{name}"), |b| {
            b.iter(|| {
                let mut n = 0usize;
                for p in &paths {
                    n += m.excluded(black_box(p)) as usize + m.skip_subtree(black_box(p)) as usize;
                }
                n
            })
        });
    }
    g.finish();
}

criterion_group!(group, benches);
criterion_main!(group);
