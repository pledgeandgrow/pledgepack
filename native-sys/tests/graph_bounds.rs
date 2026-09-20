//! The safe `Graph` wrapper must never hand an unknown module id to the Zig
//! library, which indexes its module array without bounds checks.

use pledgepack_native_sys::Graph;

#[test]
fn queries_on_unknown_module_ids_return_empty() {
    let g = Graph::new();
    assert!(g.get_dependents(0, 8).is_empty());
    assert!(g.get_dependencies(42, 8).is_empty());
    let a = g.add_module("a.js");
    assert!(g.get_all_dependents(a + 100).is_empty());
    assert!(g.get_all_dependencies(a + 100).is_empty());
}

#[test]
#[should_panic(expected = "unknown module id")]
fn add_dependency_rejects_unknown_ids() {
    let g = Graph::new();
    let a = g.add_module("a.js");
    g.add_dependency(a, a + 5);
}

#[test]
fn known_ids_still_work() {
    let g = Graph::new();
    let a = g.add_module("a.js");
    let b = g.add_module("b.js");
    g.add_dependency(a, b);
    assert_eq!(g.get_all_dependencies(a), vec![b]);
    assert_eq!(g.get_all_dependents(b), vec![a]);
}

#[test]
fn oversized_capacity_is_clamped_not_allocated() {
    // An attacker/bug-controlled capacity used to reach `vec![0u32; capacity]`
    // unchecked: usize::MAX aborts with "capacity overflow", and merely huge
    // values try a multi-GiB allocation.
    let g = Graph::new();
    let a = g.add_module("a.js");
    let b = g.add_module("b.js");
    g.add_dependency(a, b);
    assert_eq!(g.get_dependencies(a, usize::MAX), vec![b]);
    assert_eq!(g.get_dependents(b, usize::MAX / 8), vec![a]);
}

#[test]
fn read_file_rejects_embedded_nul() {
    let err = pledgepack_native_sys::read_file("some\0file.txt").unwrap_err();
    assert!(err.to_string().contains("NUL"), "{err}");
    let res = pledgepack_native_sys::read_files_batch(&["a\0b"]);
    assert!(res[0].is_err());
}

#[test]
fn read_file_and_batch_roundtrip_and_release_buffers() {
    let dir = std::env::temp_dir().join(format!("pledge_native_io_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let one = dir.join("one.txt");
    let empty = dir.join("empty.txt");
    std::fs::write(&one, b"hello native").unwrap();
    std::fs::write(&empty, b"").unwrap();
    let (one_s, empty_s) = (one.to_str().unwrap(), empty.to_str().unwrap());

    // Exercise the free path many times (each call used to leak its buffer).
    for _ in 0..200 {
        assert_eq!(
            pledgepack_native_sys::read_file(one_s).unwrap(),
            b"hello native"
        );
        assert!(
            pledgepack_native_sys::read_file(empty_s)
                .unwrap()
                .is_empty()
        );
        let batch = pledgepack_native_sys::read_files_batch(&[one_s, empty_s]);
        assert_eq!(batch[0].as_ref().unwrap(), b"hello native");
        assert!(batch[1].as_ref().unwrap().is_empty());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
