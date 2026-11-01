#!/usr/bin/env python3
"""Keep the native per-submit usage record identity and publication ordering exact."""
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
compute = (ROOT / "g17_compute.rs").read_text()
resources = (ROOT / "g17_resources.rs").read_text()
manager = (ROOT / "g17_manager.rs").read_text()


def block(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


objects = block(compute, "pub(crate) struct ComputeDescriptorObjects")
assert "usage_record: u64" in objects
assert "zero_page" not in objects

descriptor = block(compute, "pub(crate) fn build_compute_descriptor(")
assert "put_u64(raw, 0x0fcb, objects.usage_record);" in descriptor

address = block(resources, "pub(crate) fn compute_usage_record_gpu_va(")
assert "self.robustness.iova()" in address

descriptor_read = block(resources, "pub(crate) fn read_compute_descriptor_usage_slot(")
assert "self.read_compute_usage_slot()" in descriptor_read
assert "self.support" not in descriptor_read

assert manager.count("usage_record: queue.compute.compute_usage_record_gpu_va(),") == 3
assert manager.count("queue.compute.clear_compute_usage_slot()?;") == 3
for build_marker in (
    "fn submit_translated_first_bind_channel(",
    "fn submit_translated_repeat_channel(",
    "pub(crate) fn submit_translated_compute(",
):
    submission = block(manager, build_marker)
    clear = submission.index("queue.compute.clear_compute_usage_slot()?;")
    publish = submission.index("with_descriptor_mut")
    assert clear < publish

print("PASS compute usage record: one identity, mandatory clear before publication")
