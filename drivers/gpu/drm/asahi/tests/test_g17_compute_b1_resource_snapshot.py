#!/usr/bin/env python3
"""Check the default-off B1 retained descriptor/FList discriminator."""

from pathlib import Path


root = Path(__file__).resolve().parents[1]
resources = (root / "g17_resources.rs").read_text()
manager = (root / "g17_manager.rs").read_text()
config = (root / "g17_config.rs").read_text()


def block(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


snapshot = block(resources, "    pub(crate) fn snapshot_compute_submission(")
arm = block(manager, "    fn arm_compute_completion(")
timeout = block(manager, "    fn log_async_compute_timeout_report(") + block(manager, "    pub(crate) fn log_async_compute_timeout(")
completion = block(manager, "    pub(crate) fn poll_staged_independent_compute_completion(")
first = block(manager, "    fn submit_translated_first_bind_channel(")
repeat = block(manager, "    fn submit_translated_repeat_channel(")

assert "pub(crate) const COMPUTE_RESOURCE_SNAPSHOT: u32 = 0;" in config
assert "crate::g17_config::COMPUTE_RESOURCE_SNAPSHOT != 0" in arm
assert arm.index("snapshot_compute_submission(descriptor)?") < arm.index(
    "self.last_published_descriptor = descriptor"
) + 1024

for offset in ["0x22", "0x748", "0xfac", "0xfb0", "0xfb2"]:
    assert offset in snapshot
assert "0x40usize" in snapshot
assert "checked_mul(12)" in snapshot
assert "resource_register" in snapshot

assert "publication_resource_snapshot" in timeout
assert "snapshot_compute_submission(queue.completion_descriptor)" in timeout
assert "snapshot_compute_flist()" in timeout
assert "G17P async-timeout B1 publication:" in timeout
assert "G17P async-timeout B1 halt:" in timeout
assert "G17P async-timeout B1 halt FList:" in timeout
assert "publication/current: publication={:x?}" not in timeout
assert "G17P B1 entry retirement:" in completion
assert "snapshot_compute_entry(last.entry_offset as usize)" in completion
for submit in [first, repeat]:
    assert "G17P B1 entry publication:" in submit
    assert submit.index("snapshot_compute_entry(") < submit.index(
        "queue.producer = prepared_entry.producer_after"
    )

print("PASS default-off B1 descriptor/FList publication-vs-halt snapshot")
