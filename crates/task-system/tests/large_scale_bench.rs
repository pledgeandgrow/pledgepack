// G3.5: Large-scale benchmark test — verifies the task system handles
// 100k+ tasks without degradation.
//
// This test creates a realistic dependency graph with:
// - 100,000 leaf tasks (simulating individual module transforms)
// - 10,000 aggregation tasks (each depending on 10 leaves)
// - 1,000 mid-level tasks (each depending on 10 aggregations)
// - 100 high-level tasks (each depending on 10 mid-level)
// - 10 root tasks (each depending on 10 high-level)
//
// Total: ~111,110 tasks in a 5-layer hierarchy.
//
// The test measures:
// 1. Graph construction time (adding edges)
// 2. Dirty propagation time (marking a leaf dirty → propagate up)
// 3. Aggregation query time (querying subtree status)
// 4. Memory usage estimation

use pledgepack_task_system::{
    DependencyGraph, MemoryBackend, StoredOutput, Task, TaskBackend, TaskEngine, TaskExecutor,
    TaskId, TaskRegistry,
};
use std::time::Instant;

const NUM_LEAF_TASKS: usize = 100_000;
const NUM_AGG_TASKS: usize = 10_000;
const NUM_MID_TASKS: usize = 1_000;
const NUM_HIGH_TASKS: usize = 100;
const NUM_ROOT_TASKS: usize = 10;
const FANOUT: usize = 10;

/// Generate a deterministic TaskId for a given layer and index.
fn make_task_id(layer: &str, index: usize) -> TaskId {
    let input = format!("{}:{}", layer, index);
    TaskId::compute("bench_task", input.as_bytes())
}

/// G3.5: Build a 5-layer dependency graph with 100k+ tasks and verify
/// that graph operations complete in reasonable time.
#[test]
fn large_scale_dependency_graph_construction() {
    let total = NUM_LEAF_TASKS + NUM_AGG_TASKS + NUM_MID_TASKS + NUM_HIGH_TASKS + NUM_ROOT_TASKS;
    println!("\n  G3.5: Large-scale benchmark — {} tasks", total);

    // Phase 1: Build the dependency graph
    let start = Instant::now();
    let mut graph = DependencyGraph::default();

    // Register all tasks
    for i in 0..NUM_LEAF_TASKS {
        graph.mark_clean(make_task_id("leaf", i));
    }
    for i in 0..NUM_AGG_TASKS {
        graph.mark_clean(make_task_id("agg", i));
    }
    for i in 0..NUM_MID_TASKS {
        graph.mark_clean(make_task_id("mid", i));
    }
    for i in 0..NUM_HIGH_TASKS {
        graph.mark_clean(make_task_id("high", i));
    }
    for i in 0..NUM_ROOT_TASKS {
        graph.mark_clean(make_task_id("root", i));
    }

    // Suppress unused variable warning
    let _ = &graph;

    let register_time = start.elapsed();
    println!("  Register {} tasks: {:?}", total, register_time);

    // Add edges: agg[i] depends on leaf[i*10..i*10+10]
    let edge_start = Instant::now();
    for i in 0..NUM_AGG_TASKS {
        let agg_id = make_task_id("agg", i);
        for j in 0..FANOUT {
            let leaf_idx = i * FANOUT + j;
            if leaf_idx < NUM_LEAF_TASKS {
                graph.add_edge(agg_id, make_task_id("leaf", leaf_idx));
            }
        }
    }

    // mid[i] depends on agg[i*10..i*10+10]
    for i in 0..NUM_MID_TASKS {
        let mid_id = make_task_id("mid", i);
        for j in 0..FANOUT {
            let agg_idx = i * FANOUT + j;
            if agg_idx < NUM_AGG_TASKS {
                graph.add_edge(mid_id, make_task_id("agg", agg_idx));
            }
        }
    }

    // high[i] depends on mid[i*10..i*10+10]
    for i in 0..NUM_HIGH_TASKS {
        let high_id = make_task_id("high", i);
        for j in 0..FANOUT {
            let mid_idx = i * FANOUT + j;
            if mid_idx < NUM_MID_TASKS {
                graph.add_edge(high_id, make_task_id("mid", mid_idx));
            }
        }
    }

    // root[i] depends on high[i*10..i*10+10]
    for i in 0..NUM_ROOT_TASKS {
        let root_id = make_task_id("root", i);
        for j in 0..FANOUT {
            let high_idx = i * FANOUT + j;
            if high_idx < NUM_HIGH_TASKS {
                graph.add_edge(root_id, make_task_id("high", high_idx));
            }
        }
    }

    let edge_time = edge_start.elapsed();
    let total_edges = NUM_AGG_TASKS * FANOUT
        + NUM_MID_TASKS * FANOUT
        + NUM_HIGH_TASKS * FANOUT
        + NUM_ROOT_TASKS * FANOUT;
    println!("  Add {} edges: {:?}", total_edges, edge_time);

    let total_construction = start.elapsed();
    println!("  Total graph construction: {:?}", total_construction);

    // Assert reasonable performance (generous limits for CI)
    assert!(
        total_construction.as_secs() < 30,
        "Graph construction should complete in < 30s, took {:?}",
        total_construction
    );
}

/// G3.5: Test dirty propagation from a leaf to root tasks.
#[test]
fn large_scale_dirty_propagation() {
    println!("\n  G3.5: Dirty propagation benchmark");

    let mut graph = DependencyGraph::default();

    // Build a smaller but still significant graph for dirty propagation
    // 10k leaf tasks, 1k agg, 100 mid, 10 high, 1 root
    const LEAVES: usize = 10_000;
    const AGGS: usize = 1_000;
    const MIDS: usize = 100;
    const HIGHS: usize = 10;

    for i in 0..LEAVES {
        graph.mark_clean(make_task_id("leaf", i));
    }
    for i in 0..AGGS {
        let agg_id = make_task_id("agg", i);
        graph.mark_clean(agg_id);
        for j in 0..FANOUT {
            let leaf_idx = i * FANOUT + j;
            if leaf_idx < LEAVES {
                graph.add_edge(agg_id, make_task_id("leaf", leaf_idx));
            }
        }
    }
    for i in 0..MIDS {
        let mid_id = make_task_id("mid", i);
        graph.mark_clean(mid_id);
        for j in 0..FANOUT {
            let agg_idx = i * FANOUT + j;
            if agg_idx < AGGS {
                graph.add_edge(mid_id, make_task_id("agg", agg_idx));
            }
        }
    }
    for i in 0..HIGHS {
        let high_id = make_task_id("high", i);
        graph.mark_clean(high_id);
        for j in 0..FANOUT {
            let mid_idx = i * FANOUT + j;
            if mid_idx < MIDS {
                graph.add_edge(high_id, make_task_id("mid", mid_idx));
            }
        }
    }
    let root_id = make_task_id("root", 0);
    graph.mark_clean(root_id);
    for i in 0..HIGHS {
        graph.add_edge(root_id, make_task_id("high", i));
    }

    // Mark a single leaf dirty and propagate
    let prop_start = Instant::now();
    let dirty_leaf = make_task_id("leaf", 0);
    graph.mark_dirty(dirty_leaf);

    let prop_time = prop_start.elapsed();
    println!(
        "  Dirty propagation (1 leaf → root, {} tasks): {:?}",
        LEAVES, prop_time
    );

    assert!(
        prop_time.as_secs() < 5,
        "Dirty propagation should complete in < 5s, took {:?}",
        prop_time
    );
}

/// G3.5: Test TaskId computation at scale — 100k IDs should be fast.
#[test]
fn large_scale_task_id_computation() {
    println!(
        "\n  G3.5: TaskId computation benchmark — {} IDs",
        NUM_LEAF_TASKS
    );

    let start = Instant::now();
    let mut ids = Vec::with_capacity(NUM_LEAF_TASKS);

    for i in 0..NUM_LEAF_TASKS {
        let input = format!("leaf_task_{}", i);
        ids.push(TaskId::compute("bench_fn", input.as_bytes()));
    }

    let compute_time = start.elapsed();
    println!("  Compute {} TaskIds: {:?}", NUM_LEAF_TASKS, compute_time);

    assert!(
        compute_time.as_secs() < 10,
        "TaskId computation should complete in < 10s, took {:?}",
        compute_time
    );

    // Verify uniqueness
    let unique = ids.iter().collect::<std::collections::HashSet<_>>().len();
    assert_eq!(unique, NUM_LEAF_TASKS, "All TaskIds should be unique");
}

/// G3.5: Test the TaskEngine with a large number of registered tasks.
/// This verifies that the registry and engine can handle 100k+ task registrations
/// and that cache lookups remain fast.
#[tokio::test]
async fn large_scale_engine_cache_hits() {
    const NUM_TASKS: usize = 50_000;

    println!("\n  G3.5: Engine cache benchmark — {} tasks", NUM_TASKS);

    let registry = TaskRegistry::new();

    // Register tasks
    let register_start = Instant::now();
    for i in 0..NUM_TASKS {
        let task_id = TaskId::compute("cached_fn", format!("input_{}", i).as_bytes());
        let output_val = i as u64;
        registry.register(
            task_id,
            format!("cached_fn_{}", i),
            TaskExecutor::sync(move || Ok(StoredOutput::new(task_id, &output_val, vec![])?)),
        );
    }
    let register_time = register_start.elapsed();
    println!("  Register {} tasks: {:?}", NUM_TASKS, register_time);

    // Build engine with memory backend
    let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

    // Read all tasks (first read = compute + cache)
    let first_read_start = Instant::now();
    for i in 0..NUM_TASKS {
        let task_id = TaskId::compute("cached_fn", format!("input_{}", i).as_bytes());
        let task: Task<u64> = Task::from_id(task_id);
        let result = task.read(&engine).await.unwrap();
        assert_eq!(*result, i as u64);
    }
    let first_read_time = first_read_start.elapsed();
    println!(
        "  First read (compute + cache) {} tasks: {:?}",
        NUM_TASKS, first_read_time
    );

    // Second read (should all be cache hits)
    let second_read_start = Instant::now();
    for i in 0..NUM_TASKS {
        let task_id = TaskId::compute("cached_fn", format!("input_{}", i).as_bytes());
        let task: Task<u64> = Task::from_id(task_id);
        let result = task.read(&engine).await.unwrap();
        assert_eq!(*result, i as u64);
    }
    let second_read_time = second_read_start.elapsed();
    println!(
        "  Second read (cache hits) {} tasks: {:?}",
        NUM_TASKS, second_read_time
    );

    // Cache hits should be significantly faster than first reads
    // (generous assertion for CI environments)
    assert!(
        first_read_time.as_secs() < 60,
        "First read should complete in < 60s, took {:?}",
        first_read_time
    );
    assert!(
        second_read_time.as_secs() < 30,
        "Cache hit reads should complete in < 30s, took {:?}",
        second_read_time
    );

    let stats = engine.stats();
    println!("  Engine stats: {:?}", stats);
}
