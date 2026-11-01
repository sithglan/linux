#!/usr/bin/env python3
"""Compile the production render interval validator and audit log ordering."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
completion = (root / "g17_completion.rs").read_text()
manager = (root / "g17_manager.rs").read_text()


def block(source, marker):
    start = source.index(marker)
    brace = source.index("{", start)
    depth = 1
    end = brace + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


state = block(completion, "pub(crate) struct RenderExecutionState")
validator = block(completion, "pub(crate) const fn render_gpu_interval")
tests = r'''
#[test]
fn valid_native_pair_preserves_pass_envelope() {
    let execution = RenderExecutionState {
        job_list_empty: true,
        tiling_timestamps: [10, 10],
        fragment_timestamps: [10, 40],
    };
    assert_eq!(render_gpu_interval(execution), Some([10, 40]));
}

#[test]
fn exact_timestamp_pairs_publish_even_while_a_successor_remains_linked() {
    let good = RenderExecutionState {
        job_list_empty: true,
        tiling_timestamps: [10, 10],
        fragment_timestamps: [10, 40],
    };
    assert_eq!(render_gpu_interval(RenderExecutionState {
        job_list_empty: false, ..good
    }), Some([10, 40]));
    for execution in [
        RenderExecutionState { tiling_timestamps: [0, 0], fragment_timestamps: [0, 40], ..good },
        RenderExecutionState { tiling_timestamps: [10, 11], ..good },
        RenderExecutionState { fragment_timestamps: [11, 40], ..good },
        RenderExecutionState { fragment_timestamps: [10, 10], ..good },
        RenderExecutionState { fragment_timestamps: [10, 9], ..good },
    ] {
        assert_eq!(render_gpu_interval(execution), None);
    }
}
'''

with tempfile.TemporaryDirectory() as directory:
    source = Path(directory) / "test.rs"
    source.write_text("#![allow(dead_code)]\n" + state + validator + tests)
    executable = Path(directory) / "test"
    subprocess.run(["rustc", "--edition=2021", "--test", str(source),
                    "-o", str(executable)], check=True)
    subprocess.run([str(executable), "--test-threads=1"], check=True)

poll = block(manager, "    pub(crate) fn commit_render_observation(")
log = poll.index('"G17P render: GPU-INTERVAL')
promote_tracker = poll.index("self.render_slots.get_mut(pair_slot).tracker = self.render_slots.get_mut(pair_slot).queued_tracker.take()")
assert log < promote_tracker
assert "render_gpu_interval" in poll
assert "binding.vm_id()" in poll and "binding.root()" in poll
assert "queues.tiling.queue_id" in poll and "queues.fragment.queue_id" in poll
assert len(re.findall(r"GPU-INTERVAL", poll)) == 1
print("PASS render GPU interval publication: exact native pass envelope, invalid retirement rejection, VM/root/QID identity, pre-clear/pre-increment ordering")
