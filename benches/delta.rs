//! The rsync rolling loop, signatures and the rebuild on the receiving side, on a 16 MiB file.

mod common;

use common::noise;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use mirako::delta;
use mirako::proto::{DeltaChunk, Op};
use std::fs;
use std::hint::black_box;
use std::time::Duration;

const SIZE: usize = 16 << 20;

fn benches(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.apk");
    // an APK's stored entries are already compressed: noise is the realistic content
    let old = noise(SIZE, 1);
    fs::write(&old_path, &old).unwrap();
    let sig = delta::signature(&old_path).unwrap();

    let mut inserted = Vec::with_capacity(SIZE + 4096);
    inserted.extend_from_slice(&old[..SIZE / 2]);
    inserted.extend_from_slice(&noise(4096, 99));
    inserted.extend_from_slice(&old[SIZE / 2..]);
    let unrelated = noise(SIZE, 2);
    let mut shifted = vec![0x42u8];
    shifted.extend_from_slice(&old);

    let mut g = c.benchmark_group("delta");
    g.sample_size(10)
        .measurement_time(Duration::from_secs(2))
        .warm_up_time(Duration::from_secs(1));
    g.throughput(Throughput::Bytes(SIZE as u64));

    g.bench_function("signature", |b| b.iter(|| delta::signature(black_box(&old_path)).unwrap()));

    for (name, new) in [
        ("identical", &old),
        ("insert_4k_middle", &inserted),
        ("unrelated", &unrelated),
        ("shifted_by_one", &shifted),
    ] {
        g.bench_function(name, |b| {
            b.iter(|| {
                let mut wire = 0usize;
                delta::delta(black_box(new), &sig, |op| {
                    wire += match &op {
                        Op::Data(d) => d.len(),
                        Op::Copy { .. } => 8,
                    };
                    Ok(())
                })
                .unwrap();
                wire
            })
        });
    }

    let head = DeltaChunk {
        path: "app/build/outputs/apk/debug/app-debug.apk".into(),
        mode: 0o644,
        mtime_ns: 0,
        size: inserted.len() as u64,
        hash: *blake3::hash(&inserted).as_bytes(),
        ops: Vec::new(),
        last: false,
    };
    g.bench_function("stream/insert_4k_middle", |b| {
        b.iter(|| {
            delta::stream(black_box(&inserted), &sig, &head, |chunk| {
                black_box(chunk);
                Ok(())
            })
            .unwrap()
        })
    });

    for (name, new) in [("identical", &old), ("unrelated", &unrelated)] {
        let mut ops = Vec::new();
        delta::delta(new, &sig, |op| {
            ops.push(op);
            Ok(())
        })
        .unwrap();
        g.bench_function(format!("apply/{name}"), |b| {
            b.iter(|| {
                let mut old_file = fs::File::open(&old_path).unwrap();
                let mut hasher = blake3::Hasher::new();
                delta::apply(Some(&mut old_file), SIZE as u64, sig.block, &ops, &mut std::io::sink(), &mut hasher).unwrap();
                hasher.finalize()
            })
        });
    }
    g.finish();
}

criterion_group!(group, benches);
criterion_main!(group);
