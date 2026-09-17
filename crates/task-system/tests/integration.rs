// Integration tests for the task system — exercises the #[task] macro,
// dependency chains, caching, invalidation, and the demand-driven scheduler.

use pledgepack_task_system::{
    MemoryBackend, StoredOutput, Task, TaskBackend, TaskEngine, TaskEngineBuilder, TaskExecutor,
    TaskId, TaskRegistry,
};

/// Test that TaskId is deterministic for the same inputs.
#[test]
fn task_id_determinism_across_constructions() {
    let id1 = TaskId::compute("my_function", b"input_data");
    let id2 = TaskId::compute("my_function", b"input_data");
    let id3 = TaskId::compute("my_function", b"different_data");

    assert_eq!(id1, id2, "Same inputs must produce same TaskId");
    assert_ne!(id1, id3, "Different inputs must produce different TaskId");
}

/// Test that a simple task computes and caches correctly.
#[tokio::test]
async fn simple_task_compute_and_cache() {
    let registry = TaskRegistry::new();
    let task_id = TaskId::compute("double", b"21");

    registry.register(
        task_id,
        "double".to_string(),
        TaskExecutor::sync(move || Ok(StoredOutput::new(task_id, &84u32, vec![])?)),
    );

    let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

    let task: Task<u32> = Task::from_id(task_id);
    let result = task.read(&engine).await.unwrap();
    assert_eq!(*result, 84);

    // Second read should hit cache
    let result2 = task.read(&engine).await.unwrap();
    assert_eq!(*result2, 84);
}

/// Test that disk cache persists across engine instances.
#[tokio::test]
async fn disk_cache_persists_across_engines() {
    let tmp = tempfile::tempdir().unwrap();
    let disk = pledgepack_task_system::DiskBackend::new(tmp.path().to_path_buf()).unwrap();

    let registry = TaskRegistry::new();
    let task_id = TaskId::compute("persisted_value", b"");

    registry.register(
        task_id,
        "persisted_value".to_string(),
        TaskExecutor::sync(move || {
            Ok(StoredOutput::new(
                task_id,
                &"persisted".to_string(),
                vec![],
            )?)
        }),
    );

    // First engine: compute and store to disk
    let engine1 = TaskEngineBuilder::new(registry).with_disk(disk).build();

    let task: Task<String> = Task::from_id(task_id);
    let result1 = task.read(&engine1).await.unwrap();
    assert_eq!(*result1, "persisted");

    // Second engine: should load from disk (memory is empty)
    let registry2 = TaskRegistry::new();
    registry2.register(
        task_id,
        "persisted_value".to_string(),
        TaskExecutor::sync(move || {
            // This should NOT be called if disk cache works
            Ok(StoredOutput::new(
                task_id,
                &"recomputed".to_string(),
                vec![],
            )?)
        }),
    );

    let disk2 = pledgepack_task_system::DiskBackend::new(tmp.path().to_path_buf()).unwrap();
    let engine2 = TaskEngineBuilder::new(registry2).with_disk(disk2).build();

    let result2 = task.read(&engine2).await.unwrap();
    assert_eq!(
        *result2, "persisted",
        "Should load from disk, not recompute"
    );
}

/// Test that invalidation marks tasks dirty and they get recomputed on next read.
#[tokio::test]
async fn invalidation_and_recompute() {
    let registry = TaskRegistry::new();
    let task_id = TaskId::compute("mutable_value", b"");

    let value = std::sync::Arc::new(std::sync::Mutex::new("v1".to_string()));
    let value_clone = value.clone();

    registry.register(
        task_id,
        "mutable_value".to_string(),
        TaskExecutor::sync(move || {
            let v = value_clone.lock().unwrap().clone();
            Ok(StoredOutput::new(task_id, &v, vec![])?)
        }),
    );

    let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

    let task: Task<String> = Task::from_id(task_id);
    let r1 = task.read(&engine).await.unwrap();
    assert_eq!(*r1, "v1");

    // Change the value
    *value.lock().unwrap() = "v2".to_string();

    // Invalidate the task
    engine.invalidate(task_id);

    // Read again — should recompute
    let r2 = task.read(&engine).await.unwrap();
    assert_eq!(*r2, "v2");
}

/// Test that the aggregation graph tracks dirty counts correctly.
#[test]
fn aggregation_graph_dirty_propagation() {
    use pledgepack_task_system::{AggregationGraph, DependencyGraph};

    let dep_graph = DependencyGraph::new();
    let agg_graph = AggregationGraph::new();

    // Build a chain: a → b → c → d
    let a = TaskId::compute("a", b"1");
    let b = TaskId::compute("b", b"2");
    let c = TaskId::compute("c", b"3");
    let d = TaskId::compute("d", b"4");

    dep_graph.add_edge(a, b);
    dep_graph.add_edge(b, c);
    dep_graph.add_edge(c, d);

    agg_graph.build_from(&dep_graph);

    // Initially all clean
    assert_eq!(agg_graph.subtree_dirty_count(&a), 0);
    assert_eq!(agg_graph.subtree_total_count(&a), 4);

    // Mark d dirty — should propagate up
    dep_graph.set_status(d, pledgepack_task_system::TaskStatus::Dirty);
    agg_graph.mark_dirty(d, &dep_graph);

    // a's subtree should now have dirty tasks
    assert!(agg_graph.subtree_dirty_count(&a) > 0);
    assert!(!agg_graph.is_subtree_clean(&a));
}

/// Test that active queries filter dirty tasks correctly.
#[test]
fn active_query_filters_dirty_tasks() {
    let registry = TaskRegistry::new();

    let root_id = TaskId::compute("root", b"");
    let child_id = TaskId::compute("child", b"");

    registry.register(
        child_id,
        "child".to_string(),
        TaskExecutor::sync(move || {
            Ok(StoredOutput::new(
                child_id,
                &"child_value".to_string(),
                vec![],
            )?)
        }),
    );

    registry.register(
        root_id,
        "root".to_string(),
        TaskExecutor::sync(move || {
            Ok(StoredOutput::new(
                root_id,
                &"root_value".to_string(),
                vec![child_id],
            )?)
        }),
    );

    let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

    // Register an active query for root
    let query_id = engine.register_query(vec![root_id]);

    // Initially no dirty tasks
    let dirty = engine.dirty_tasks_for_active_queries();
    assert_eq!(dirty.len(), 0);

    // Invalidate root
    engine.invalidate(root_id);

    // Now root should be dirty and in an active query
    let dirty = engine.dirty_tasks_for_active_queries();
    assert!(dirty.contains(&root_id));

    // Unregister the query
    engine.unregister_query(query_id);

    // Now dirty tasks should not be in any active query
    let dirty = engine.dirty_tasks_for_active_queries();
    assert_eq!(dirty.len(), 0);
}

/// Test that the #[task] macro generates correct TaskId computation.
#[test]
fn task_macro_generates_task_id() {
    use pledgepack_task_system::task;

    // Define a task function using the macro
    #[task]
    fn my_task(input: String) -> Task<u32> {
        // The body is in the *_impl function; the wrapper just computes the TaskId
        let _ = input;
        42
    }

    // The wrapper should return a Task<u32> with a deterministic TaskId
    let t1 = my_task("hello".to_string());
    let t2 = my_task("hello".to_string());
    let t3 = my_task("world".to_string());

    assert_eq!(t1.id(), t2.id(), "Same inputs → same TaskId");
    assert_ne!(t1.id(), t3.id(), "Different inputs → different TaskId");
}

/// Test that Task<T> is 16 bytes (Copy, no heap allocation).
#[test]
fn task_is_compact() {
    assert_eq!(
        std::mem::size_of::<Task<u32>>(),
        16,
        "Task<T> must be 16 bytes (just the TaskId)"
    );
    assert_eq!(
        std::mem::size_of::<Task<String>>(),
        16,
        "Task<T> size is independent of T"
    );
}

// ============ Environment-Aware TaskId Tests (G5.1-G5.3) ============

/// Test that environment-aware TaskId differs from non-environment TaskId.
#[test]
fn env_aware_task_id_differs_from_plain() {
    use pledgepack_task_system::Environment;

    let plain = TaskId::compute("my_func", b"input");
    let client = TaskId::compute_with_env("my_func", b"input", Environment::Client);
    let server = TaskId::compute_with_env("my_func", b"input", Environment::Server);

    assert_ne!(plain, client, "Environment-aware ID must differ from plain");
    assert_ne!(client, server, "Client and Server IDs must differ");
}

/// Test that same environment produces same TaskId.
#[test]
fn env_aware_task_id_deterministic() {
    use pledgepack_task_system::Environment;

    let id1 = TaskId::compute_with_env("render", b"data", Environment::Client);
    let id2 = TaskId::compute_with_env("render", b"data", Environment::Client);
    assert_eq!(id1, id2, "Same env + inputs → same TaskId");
}

/// Test that different environments produce different TaskIds for same function+input.
#[test]
fn env_aware_task_id_all_environments_differ() {
    use pledgepack_task_system::Environment;

    let func = "transform";
    let input = b"source";
    let client = TaskId::compute_with_env(func, input, Environment::Client);
    let server = TaskId::compute_with_env(func, input, Environment::Server);
    let edge = TaskId::compute_with_env(func, input, Environment::Edge);
    let worker = TaskId::compute_with_env(func, input, Environment::Worker);
    let shared = TaskId::compute_with_env(func, input, Environment::Shared);

    let mut ids = vec![client, server, edge, worker, shared];
    let len_before = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(
        len_before,
        ids.len(),
        "All 5 environment IDs must be unique"
    );
}

/// Test that from_tasks_with_env produces correct environment-aware TaskId.
#[test]
fn from_tasks_with_env_produces_valid_id() {
    use pledgepack_task_system::Environment;

    let child = TaskId::compute("child", b"x");
    let parent = TaskId::from_tasks_with_env("parent", &[child], Environment::Server);

    let child2 = TaskId::compute("child", b"x");
    let parent2 = TaskId::from_tasks_with_env("parent", &[child2], Environment::Server);

    assert_eq!(parent, parent2, "Same children + env → same parent ID");

    let parent_client = TaskId::from_tasks_with_env("parent", &[child], Environment::Client);
    assert_ne!(parent, parent_client, "Different env → different parent ID");
}

/// Test environment thread-local context.
#[test]
fn environment_thread_local_context() {
    use pledgepack_task_system::{Environment, current_environment, run_with_environment};

    assert_eq!(current_environment(), Environment::Shared);

    run_with_environment(Environment::Server, || {
        assert_eq!(current_environment(), Environment::Server);
    });

    assert_eq!(
        current_environment(),
        Environment::Shared,
        "Should restore after run"
    );
}

// ============ Read Tracker Tests (G5.4-G5.5 integration) ============

/// Test that read tracker records file reads and they can be collected.
#[test]
fn read_tracker_integration() {
    use pledgepack_task_system::ReadTracker;

    let mut tracker = ReadTracker::new();
    tracker.activate();
    tracker.record_read("src/components/Button.tsx");
    tracker.record_read("src/utils/helpers.ts");
    tracker.record_read("src/components/Button.tsx"); // dedup

    assert_eq!(tracker.len(), 2);
    let reads = tracker.reads();
    assert!(
        reads
            .iter()
            .any(|p| p.to_string_lossy().ends_with("Button.tsx"))
    );
    assert!(
        reads
            .iter()
            .any(|p| p.to_string_lossy().ends_with("helpers.ts"))
    );
}

/// Test that read tracker can be installed and collected for async contexts.
#[tokio::test]
async fn read_tracker_async_install_collect() {
    use pledgepack_task_system::{collect_tracker, install_tracker, record_read};

    install_tracker();
    record_read("src/app.tsx");
    record_read("src/routes/home.tsx");

    let reads = collect_tracker();
    assert_eq!(reads.len(), 2);
    let read_paths = reads.reads();
    assert!(
        read_paths
            .iter()
            .any(|p| p.to_string_lossy().ends_with("app.tsx"))
    );
    assert!(
        read_paths
            .iter()
            .any(|p| p.to_string_lossy().ends_with("home.tsx"))
    );
}

// ============ Route Tracker Tests (G6.1-G6.3) ============

/// Test that route tracker records modules per route.
#[tokio::test]
async fn route_tracker_records_modules() {
    use pledgepack_task_system::{RouteTracker, RouteTrackerConfig};

    let tracker = RouteTracker::new(RouteTrackerConfig::default());

    tracker.record_module("/about", "src/pages/about.tsx").await;
    tracker
        .record_module("/about", "src/components/Header.tsx")
        .await;
    tracker.record_module("/", "src/pages/index.tsx").await;

    let about_modules = tracker.modules_for_route("/about").await.unwrap();
    assert_eq!(about_modules.len(), 2);

    let root_modules = tracker.modules_for_route("/").await.unwrap();
    assert_eq!(root_modules.len(), 1);
}

/// Test that route tracker marks routes active/inactive.
#[tokio::test]
async fn route_tracker_active_inactive() {
    use pledgepack_task_system::{RouteTracker, RouteTrackerConfig};

    let tracker = RouteTracker::new(RouteTrackerConfig::default());

    tracker.mark_active("/dashboard").await;
    tracker.mark_active("/settings").await;

    assert_eq!(tracker.active_routes().await.len(), 2);

    tracker.mark_inactive("/dashboard").await;
    assert_eq!(tracker.active_routes().await.len(), 1);
}

/// Test that route tracker LRU eviction removes oldest inactive routes.
#[tokio::test]
async fn route_tracker_lru_eviction() {
    use pledgepack_task_system::{RouteTracker, RouteTrackerConfig};

    let config = RouteTrackerConfig {
        max_routes: 2,
        max_total_modules: 100,
        enable_prediction: false,
        prediction_history_size: 10,
    };
    let tracker = RouteTracker::new(config);

    // Add 3 routes; the oldest inactive one should be evicted
    tracker.record_module("/old", "src/old.tsx").await;
    tracker.mark_active("/old").await;
    tracker.mark_inactive("/old").await;

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    tracker.record_module("/mid", "src/mid.tsx").await;
    tracker.mark_active("/mid").await;
    tracker.mark_inactive("/mid").await;

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    tracker.record_module("/new", "src/new.tsx").await;
    tracker.mark_active("/new").await; // active — won't be evicted

    // Evict — should remove /old (oldest inactive)
    let evicted = tracker.evict_lru().await;
    assert_eq!(evicted.len(), 1);
    assert_eq!(evicted[0].0, "/old");
    assert_eq!(tracker.route_count().await, 2);
}

/// Test that route tracker predicts next routes from navigation history.
#[tokio::test]
async fn route_tracker_prediction() {
    use pledgepack_task_system::{RouteTracker, RouteTrackerConfig};

    let config = RouteTrackerConfig {
        max_routes: 100,
        max_total_modules: 1000,
        enable_prediction: true,
        prediction_history_size: 50,
    };
    let tracker = RouteTracker::new(config);

    // Simulate navigation: / → /about → / → /about → /contact
    tracker.mark_active("/").await;
    tracker.mark_active("/about").await;
    tracker.mark_active("/").await;
    tracker.mark_active("/about").await;
    tracker.mark_active("/contact").await;

    let predictions = tracker.predict_next_routes("/").await;
    assert!(!predictions.is_empty());
    assert_eq!(
        predictions[0], "/about",
        "Most common next route after / should be /about"
    );
}

/// Test that route tracker marks prefetched routes.
#[tokio::test]
async fn route_tracker_prefetch() {
    use pledgepack_task_system::{RouteTracker, RouteTrackerConfig};

    let tracker = RouteTracker::new(RouteTrackerConfig::default());

    tracker.mark_prefetched("/blog").await;
    tracker.mark_prefetched("/blog").await;

    let prefetched = tracker.prefetched_routes().await;
    assert_eq!(prefetched.len(), 1);
    assert_eq!(prefetched[0], "/blog");
}
