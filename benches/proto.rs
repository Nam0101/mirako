//! zstd chunks and frame encode/decode: the per-byte and per-entry CPU cost of the wire.

mod common;

use common::{compressible, noise};
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use mirako::proto::{self, Chunk, Entry, Kind, Req, Resp, CHUNK};
use std::hint::black_box;
use std::io::Cursor;
use std::time::Duration;

fn zstd(c: &mut Criterion) {
    let mut g = c.benchmark_group("zstd");
    g.sample_size(20)
        .measurement_time(Duration::from_secs(3))
        .warm_up_time(Duration::from_secs(1));
    g.throughput(Throughput::Bytes(CHUNK as u64));
    for (name, data) in [("compressible", compressible(CHUNK, 1)), ("noise", noise(CHUNK, 1))] {
        g.bench_function(format!("compress/{name}"), |b| {
            b.iter(|| proto::compress(black_box(&data)).unwrap())
        });
        let z = proto::compress(&data).unwrap();
        g.bench_function(format!("decompress/{name}"), |b| {
            b.iter(|| proto::decompress(black_box(&z)).unwrap())
        });
    }
    g.finish();
}

fn frames(c: &mut Criterion) {
    let mut g = c.benchmark_group("frame");
    g.sample_size(20)
        .measurement_time(Duration::from_secs(3))
        .warm_up_time(Duration::from_secs(1));

    const N: usize = 5000;
    let manifest = Resp::Manifest {
        root: "/Users/builder/mirako/my-android-app".into(),
        entries: (0..N)
            .map(|i| Entry {
                path: format!("app/src/main/java/com/example/feature{}/ui/Screen{i}.kt", i % 40),
                kind: Kind::File,
                size: 1000 + i as u64,
                mtime_ns: 1_700_000_000_000_000_000 + i as i64,
                mode: 0o644,
                hash: *blake3::hash(&i.to_le_bytes()).as_bytes(),
            })
            .collect(),
    };
    g.throughput(Throughput::Elements(N as u64));
    g.bench_function("manifest_5000", |b| {
        let mut buf = Vec::new();
        b.iter(|| {
            buf.clear();
            proto::write_frame(&mut buf, black_box(&manifest)).unwrap();
            let back: Resp = proto::read_frame(&mut Cursor::new(&buf)).unwrap();
            back
        })
    });

    let put = Req::Put(Chunk {
        path: "app/build/outputs/apk/debug/app-debug.apk".into(),
        mode: 0o644,
        mtime_ns: 0,
        size: CHUNK as u64,
        offset: 0,
        last: true,
        data: noise(CHUNK, 3),
    });
    g.throughput(Throughput::Bytes(CHUNK as u64));
    g.bench_function("put_4mib", |b| {
        let mut buf = Vec::new();
        b.iter(|| {
            buf.clear();
            proto::write_frame(&mut buf, black_box(&put)).unwrap();
            let back: Req = proto::read_frame(&mut Cursor::new(&buf)).unwrap();
            back
        })
    });
    g.finish();
}

criterion_group!(group, zstd, frames);
criterion_main!(group);
