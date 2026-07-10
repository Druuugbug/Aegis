use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::sync::Arc;
use anyhow::Result;
use async_trait::async_trait;
use aegis_core::graph::{
    GraphBuilder, GraphState, GraphNode,
    LastValue, Aggregate, Channel,
    InMemoryCheckpointSaver, Checkpoint, CheckpointSaver,
};

#[derive(Clone, Default)]
struct BenchState {
    counter: usize,
    data: Vec<String>,
}

impl GraphState for BenchState {
    fn merge(&mut self, mut other: Self) {
        self.counter += other.counter;
        self.data.append(&mut other.data);
    }
}

struct NoopNode(String);
#[async_trait]
impl GraphNode<BenchState> for NoopNode {
    async fn execute(&self, _state: &mut BenchState) -> Result<()> {
        Ok(())
    }
    fn name(&self) -> &str {
        &self.0
    }
}

struct CountNode(String);
#[async_trait]
impl GraphNode<BenchState> for CountNode {
    async fn execute(&self, state: &mut BenchState) -> Result<()> {
        state.counter += 1;
        Ok(())
    }
    fn name(&self) -> &str {
        &self.0
    }
}

fn build_chain(n: usize) -> aegis_core::graph::Graph<BenchState> {
    let mut builder = GraphBuilder::new().set_entry("n0");
    for i in 0..n {
        builder = builder.add_node(format!("n{i}"), Arc::new(NoopNode(format!("n{i}"))));
        if i < n - 1 {
            builder = builder.add_edge(format!("n{i}"), format!("n{}", i + 1));
        }
    }
    builder.compile().unwrap()
}

fn bench_graph_builder_compile(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_builder");

    group.bench_function("compile_10_nodes", |b| {
        b.iter(|| black_box(build_chain(10)));
    });

    group.bench_function("compile_100_nodes", |b| {
        b.iter(|| black_box(build_chain(100)));
    });

    group.bench_function("compile_with_conditional_edges", |b| {
        b.iter(|| {
            let graph = GraphBuilder::new()
                .set_entry("start")
                .add_node("start", Arc::new(NoopNode("start".into())))
                .add_node("a", Arc::new(NoopNode("a".into())))
                .add_node("b", Arc::new(NoopNode("b".into())))
                .add_conditional_edge("start", |s: &BenchState| {
                    if s.counter > 0 { "a".to_string() } else { "b".to_string() }
                })
                .add_edge("a", "__end__")
                .add_edge("b", "__end__")
                .compile()
                .unwrap();
            black_box(graph)
        });
    });

    group.finish();
}

fn bench_graph_execute(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("graph_execute");

    group.bench_function("chain_10_noop_nodes", |b| {
        let graph = build_chain(10);
        b.iter(|| {
            let mut state = BenchState::default();
            rt.block_on(graph.execute(&mut state)).unwrap();
            black_box(state)
        });
    });

    group.bench_function("chain_50_count_nodes", |b| {
        let mut builder = GraphBuilder::new().set_entry("n0");
        for i in 0..50 {
            builder = builder.add_node(format!("n{i}"), Arc::new(CountNode(format!("n{i}"))));
            if i < 49 {
                builder = builder.add_edge(format!("n{i}"), format!("n{}", i + 1));
            }
        }
        let graph = builder.compile().unwrap();
        b.iter(|| {
            let mut state = BenchState::default();
            rt.block_on(graph.execute(&mut state)).unwrap();
            black_box(state)
        });
    });

    group.finish();
}

fn bench_channels(c: &mut Criterion) {
    let mut group = c.benchmark_group("channels");

    group.bench_function("last_value_1000_updates", |b| {
        b.iter(|| {
            let mut lv = LastValue::<usize>::default();
            for i in 0..1000 {
                lv.update(vec![i]);
            }
            black_box(lv.consume())
        });
    });

    group.bench_function("last_value_checkpoint_roundtrip", |b| {
        b.iter(|| {
            let mut lv = LastValue::<usize>::default();
            lv.update(vec![42]);
            let cp = lv.checkpoint();
            black_box(cp)
        });
    });

    group.bench_function("aggregate_sum_1000", |b| {
        b.iter(|| {
            let mut agg = Aggregate::<usize, _>::new(0, |a, b| a + b);
            for i in 0..1000 {
                agg.update(vec![i]);
            }
            black_box(agg.consume())
        });
    });

    group.finish();
}

fn bench_checkpoint(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("checkpoint");

    group.bench_function("save_and_latest", |b| {
        let saver = InMemoryCheckpointSaver::new();
        let mut counter = 0u64;
        b.iter(|| {
            counter += 1;
            let mut ck = Checkpoint::new();
            ck.channel_values.insert(
                "counter".to_string(),
                aegis_core::graph::ChannelCheckpoint {
                    kind: "LastValue".to_string(),
                    data: serde_json::json!(counter),
                },
            );
            rt.block_on(async {
                saver.put(&ck).await.unwrap();
                black_box(saver.latest().await.unwrap());
            });
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_graph_builder_compile,
    bench_graph_execute,
    bench_channels,
    bench_checkpoint,
);
criterion_main!(benches);
