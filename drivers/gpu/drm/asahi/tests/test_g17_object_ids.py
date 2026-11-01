#!/usr/bin/env python3
"""Exercise the production object-ID allocator without a running kernel."""

from pathlib import Path
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
resources = (ROOT / "g17_resources.rs").read_text()
render = (ROOT / "g17_render.rs").read_text()


def function(source, name):
    match = re.search(r"^(?:pub\(crate\) )?(?:const )?fn " + name + r"\b", source, re.M)
    assert match, name
    end = source.index("{", match.end()) + 1
    depth = 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[match.start():end]


source = "\n".join([
    "use std::sync::atomic::{AtomicU32, Ordering};",
    "type Result<T> = std::result::Result<T, i32>;",
    "const EOVERFLOW: i32 = 75;",
    re.search(r"^static G17P_RENDER_GID_COUNTER:.*;", resources, re.M).group(),
    "mod g17_render {",
    function(render, "g17p_render_lifecycle_pair"),
    "}",
    *[function(resources, name) for name in [
        "g17p_render_gid_add", "g17p_allocate_object_id",
        "g17p_allocate_render_lifecycle_pair",
    ]],
    r'''
fn record_pair(predecessor: u32, ids: &mut Vec<u32>) {
    let (fragment, tiling) = g17p_allocate_render_lifecycle_pair(predecessor).unwrap();
    assert_eq!((fragment >> 32) as u32, predecessor);
    assert_eq!((tiling >> 32) as u32, predecessor);
    assert!(fragment as u32 > predecessor);
    assert_eq!(tiling as u32, fragment as u32 + 1);
    ids.extend([fragment as u32, tiling as u32]);
}

#[test]
fn object_ids_survive_client_churn_and_remain_unique() {
    // A new graph followed by a retained submission. The old allocator fails
    // at approximately client 255 despite almost all reserved IDs being unused.
    let mut ids = Vec::new();
    for _ in 0..100_000 {
        let predecessor = g17p_allocate_object_id().unwrap();
        ids.push(predecessor);
        record_pair(predecessor, &mut ids);
        record_pair(predecessor, &mut ids);
    }
    assert_eq!(G17P_RENDER_GID_COUNTER.load(Ordering::Acquire), 500_001);
    assert!(ids.iter().copied().eq(1..500_001));

    // Pool-owner and command allocations share the same atomic namespace.
    let mut threads = Vec::new();
    for _ in 0..8 {
        threads.push(std::thread::spawn(|| {
            let mut ids = Vec::new();
            for _ in 0..2048 {
                let predecessor = g17p_allocate_object_id().unwrap();
                ids.push(predecessor);
                record_pair(predecessor, &mut ids);
            }
            ids
        }));
    }
    let mut concurrent = Vec::new();
    for thread in threads {
        concurrent.extend(thread.join().unwrap());
    }
    concurrent.sort_unstable();
    let next = 500_001 + concurrent.len() as u32;
    assert!(concurrent.into_iter().eq(500_001..next));
    assert_eq!(G17P_RENDER_GID_COUNTER.load(Ordering::Acquire), next);

    // Do not wrap into IDs that could still belong to retained firmware state.
    G17P_RENDER_GID_COUNTER.store(u32::MAX - 3, Ordering::Release);
    let predecessor = g17p_allocate_object_id().unwrap();
    record_pair(predecessor, &mut Vec::new());
    assert_eq!(g17p_allocate_object_id(), Err(EOVERFLOW));
    assert_eq!(g17p_allocate_render_lifecycle_pair(predecessor), Err(EOVERFLOW));
    assert_eq!(G17P_RENDER_GID_COUNTER.load(Ordering::Acquire), u32::MAX);
}
''',
])

with tempfile.TemporaryDirectory(prefix="g17-object-ids-") as tmp:
    source_path = Path(tmp) / "test.rs"
    binary = Path(tmp) / "test"
    source_path.write_text(source)
    subprocess.run(["rustc", "--edition=2021", "--test", str(source_path),
                    "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
