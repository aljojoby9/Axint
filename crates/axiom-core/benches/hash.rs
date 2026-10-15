//! Hashing throughput. The design budget is multi-gigabyte-per-second BLAKE3.

use axiom_core::AxiomHash;
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

fn hash_throughput(criterion: &mut Criterion) {
    let data = vec![0xA5_u8; 8 * 1024 * 1024];
    let mut group = criterion.benchmark_group("blake3");
    group.throughput(Throughput::Bytes(u64::try_from(data.len()).unwrap_or(0)));
    group.bench_function("parallel_8mib", |bencher| {
        bencher.iter(|| AxiomHash::hash_bytes(black_box(&data)));
    });
    group.finish();
}

criterion_group!(benches, hash_throughput);
criterion_main!(benches);
