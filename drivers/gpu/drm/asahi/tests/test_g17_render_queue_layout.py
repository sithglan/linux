#!/usr/bin/env python3
"""Execute QID/placement routing and prove all 128 records avoid live metadata."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / "g17_resources.rs").read_text()


def item(marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


names = [
    "G17P_RENDER_QUEUE_METADATA_BASE", "G17P_RENDER_QUEUE_GRAPH_SIZE",
    "G17P_RENDER_QUEUE_RECORD_SIZE", "G17P_RENDER_QUEUE_VA_STRIDE",
    "G17P_USER_TILING_POINTERS", "G17P_RENDER_POINTER_BLOCK_SIZE",
    "G17P_USER_TILING_RING", "G17P_USER_TILING_OPTIONAL", "G17P_USER_TILING_EVENT",
    "G17P_USER_FRAGMENT_OPTIONAL", "G17P_USER_FRAGMENT_EVENT",
    "G17P_USER_TILING_ENTRY_SIGNAL", "G17P_USER_FRAGMENT_ENTRY_SIGNAL",
    "G17P_RENDER_OPTIONAL_STRIDE", "G17P_RENDER_EVENT_STRIDE",
    "G17P_RENDER_DESCRIPTOR_SLOT_COUNT",
]
constants = "\n".join(re.search(
    r"(?:pub\(crate\) )?const " + name + r": [^;]+;", source
).group() for name in names)
storage = item("impl G17PUserRenderStorage {")
local_constants = "\n".join(re.search(
    r"const " + name + r": [^;]+;", storage
).group() for name in ["FRAGMENT_POINTERS", "JOB_LIST", "FRAGMENT_RING"])
body = f'''#![allow(dead_code)]
{constants}
const G17P_RENDER_JOB_QUEUE_LOW_BASE: u64 = 0x70_01ef_8000;
{local_constants}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
{item('pub(crate) struct G17PRenderQueuePair {')}
{item('impl G17PRenderQueuePair {')}
#[test] fn every_qid_record_is_disjoint_from_every_channel_metadata_region() {{
    let regions = [
        (G17P_USER_TILING_POINTERS, G17P_RENDER_POINTER_BLOCK_SIZE),
        (FRAGMENT_POINTERS, G17P_RENDER_POINTER_BLOCK_SIZE),
        (JOB_LIST, 0x18),
        (G17P_USER_TILING_ENTRY_SIGNAL, 0x40),
        (G17P_USER_FRAGMENT_ENTRY_SIGNAL, 0x40),
        (G17P_USER_TILING_RING, 0x4000),
        (FRAGMENT_RING, 0x4000),
        (G17P_USER_TILING_OPTIONAL, G17P_RENDER_DESCRIPTOR_SLOT_COUNT * G17P_RENDER_OPTIONAL_STRIDE),
        (G17P_USER_FRAGMENT_OPTIONAL, G17P_RENDER_DESCRIPTOR_SLOT_COUNT * G17P_RENDER_OPTIONAL_STRIDE),
        (G17P_USER_TILING_EVENT, G17P_RENDER_DESCRIPTOR_SLOT_COUNT * G17P_RENDER_EVENT_STRIDE),
        (G17P_USER_FRAGMENT_EVENT, G17P_RENDER_DESCRIPTOR_SLOT_COUNT * G17P_RENDER_EVENT_STRIDE),
    ];
    for (index, (start, size)) in regions.iter().copied().enumerate() {{
        assert!(start + size <= G17P_RENDER_QUEUE_GRAPH_SIZE);
        for (other, length) in regions[..index].iter().copied() {{
            assert!(start + size <= other || other + length <= start);
        }}
        for qid in 0..128 {{
            let pair = G17PRenderQueuePair::with_queue_ids(0, qid, (qid+1)%128).unwrap();
            let record = pair.record_offset(true);
            assert!(record + G17P_RENDER_QUEUE_RECORD_SIZE <= start);
        }}
    }}
}}
#[test] fn arbitrary_engine_ids_do_not_relocate_live_memory_placement() {{
    for slot in 0..G17PRenderQueuePair::SLOT_COUNT {{
        for ta in 0..128 {{
            let d3 = (ta + 65) % 128;
            let pair = G17PRenderQueuePair::with_queue_ids(slot, ta, d3).unwrap();
            assert_eq!(pair.slot(), slot);
            assert!(pair.dynamic_placement);
            if let Some(original) = G17PRenderQueuePair::for_slot(slot) {{
                assert_eq!(pair.low_va(true), original.low_va(true));
                assert_eq!(pair.low_va(false), original.low_va(false));
            }}
            assert_eq!(pair.record_offset(true), ta as usize * 0xc0);
            assert_eq!(pair.record_offset(false), d3 as usize * 0xc0);
            let masks = pair.completion_masks().unwrap();
            assert_eq!(masks[0].count_ones() + masks[1].count_ones(), 2);
            assert_ne!(masks[ta as usize/64] & (1 << (ta%64)), 0);
            assert_ne!(masks[d3 as usize/64] & (1 << (d3%64)), 0);
        }}
    }}
    assert!(G17PRenderQueuePair::with_queue_ids(0, 6, 6).is_none());
    assert!(G17PRenderQueuePair::with_queue_ids(0, 0, 128).is_none());
    assert!(G17PRenderQueuePair::with_queue_ids(64, 0, 1).is_none());
}}
'''
with tempfile.TemporaryDirectory(prefix="g17-render-layout-") as tmp:
    src = Path(tmp) / "test.rs"
    binary = Path(tmp) / "test"
    src.write_text(body)
    subprocess.run(["rustc", "--edition=2021", "--test", str(src), "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
