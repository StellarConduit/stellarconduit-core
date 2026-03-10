use criterion::{black_box, criterion_group, criterion_main, Criterion};
use stellarconduit_core::gossip::fanout::{select_random_peers, FanoutCalculator};
use stellarconduit_core::peer::identity::PeerIdentity;

fn bench_fanout_calculate_dense_mesh(c: &mut Criterion) {
    let calculator = FanoutCalculator::new();
    // 50 connections and known mesh size (say 1000)
    let active_connections = 50;
    let mesh_size = Some(1000);

    c.bench_function("fanout_calculate_dense_mesh", |b| {
        b.iter(|| {
            black_box(calculator.calculate(black_box(active_connections), black_box(mesh_size)));
        });
    });
}

fn bench_fanout_calculate_sparse_mesh(c: &mut Criterion) {
    let calculator = FanoutCalculator::new();
    // 3 connections and unknown topology
    let active_connections = 3;
    let mesh_size = None;

    c.bench_function("fanout_calculate_sparse_mesh", |b| {
        b.iter(|| {
            black_box(calculator.calculate(black_box(active_connections), black_box(mesh_size)));
        });
    });
}

fn bench_select_random_peers(c: &mut Criterion) {
    let peers: Vec<PeerIdentity> = (0..30)
        .map(|i| {
            let mut key = [0u8; 32];
            let bytes = (i as u32).to_le_bytes();
            key[0..4].copy_from_slice(&bytes);
            PeerIdentity::new(key)
        })
        .collect();
    let fanout = 6;

    c.bench_function("select_random_peers_30_6", |b| {
        b.iter(|| {
            black_box(select_random_peers(black_box(&peers), black_box(fanout)));
        });
    });
}

criterion_group!(
    benches,
    bench_fanout_calculate_dense_mesh,
    bench_fanout_calculate_sparse_mesh,
    bench_select_random_peers
);
criterion_main!(benches);
