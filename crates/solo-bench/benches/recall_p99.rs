// SPDX-License-Identifier: Apache-2.0

//! Placeholder benchmark — wired up in week 5.

use criterion::{Criterion, criterion_group, criterion_main};

fn bench_placeholder(c: &mut Criterion) {
    c.bench_function("placeholder_noop", |b| {
        b.iter(|| {
            // No-op until commit 5.1 lands the LongMemEval / MemoryAgentBench
            // corpora and the p99 budget assertions.
        });
    });
}

criterion_group!(benches, bench_placeholder);
criterion_main!(benches);
