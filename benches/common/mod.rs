//! Deterministic inputs shared by the benches.
#![allow(dead_code)] // each bench uses a subset

use std::fs;
use std::path::Path;

/// xorshift64 bytes: incompressible, never matches anything else.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        out.extend_from_slice(&s.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Looks like build output / class-file constant pools: repeating identifiers with varying numbers.
pub fn compressible(len: usize, seed: u64) -> Vec<u8> {
    const WORDS: &[&str] = &[
        "com/example/app/ui/MainActivity",
        "androidx/compose/runtime/Composer",
        "kotlin/jvm/internal/Intrinsics",
        "Lkotlin/coroutines/Continuation;",
        "invokeSuspend",
        "R$drawable",
        "getLifecycle",
        "checkNotNullParameter",
    ];
    let mut s = seed | 1;
    let mut out = Vec::with_capacity(len + 128);
    while out.len() < len {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let w = WORDS[(s % WORDS.len() as u64) as usize];
        out.extend_from_slice(format!("{w}${} line {} ", s % 97, (s >> 8) % 4096).as_bytes());
        if s.is_multiple_of(7) {
            out.push(b'\n');
        }
    }
    out.truncate(len);
    out
}

pub const SMALL_FILES: usize = 2000;
pub const BIG_FILES: usize = 5;

/// An Android-like source tree: `SMALL_FILES` ~1 KiB sources under `mod*/src/pkg*/`, `BIG_FILES`
/// 2 MiB jars under `libs/`, plus `build/` and `.gradle/` dirs the excludes skip.
pub fn android_tree(root: &Path) {
    let mut n = 0u64;
    for m in 0..10 {
        for p in 0..10 {
            let dir = root.join(format!("mod{m}/src/pkg{p}"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..20 {
                n += 1;
                fs::write(dir.join(format!("F{f}.kt")), compressible(1024, n)).unwrap();
            }
        }
        let build = root.join(format!("mod{m}/build/intermediates/classes"));
        fs::create_dir_all(&build).unwrap();
        let keep = root.join(format!("mod{m}/build/intermediates/keep"));
        fs::create_dir_all(&keep).unwrap();
        let outputs = root.join(format!("mod{m}/build/outputs"));
        fs::create_dir_all(&outputs).unwrap();
        for f in 0..20 {
            fs::write(build.join(format!("C{f}.class")), compressible(1024, 10_000 + n + f)).unwrap();
        }
        fs::write(keep.join("redirect"), b"listingFile=../apk/debug/output.json\n").unwrap();
        fs::write(outputs.join("app.apk"), compressible(4096, 20_000 + n)).unwrap();
    }
    let gradle = root.join(".gradle/8.9/fileHashes");
    fs::create_dir_all(&gradle).unwrap();
    fs::write(gradle.join("fileHashes.bin"), noise(64 * 1024, 7)).unwrap();
    fs::create_dir_all(root.join("libs")).unwrap();
    for i in 0..BIG_FILES {
        fs::write(root.join(format!("libs/big{i}.jar")), compressible(2 << 20, 30_000 + i as u64)).unwrap();
    }
}
