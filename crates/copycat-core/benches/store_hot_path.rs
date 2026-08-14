//! Hot-path performance benchmark for `copycat-core` store operations.
//!
//! Measures hot operation latency distributions. Run:
//!
//!   cargo bench -p copycat-core --bench store_hot_path
//!   COPYCAT_PERF_N=500000 COPYCAT_PERF_CAP=1024 cargo bench -p copycat-core --bench store_hot_path

use std::time::Instant;

use copycat_core::entry::{Entry, Source};
use copycat_core::store::{ClipboardStore, MemoryStore, Recall};

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Time `iters` calls to `op`, returning per-call nanoseconds sorted ascending.
fn measure(iters: usize, mut op: impl FnMut(usize)) -> Vec<u64> {
    // Warm up so we're not measuring cold caches / first-touch page faults.
    for i in 0..(iters / 10).max(1) {
        op(i);
    }
    let mut samples = Vec::with_capacity(iters);
    for i in 0..iters {
        let t = Instant::now();
        op(i);
        samples.push(t.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();
    samples
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn report(name: &str, s: &[u64]) {
    let mean = s.iter().sum::<u64>() as f64 / s.len() as f64;
    let thru = if mean > 0.0 { 1e9 / mean } else { 0.0 };
    println!(
        "  {name:<22} p50={:>6}ns  p95={:>6}ns  p99={:>6}ns  max={:>7}ns  mean={:>7.1}ns  (~{:.1}M ops/s)",
        pct(s, 0.50),
        pct(s, 0.95),
        pct(s, 0.99),
        s[s.len() - 1],
        mean,
        thru / 1e6,
    );
}

fn text_entry(id: usize) -> Entry {
    // Vary content so trivial dedup isn't what gets measured.
    Entry::from_text(
        format!("{id:013}-000000"),
        format!("https://example.com/report/{id}?token=v{id}"),
        id as i64,
        Source::simple("perf"),
    )
}

fn main() {
    let n = env_usize("COPYCAT_PERF_N", 200_000);
    let cap = env_usize("COPYCAT_PERF_CAP", 256);

    if cfg!(debug_assertions) {
        eprintln!("WARNING: debug build — numbers are not representative. Use `cargo bench`.\n");
    }
    println!("temporal-store hot path — N={n} ops, capacity={cap} entries\n");

    // Push against a full ring so eviction cost is included.
    let mut store = MemoryStore::new(cap);
    for i in 0..cap {
        store.push(text_entry(i)).unwrap();
    }
    let mut next = cap;
    let copy = measure(n, |_| {
        store.push(text_entry(next)).unwrap();
        next += 1;
    });
    report("copy (push+evict)", &copy);

    let read0 = measure(n, |_| {
        std::hint::black_box(store.get(0));
    });
    report("recall current get(0)", &read0);

    let mid = cap / 2;
    let readk = measure(n, |_| {
        std::hint::black_box(store.get(mid));
    });
    report(&format!("recall get({mid})"), &readk);

    let q = Recall {
        limit: 10,
        ..Default::default()
    };
    let window = measure(n, |_| {
        std::hint::black_box(store.recall(&q).unwrap());
    });
    report("recall last-10 window", &window);

    println!(
        "\nContext: a single LLM turn to regenerate one of these values is \
         typically 10^8-10^9 ns (100ms-1s+).\nRecall above is the alternative — \
         that gap is the reason copycat exists."
    );
}
