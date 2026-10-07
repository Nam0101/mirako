//! Remote → local path rewriting of streamed build output, fed in pipe-sized chunks.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use mirako::rewrite::LineRewriter;
use std::hint::black_box;

const REMOTE: &str = "/Users/builder/mirako/my-android-app";
const LOCAL: &str = "/Users/dev/AndroidStudioProjects/my-android-app";

fn log(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 256);
    let mut i = 0usize;
    while out.len() < len {
        let line = match i % 4 {
            0 => format!(
                "w: file://{REMOTE}/feature{}/src/main/java/Screen{i}.kt:{}:5 Parameter 'x' is never used ({REMOTE})\n",
                i % 30,
                i % 400
            ),
            1 => format!(
                "> Task :feature{}:compileDebugKotlin UP-TO-DATE {REMOTE}/feature{}/build\n",
                i % 30,
                i % 30
            ),
            2 => format!("e: {REMOTE}/app/src/main/AndroidManifest.xml:{}: error in {REMOTE}/app\n", i % 90),
            _ => format!("Copying {REMOTE}/app/build/intermediates/dex/debug/classes{i}.dex -> {REMOTE}/app/build/outputs\n"),
        };
        out.extend_from_slice(line.as_bytes());
        i += 1;
    }
    out.truncate(len);
    out
}

fn benches(c: &mut Criterion) {
    let data = log(4 << 20);
    let mut g = c.benchmark_group("rewrite");
    g.sample_size(20);
    g.throughput(Throughput::Bytes(data.len() as u64));
    g.bench_function("feed_32k_chunks", |b| {
        b.iter(|| {
            let mut rw = LineRewriter::new(REMOTE, LOCAL);
            let mut out = 0usize;
            for chunk in black_box(&data).chunks(32 * 1024) {
                out += rw.feed(chunk).len();
            }
            out + rw.flush().len()
        })
    });
    g.finish();
}

criterion_group!(group, benches);
criterion_main!(group);
