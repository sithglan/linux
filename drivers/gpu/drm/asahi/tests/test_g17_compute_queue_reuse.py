#!/usr/bin/env python3
"""Check production policy: installed channels stay retained until stop."""

from pathlib import Path


root = Path(__file__).resolve().parents[1]
runtime = (root / "g17_async_compute.rs").read_text()
limits = (root / "g17_queue_limits.rs").read_text()
manager = (root / "g17_manager.rs").read_text()
live = (root / "g17_live_boot.rs").read_text()
config = (root / "g17_config.rs").read_text()


def block(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


prepare = block(runtime, "    pub(crate) fn prepare_async_compute_queue(")
matches = block(runtime, "    pub(crate) fn async_compute_queue_matches(")
release = block(runtime, "    pub(crate) fn release_async_compute_queue(")
service = block(runtime, "    fn service_async_compute_completions_matching(")
# (serialization audit) a queue's first install is an ordinary reader lease; the
# async_compute_needs_install predicate and the install drain no longer exist.
predicate = block(limits, "pub(crate) const fn reusable_idle_compute_graph(")
rebindable = block(limits, "pub(crate) const fn rebindable_released_compute_graph(")
stop = block(manager, "    pub(crate) fn release_runtime_objects_after_processor_stop(")
runtime_stop = block(live, "    fn release_runtime_objects(&mut self)")

for field in ["q.owner.is_none()", "q.inflight.is_none()", "q.binding.is_none()", "q.context.is_none()"]:
    assert field in prepare
assert "q.quarantined" in prepare
selection = prepare[: prepare.index("let index =")]
assert "q.queue.requires_first_install()" in selection
assert "!q.queue.requires_first_install()" in selection
assert "crate::g17_config::ASYNC_COMPUTE_REBIND == 0" in selection
assert "pub(crate) const ASYNC_COMPUTE_REBIND: u32 = 1;" in config
assert "rebindable_released_compute_graph(" in selection
for field in ["q.owner.is_none()", "q.inflight.is_none()", "q.released", "q.quarantined"]:
    assert field in selection
assert "retaining previous binding until completion" in prepare
assert "q.previous_binding.is_none()" in selection
assert "return Ok(false)" in matches
for identity in [
    "q.queue.queue_id() != hardware_queue",
    "!Arc::ptr_eq(installed_context, context)",
    "!context.is_current()",
    "q.binding.is_none()",
]:
    assert identity in matches
assert "q.released" in matches and "q.quarantined" in matches
assert "owner_free && active_free && binding_free && context_free && !quarantined" in predicate
assert "owner_free && active_free && released && !quarantined" in rebindable
assert "q.inflight.is_none() && !q.quarantined" in release
# The claim is given up at close; the binding and context are NOT, because the
# firmware still has the QID installed and sweeps its record pointer. They are
# retained until processor stop -- see
# test_g17_async_release_retains_firmware_state.
assert release.index("q.released = true") < release.index("q.owner = None")
assert "q.binding = None" not in release
assert service.index("q.inflight.take()") < service.index("if q.released && q.inflight.is_none()")
assert "q.binding = None" not in service
assert "q.binding.take()" in prepare and "q.context.take()" in prepare
assert "self.channel_ids = g17p_initial_channel_table()" in stop
assert "self.next_channel_identity =" not in stop
for stale_session_field in [
    "self.pending_add_kicks = None",
    "self.last_published_descriptor = 0",
    "self.last_completion_end = 0",
    "self.compute_stamp_armed = false",
]:
    assert stale_session_field in stop
assert runtime_stop.index("self.release_async_compute_after_processor_stop()") < runtime_stop.index(
    "manager.release_runtime_objects_after_processor_stop()"
)


# Runtime fallible ownership and publication behavior is exercised by
# test_g17_async_compute_rebind.py; global exhaustion by test_g17_channel_table.py.
# Do not substitute a Python pool model for those production methods.
print("PASS installed-channel retention and stopped-session source contracts")
