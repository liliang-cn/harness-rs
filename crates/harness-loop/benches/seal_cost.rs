//! What sealing costs, per run.
//!
//! The gate hashes its declared files twice: once before the model's first turn
//! and once before a pass is accepted. That is the whole added cost, and it is
//! paid per run rather than per turn — but "small" is a claim, not an
//! observation, so here is the observation.

use harness_loop::SealSet;
use std::path::PathBuf;
use std::time::Instant;

fn bench(label: &str, iters: u32, mut f: impl FnMut()) {
    for _ in 0..iters.min(20) {
        f();
    }
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    let per = t.elapsed().as_secs_f64() / iters as f64;
    println!("  {label:<44} {:>10.1} µs", per * 1e6);
}

fn main() {
    let dir = std::env::temp_dir().join(format!("harness-seal-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    println!("=== SealSet::capture — the per-run cost of an unforgeable gate ===");
    println!("machine: {}", machine());
    println!("(a run pays this twice: once at start, once before accepting a pass)\n");

    // A contract is normally a handful of small files: a test file, a fixture,
    // an expected-output blob.
    for (n, size) in [(1usize, 1024usize), (5, 4096), (20, 8192)] {
        let mut paths: Vec<PathBuf> = Vec::new();
        for i in 0..n {
            let p = format!("c{i}.txt");
            std::fs::write(dir.join(&p), "x".repeat(size)).unwrap();
            paths.push(PathBuf::from(p));
        }
        let label = format!("{n} file(s) × {} KiB", size / 1024);
        bench(&label, 2000, || {
            std::hint::black_box(SealSet::capture(&dir, paths.iter()));
        });
    }

    // The pathological end: someone seals a large generated fixture.
    let big = dir.join("big.bin");
    std::fs::write(&big, vec![7u8; 8 * 1024 * 1024]).unwrap();
    bench("1 file × 8 MiB (seal something large)", 100, || {
        std::hint::black_box(SealSet::capture(&dir, ["big.bin"]));
    });

    // Comparison: nothing sealed is the default, and must cost nothing.
    let none: Vec<PathBuf> = Vec::new();
    bench("nothing sealed (the default path)", 20000, || {
        std::hint::black_box(SealSet::capture(&dir, none.iter()));
    });

    let _ = std::fs::remove_dir_all(&dir);
}

fn machine() -> String {
    std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}
