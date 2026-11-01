#!/usr/bin/env python3
"""Source-integration checks for retained compute QoS identity publication."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
COMPUTE = (ROOT / "g17_compute.rs").read_text()
MANAGER = (ROOT / "g17_manager.rs").read_text()
RESOURCES = (ROOT / "g17_resources.rs").read_text()


def block(source: str, start: str, end: str) -> str:
    begin = source.index(start)
    return source[begin:source.index(end, begin)]


allocator = block(
    MANAGER,
    "fn allocate_compute(&mut self)",
    "/// Return a submission's hold",
)
for term in (
    "for id in 1..G17P_TA_HARDWARE_BUFFER_ID_COUNT as u32",
    "self.is_free(id)",
    "!self.is_reserved(id)",
    "self.remove_from_stack(id)",
):
    assert term in allocator, term

prepare = block(
    MANAGER,
    "pub(crate) fn prepare_g17p_compute_queue(",
    "/// Prepare a queue for its firmware-mediated tag-15 installation.",
)
assert "allocate_compute_queue_id()" in prepare
assert "allocate_compute()" not in prepare
assert "hardware_buffer_id," in prepare
logical = block(MANAGER, "pub(crate) fn prepare_logical_queue_context(", "pub(crate) fn prepare_g17p_compute_queue(")
assert "G17PLogicalQosLease::new" in logical
assert "self.channel_ids.cancel_unpublished(channel_lease)" in prepare

encoder = block(
    COMPUTE,
    "pub(crate) fn encode_compute_config_update(",
    "pub(crate) fn encode_compute_queue_context_item(",
)
assert "hardware_buffer_id: u8" in encoder
assert "(0x56, u16::from(hardware_buffer_id))" in encoder

qos = block(
    RESOURCES,
    "fn publish_qos_queue_record(",
    "/// Dump the QOS resource regions",
)
for term in (
    "queue_id: u8",
    "hardware_buffer_id: u8",
    "scheduler_state: u64",
    "data_master: u8",
    "0x800usize",
    "0xc00usize",
    "0x400usize",
    "0x600usize",
    "g17p_live_read_u64(bytes, 0xe00)",
    "0xe08usize",
    "0xe20usize",
    "record[0] = hardware_buffer_id as u8",
    "record[1] = qos_class",
    "Ordering::SeqCst",
    "pub(crate) fn publish_compute_qos_queue_record(",
    "scheduler_state,\n            2,",
):
    assert term in qos, term

first = block(
    MANAGER,
    "fn submit_translated_first_bind_channel(",
    "fn submit_translated_repeat_channel(",
)
first_stage = first.index("stage_initial_channel_group_without_fallback_event(")
first_qos = first.index("publish_compute_qos_queue_record(")
first_tag15 = first.index("publish_initial_config_update_pointer(group)")
assert first_stage < first_qos < first_tag15
assert first.count("queue.qos_hardware_buffer_id()") >= 2
assert "cancel_qos_publication(qos_publication)" in first

repeat = block(
    MANAGER,
    "fn submit_translated_repeat_channel(",
    "pub(crate) fn submit_translated_compute(",
)
repeat_stage = repeat.index("stage_repeat_config_update(")
repeat_command = repeat.index('"the tag-3 command pointer"')
repeat_qos = repeat.index("publish_compute_qos_queue_record(")
repeat_tag15 = repeat.index('"the tag-15 ConfigUpdate pointer"')
assert repeat_stage < repeat_command < repeat_qos < repeat_tag15
assert repeat.count("queue.qos_hardware_buffer_id()") >= 2
assert "cancel_qos_publication(qos_publication)" in repeat
complete = block(MANAGER, "pub(crate) fn poll_staged_independent_compute_completion(", "fn allocate_compute_queue_id(")
assert complete.index("read_compute_stamp()?") < complete.index("complete_qos_queue_records(")
assert complete.index("complete_qos_queue_records(") < complete.index("queue.completion_descriptor = 0")
paired = MANAGER[MANAGER.index("let retired_sksm ="):]
assert paired.index("self.retire_tracked_render_qos(pair_slot)?") < paired.index(
    "self.render_slots.get_mut(pair_slot).tracker = self.render_slots.get_mut(pair_slot).queued_tracker.take()"
)
tracked = block(MANAGER, "fn retire_tracked_render_qos(", "pub(crate) fn retire_abandoned_render_if_firmware_retired(")
assert tracked.index("if !self.render_slots.get(pair_slot).tracker.as_ref().ok_or(ENODEV)?.qos_pending") < tracked.index("complete_qos_queue_records(")
assert tracked.index("complete_qos_queue_records(") < tracked.index("qos_pending = false")
retire = block(RESOURCES, "pub(crate) fn complete_qos_queue_records(", "/// Dump the QOS resource regions")
assert retire.index("row[0] != hbid") < retire.index("bytes[offset] = 0xff")
assert "owners[..index]" in retire

reset = block(
    MANAGER,
    "pub(crate) fn release_runtime_objects_after_processor_stop(",
    "/// Restore cold initdata",
)
assert "self.render_hardware_buffer_ids = G17PHardwareBufferIdAllocator::new();" in reset
assert reset.index("G17PHardwareBufferIdAllocator::new()") < reset.index(
    "self.channel_ids = g17p_initial_channel_table()"
)

print("g17 compute HardwareBufferID/QoS publication checks: PASS")
