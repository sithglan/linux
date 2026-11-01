#!/usr/bin/env python3
"""Stress retained render-slot reuse without losing one pair identity."""

from pathlib import Path
import re


ROOT = Path(__file__).resolve().parents[5]
DRM = (ROOT / "drivers/gpu/drm/asahi/g17_drm.rs").read_text()
LIVE = (ROOT / "drivers/gpu/drm/asahi/g17_live_boot.rs").read_text()
MANAGER = (ROOT / "drivers/gpu/drm/asahi/g17_manager.rs").read_text()
RESOURCES = (ROOT / "drivers/gpu/drm/asahi/g17_resources.rs").read_text()


def block(source: str, start: str, end: str) -> str:
    begin = source.index(start)
    return source[begin : source.index(end, begin)]


pair_source = block(
    RESOURCES,
    "pub(crate) struct G17PRenderQueuePair {",
    "const _: () = assert!(G17PRenderQueuePair::for_slot(1)",
)
assert "let tiling = slot as u16 * 2" in pair_source
assert "fragment: tiling + 1" in pair_source
assert "self.placement_slot" in pair_source
assert "self.tiling >= 128 || self.fragment >= 128" in pair_source
assert "masks[self.tiling as usize / 64]" in pair_source
assert "masks[self.fragment as usize / 64]" in pair_source

# Production routing must derive every externally visible identity from the
# one pair selected by the DRM lease. These anchors deliberately cross layer
# boundaries: losing any one of them recreates a mixed-QID submission.
submit = block(
    LIVE,
    "pub(crate) fn submit_translated_render(",
    "fn ensure_g17p_render_registration_power(",
)
assert ".render_queue_pair(render_slot)" in submit
begin_call = submit.index(".begin_translated_render(")
assert submit.index("&drm", begin_call) < submit.index("vm", begin_call)
assert submit.index("vm", begin_call) < submit.index("queue_pair", begin_call)
assert "ensure_g17p_render_registration_power(manager, &registers)?" in submit
assert "self.install_render_completion(render_slot, completion)?" in submit

begin = block(
    MANAGER,
    "fn begin_translated_render_with_hardware_buffer(",
    "pub(crate) fn publish_default_render_pair_after_freelist(",
)
for route in (
    "self.new_render_sksm_queues(pair, hardware_buffer_id, entry_vas)",
    "publish_render_qos_queue_record(fragment_header)",
    "publish_render_qos_queue_record(header)",
    ".publish_partial_opening_pair(\n                true,\n                queue_pair,",
):
    assert re.sub(r"\s+", "", route) in re.sub(r"\s+", "", begin), route

registration = block(
    MANAGER,
    "fn configure_fresh_nonbootstrap_render_pair(",
    "fn publish_render_sksm_group(",
)
assert "queues.fragment.config.write_pair()" in registration
assert "queues.tiling.config.write_pair()" in registration
assert "pair.tiling" in registration and "pair.fragment" in registration
assert "qid2" not in registration.lower() and "qid3" not in registration.lower()

validation = block(
    MANAGER,
    "fn validate_render_queue_consistency(",
    "fn commit_render_firmware_install(",
)
for identity in (
    "header_qid != queue_id",
    "header_qid != producer.queue_id",
    "config != expected",
    "header.field_56 != u16::from(producer.qos.queue_byte_32)",
    "qos_record[0] != producer.qos.queue_byte_32",
    "u32::from(data_master) != expected_data_master",
    "header.context_id",
    "descriptor_context_ids",
):
    assert identity in validation, identity

completion = block(
    MANAGER,
    "pub(crate) fn render_completion_masks(",
    "pub(crate) const RENDER_COMPLETION_POLLS",
)
assert ".queue_pair()" in completion and ".completion_masks()" in completion

outer = block(
    RESOURCES,
    "pub(crate) fn publish_partial_opening_pair<F, G>(",
    "pub(crate) fn publish_deferred_partial_opening_pair(",
)
assert "queue_pair.fragment as u8" in outer
assert "queue_pair.tiling as u8" in outer

drm_submit = block(
    DRM,
    "fn publish(&self, packet: Arc<g17_job::G17PRenderJobPacket>)",
    "    fn timed_out(",
)
for use in (
    "let render_slot = completion.attach_lease(lease)?;",
    "render_slot,\n                    true,",
    "timeout_deferred_render(\n                        render_slot, submission_id, completion, error,",
):
    assert re.sub(r"\s+", "", use) in re.sub(r"\s+", "", drm_submit), use


# Execute the real allocator and record/mask encoders instead of a fixed-pair
# Python model that could hide production ownership bugs.
import subprocess
import sys
for test in ["test_g17_channel_table.py", "test_g17_render_queue_layout.py", "test_g17_render_slot_affinity.py"]:
    subprocess.run([sys.executable, str(Path(__file__).with_name(test))], check=True)
print("PASS production render identity routing and stable-owner stress")
