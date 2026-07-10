use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::sync::Arc;
use tempfile::TempDir;
use aegis_memory::{
    filesystem::{FileSystem, MountableFS, MemFs, WriteFlag},
    context::{ContextBuilder, ContextSection, MemoryResult},
    cas::ContentAddressedStorage,
    hybrid_search::{HybridSearch, SearchScope},
    taxonomy::{MemoryTaxonomy, Wing, Room, Drawer},
    wal::{WriteAheadLog, WalEntry},
};

fn create_test_taxonomy(num_wings: usize, rooms_per_wing: usize, drawers_per_room: usize) -> MemoryTaxonomy {
    let mut tax = MemoryTaxonomy::new();
    for w in 0..num_wings {
        let mut wing = Wing::default();
        for r in 0..rooms_per_wing {
            let mut room = Room::default();
            for d in 0..drawers_per_room {
                room.drawers.push(Drawer::new(
                    format!("drawer_{w}_{r}_{d}"),
                    format!("Content of drawer {w}/{r}/{d} with searchable keywords"),
                    format!("wing-{w}"),
                    format!("room-{r}"),
                    "bench.md".to_string(),
                    d as u32,
                ));
            }
            wing.rooms.insert(format!("room-{r}"), room);
        }
        tax.wings.insert(format!("wing-{w}"), wing);
    }
    tax
}

fn bench_mountable_fs(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("mountable_fs");

    group.bench_function("mount_and_exists_100", |b| {
        b.iter(|| {
            let mut fs = MountableFS::new();
            for i in 0..100 {
                fs.mount(&format!("/wing-{i}/room"), Arc::new(MemFs::new()));
            }
            rt.block_on(async {
                black_box(fs.exists("/wing-50/room/test").await)
            });
        });
    });

    group.bench_function("mount_100_backends", |b| {
        b.iter(|| {
            let mut fs = MountableFS::new();
            for i in 0..100 {
                fs.mount(&format!("/path-{i}"), Arc::new(MemFs::new()));
            }
            black_box(fs)
        });
    });

    group.finish();
}

fn bench_memfs(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("memfs");

    group.bench_function("write_and_read_100_files", |b| {
        b.iter(|| {
            let fs = MemFs::new();
            rt.block_on(async {
                for i in 0..100 {
                    let path = format!("/file-{i}.txt");
                    fs.write(&path, format!("content-{i}").as_bytes(), 0, WriteFlag::Create).await.unwrap();
                    black_box(fs.read(&path, 0, u64::MAX).await.unwrap());
                }
            });
        });
    });

    group.finish();
}

fn bench_context_builder(c: &mut Criterion) {
    let mut group = c.benchmark_group("context_builder");

    group.bench_function("build_5_memory_sections_4096_budget", |b| {
        b.iter(|| {
            let mut builder = ContextBuilder::new(4096);
            builder = builder.with_section(ContextSection::Identity {
                content: "You are a helpful assistant.".to_string(),
            });
            for i in 0..5 {
                builder = builder.with_section(ContextSection::Memory {
                    results: vec![MemoryResult {
                        content: format!("Memory entry {i} with some content to fill budget"),
                        score: 0.9 - (i as f32 * 0.1),
                        source: "wing-0/room-0".to_string(),
                        confidence: 0.8,
                    }],
                });
            }
            black_box(builder.build())
        });
    });

    group.finish();
}

fn bench_cas(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("content_addressed_storage");

    group.bench_function("put_and_get_1kb", |b| {
        let cas = ContentAddressedStorage::new();
        let data = "x".repeat(1024);
        b.iter(|| {
            rt.block_on(async {
                let hash = cas.put(&data, "text/plain").await.unwrap();
                black_box(cas.get(&hash).await.unwrap());
            });
        });
    });

    group.bench_function("put_100_entries", |b| {
        b.iter(|| {
            let cas = ContentAddressedStorage::new();
            rt.block_on(async {
                for i in 0..100 {
                    let data = format!("entry-{i}");
                    black_box(cas.put(&data, "text/plain").await.unwrap());
                }
            });
        });
    });

    group.bench_function("hash_only", |b| {
        let data = "x".repeat(1024);
        b.iter(|| {
            black_box(ContentAddressedStorage::hash(&data))
        });
    });

    group.finish();
}

fn bench_hybrid_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("hybrid_search");
    let search = HybridSearch::new();

    group.bench_function("search_1000_drawers", |b| {
        let tax = create_test_taxonomy(10, 10, 10);
        b.iter(|| {
            black_box(search.search("searchable keywords", &tax, &SearchScope::global(10)))
        });
    });

    group.bench_function("search_10000_drawers", |b| {
        let tax = create_test_taxonomy(10, 10, 100);
        b.iter(|| {
            black_box(search.search("searchable keywords", &tax, &SearchScope::global(10)))
        });
    });

    group.bench_function("search_scoped_wing", |b| {
        let tax = create_test_taxonomy(10, 10, 10);
        b.iter(|| {
            black_box(search.search("keywords", &tax, &SearchScope::wing("wing-5", 10)))
        });
    });

    group.finish();
}

fn bench_taxonomy(c: &mut Criterion) {
    let mut group = c.benchmark_group("taxonomy");

    group.bench_function("build_100wing_5room_10drawer", |b| {
        b.iter(|| {
            black_box(create_test_taxonomy(100, 5, 10))
        });
    });

    group.bench_function("find_drawer_5000_drawers", |b| {
        let tax = create_test_taxonomy(10, 10, 50);
        b.iter(|| {
            black_box(tax.find_drawer("drawer_5_5_25"))
        });
    });

    group.bench_function("stats_1000_drawers", |b| {
        let tax = create_test_taxonomy(10, 10, 10);
        b.iter(|| {
            black_box(tax.stats())
        });
    });

    group.finish();
}

fn bench_wal(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("wal");

    group.bench_function("append_100_entries", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().unwrap();
                let wal = WriteAheadLog::new(tmp.path());
                (wal, tmp)
            },
            |(wal, _tmp)| {
                rt.block_on(async {
                    for i in 0..100 {
                        let entry = WalEntry {
                            timestamp: chrono::Utc::now(),
                            operation: "write".to_string(),
                            path: format!("/file-{i}"),
                            size: 1024,
                            session_id: Some("bench".to_string()),
                        };
                        wal.append(&entry).await.unwrap();
                    }
                });
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_drawer_confidence(c: &mut Criterion) {
    let mut group = c.benchmark_group("drawer_confidence");

    group.bench_function("effective_confidence_no_history", |b| {
        let d = Drawer::new("d1".into(), "content".into(), "w".into(), "r".into(), "f".into(), 0);
        b.iter(|| black_box(d.effective_confidence()));
    });

    group.bench_function("effective_confidence_with_100_reinforcements", |b| {
        let mut d = Drawer::new("d1".into(), "content".into(), "w".into(), "r".into(), "f".into(), 0);
        for i in 0..100 {
            d.reinforce(if i % 2 == 0 { 0.5 } else { -0.3 }, format!("ctx-{i}"));
        }
        d.access_count = 500;
        b.iter(|| black_box(d.effective_confidence()));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_mountable_fs,
    bench_memfs,
    bench_context_builder,
    bench_cas,
    bench_hybrid_search,
    bench_taxonomy,
    bench_wal,
    bench_drawer_confidence,
);
criterion_main!(benches);
