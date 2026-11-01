#!/usr/bin/env python3
"""Exercise the actual retained-FList release lifecycle: fire-and-forget 0x2e, carried witnesses, backpressure."""
from pathlib import Path
import re
import subprocess
import tempfile

src = Path(__file__).resolve().parent.parent
manager = (src / "g17_manager.rs").read_text()
submission = (src / "g17_submission.rs").read_text()
initdata = (src / "g17_initdata.rs").read_text()
runtime = (src / "g17_live_boot.rs").read_text()
drm = (src / "g17_drm.rs").read_text()


def block(source, pattern):
    match = re.search(pattern, source, re.M)
    assert match, pattern
    at = source.index("{", match.end())
    depth, end = 1, at + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[match.start():end]


def constant(source, name):
    match = re.search(r"(?:pub\(crate\) )?const " + name + r": .*?;", source, re.S)
    assert match, name
    return match.group()


derive = "#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n"
submission_part = "\n".join([
    *[constant(submission, name) for name in [
        "G17P_USC_FREELIST_LOW_END", "G17P_USC_FREELIST_GROW_DESCRIPTOR_BYTES",
    ]],
    block(submission, r"^pub\(crate\) const fn g17p_render_flist_generation\("),
])
initdata_part = "\n".join(constant(initdata, name) for name in [
    "CONTROL_SHARED_ADDRESS", "CONTROL_OPERAND_TABLE_ADDRESS",
    "PARTIAL_OPENING_FREELIST_HARDWARE_BUFFER_ID",
    "CONTROL_RING_ENTRIES",
    "CONTROL_RING_INDEX_MASK",
])
lease_part = "\n".join([
    *[constant(manager, name) for name in [
        "G17P_BOOTSTRAP_UMA_SEQUENCE", "G17P_BOOTSTRAP_UMA_HARDWARE_BUFFER_ID",
        "G17P_BOOTSTRAP_UMA_STAMP", "G17P_BOOTSTRAP_UMA_COOKIE",
        "G17P_BOOTSTRAP_UMA_COUNT",
    ]],
    *[derive + block(manager, r"^(?:pub\(crate\) )?(?:struct|enum) " + name + r"\s*(?=\{)")
      for name in ["G17PBootstrapUmaCompletion", "G17PBootstrapUmaRelease",
                   "G17PBootstrapUmaRequestState", "G17PBootstrapUmaLifecycleError",
                   "G17PBootstrapUmaLifecycle"]],
    block(manager, r"^fn g17p_control_ring_target_consumed\("),
    block(manager, r"^impl G17PBootstrapUmaLifecycle\s*"),
    *["#[test]\n" + block(manager, r"^    fn " + name + r"\(\)\s*") for name in [
        "bootstrap_async_flist_releases_only_after_type13_and_paired_render",
        "bootstrap_async_flist_release_backpressure_tracks_exact_producer",
        "bootstrap_release_stays_ready_until_backpressure_clears",
        "shared_flist_holds_both_render_pairs_until_the_last_retirement",
        "retained_flist_leases_grow_once_and_release_every_captured_generation",
        "reacquire_before_release_consumption_carries_the_witness",
        "presized_render_flist_first_lease_is_warm",
        "private_render_flist_owner_grows_once_then_reacquires_warm",
    ]],
])
body = ("#![allow(dead_code)]\nmod g17_submission {" + submission_part +
        "}\nmod g17_initdata {" + initdata_part + "}\n" + lease_part)
with tempfile.TemporaryDirectory(prefix="g17-flist-backpressure-") as temp:
    binary = Path(temp) / "tests"
    subprocess.run(["rustc", "--edition=2021", "--test", "-", "-o", str(binary)],
                   input=body, text=True, check=True)
    subprocess.run([str(binary)], check=True)

settle = block(runtime, r"^    pub\(crate\) fn settle_render_usc_freelist_release_before_submission\(")
assert "settle_consumed_render_usc_freelist_releases()?" in settle
assert "fsleep" not in settle
# No per-slot "release still pending" predicate exists any more: the next
# kick never waits for firmware to consume a 0x2e release.
assert "render_usc_freelist_release_pending_for_slot" not in runtime
assert "render_slot_flist_release_sent" not in manager
submit = block(runtime, r"^    pub\(crate\) fn submit_translated_render\(")
assert "settle_render_usc_freelist_release_before_submission()?" not in submit
gate = block(manager, r"^    fn begin_translated_render_with_hardware_buffer\(")
# Pending growth is an independent lease, not a render admission barrier.
assert "matches!(owner.state, G17PBootstrapUmaRequestState::Submitted)" not in gate
assert "G17PBootstrapUmaRequestState::Sent" not in gate
grow = block(runtime, r"^    fn publish_render_usc_freelist_request\(")
assert grow.index("Ok(None) => return Ok(())") < grow.index("Err(error) if error == EAGAIN")
assert "flush_pending_primary_device_control()?" in grow and "flush_pending_render_usc_freelist_release()?" in grow
completion = block(runtime, r"^    fn settle_deferred_render_completion\(")
assert "Err(error) if error == EAGAIN" in completion
assert completion.index("Err(error) if error == EAGAIN") < completion.index("rebuild_session_or_defer")
worker = block(runtime, r"^    pub\(crate\) fn service_primary_firmware_events\(")
assert worker.index("flush_pending_primary_device_control()?") < worker.index("flush_pending_render_usc_freelist_release()?")
assert worker.index("flush_pending_render_usc_freelist_release()?") < worker.index("notifications_pending")
inline = block(runtime, r"^    fn service_firmware_events_inline\(")
assert "pending_release && !self.flush_pending_render_usc_freelist_release()?" in inline
flush = block(runtime, r"^    fn flush_pending_primary_device_control\(")
assert flush.index("send_message") < flush.index("note_state_crashed")
reset = block(manager, r"^    pub\(crate\) fn reset_session_after_processor_stop\(")
assert "self.pending_primary_control = None;" in reset
assert "self.pending_flist_release_worker_attempts = 0;" in reset
request = block(manager, r"^    pub\(crate\) fn publish_render_usc_freelist_request\(")
assert request.index("if lifecycle.request.is_none()") < request.index("primary_device_control_backpressured()")
release_service = block(runtime, r"^    fn service_render_usc_freelist_release\(")
assert release_service.index("Err(error) if error == EAGAIN") < release_service.index("queue_primary_control_continuation")
assert "settle_consumed_render_usc_freelist_releases()?" in release_service
assert "fsleep" not in release_service
assert "self.publish_with(" in block(drm, r"^    fn publish\(&self, packet: Arc<g17_job::G17PRenderJobPacket>\)")
publish = block(drm, r"^    fn publish_with\(")
# The render publish path waits on nothing firmware-ordered: no release poll,
# no control poll. Its one sleep is context-local state-capacity back-pressure
# on the adaptive completion-poll schedule; other contexts cannot consume it.
assert "primary_device_control_backpressured" not in publish
assert "render_usc_freelist_release_pending_for_slot" not in publish
# v92c adds one bounded owner-change wait: the FList takeover guard sleeps only
# while the pair still carries another owner's unconsumed 0x2e release (1 s cap).
assert publish.count("fsleep(") == 2
guard = publish.index("render_flist_takeover_ready(")
assert publish.index("fsleep(Delta::from_micros(200))", guard) < publish.index("takeover_polls += 1", guard) + 200
assert "takeover_polls >= 5000" in publish
exhausted = publish.index("match completion.work_state().and_then(|state| state.acquire())")
# Capacity waits and node initialization finish before taking device exclusion.
assert exhausted < publish.index("fsleep(", exhausted) < publish.index("self.runtime.mutex().lock()", exhausted)
assert "Err(error) if error == EBUSY" in publish
assert "Err(error) => return Err(error)" in publish
assert "crate::g17_perf::PollSchedule::new(0)" in publish
assert publish.index("settle_render_usc_freelist_release_before_submission()?") < publish.index("submit_translated_render(")
print("retained FList release: fire-and-forget 0x2e, carried witnesses, wrap-safe settlement, capacity-only publish wait PASS")
