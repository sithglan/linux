#!/usr/bin/env python3
"""Two native physical render pairs, each with alternating descriptor generations."""

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MANAGER = (ROOT / "g17_manager.rs").read_text()
RESOURCES = (ROOT / "g17_resources.rs").read_text()
LIVE = (ROOT / "g17_live_boot.rs").read_text()
DRM = (ROOT / "g17_drm.rs").read_text()
LIMITS = (ROOT / "g17_queue_limits.rs").read_text()

assert "MAX_IN_FLIGHT_PER_QUEUE: u32 = 2" in LIMITS
# Compute publishes without a host-retirement edge: several kicks per queue,
# ordered by each CL entry's implicit previous-kick parent in firmware.
assert "MAX_COMPUTE_IN_FLIGHT_PER_QUEUE: u32 = 16" in LIMITS
assert "COMPUTE_RECORD_SLOTS: usize = 2 * MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize" in LIMITS
_async_prepare = DRM[DRM.index("impl g17_job::G17PQueueBackend for G17PAsyncQueueBackend {"):]
_async_prepare = _async_prepare[:_async_prepare.index("fn publish(")]
assert "None" in _async_prepare and "async_compute_dependency" not in DRM
assert "G17P_RENDER_MAX_IN_FLIGHT: usize = 2" in RESOURCES
assert "owned >= crate::g17_queue_limits::MAX_IN_FLIGHT_PER_QUEUE as usize" in DRM
assert "render_queued_tracker: Option<G17PPartialOpeningRenderTracker>" in MANAGER
assert "self.render_tracker = self.render_queued_tracker.take();" in MANAGER
assert "pending_render_queued" in LIVE
assert "self.pending_render[index] = Some(next);" in LIVE
assert "if !self.render_fence_state.pending(slot)" in LIVE
assert "render_refs: [u8; G17P_RENDER_SLOTS]" in DRM
assert "g17p_render_descriptor_slot(submission_ordinal)\n            .checked_mul(16)" in RESOURCES
assert "render_command_ordinals[generation] != Some(submission_ordinal)" in RESOURCES
assert "g17p_render_completion_window(active.ordinal)?.target()" in MANAGER
assert '"queued-render-active-target-mismatch"' in MANAGER
assert "done: active_target" in MANAGER
assert "read: active_target" in MANAGER
assert "write: active_target" in MANAGER
assert "pub(crate) fn active_render_slot(&self) -> u8" in MANAGER
assert "fn wait_render_completion_servicing_events(\n        &mut self,\n        render_slot: u8," in LIVE
assert "let caller_slot = self\n            .manager\n            .as_ref()\n            .ok_or(ENODEV)?\n            .active_render_slot();" in LIVE
assert "manager.select_render_slot(caller_slot)" in LIVE
assert "self.wait_render_completion_servicing_events(render_slot)" in LIVE


class Queue:
    def __init__(self):
        self.generations = [[None, None] for _ in range(2)]
        self.trackers = []
        self.hwpb_refs = 0

    def publish(self, ordinal):
        assert len(self.trackers) < 2
        slot = ordinal % 2
        generation = (ordinal // 2) % 2
        assert self.generations[slot][generation] is None
        assert all(tracker["slot"] != slot for tracker in self.trackers)
        self.generations[slot][generation] = ordinal
        self.trackers.append({
            "ordinal": ordinal,
            "slot": slot,
            "generation": generation,
            "timestamp_slot": ordinal % 32,
            "target": ((ordinal * 3) + 3) % 1280,
        })
        self.hwpb_refs += 1

    def retire(self, ordinal):
        tracker = self.trackers.pop(0)
        assert tracker["ordinal"] == ordinal
        slot = tracker["slot"]
        generation = tracker["generation"]
        assert self.generations[slot][generation] == ordinal
        self.generations[slot][generation] = None
        self.hwpb_refs -= 1


queue = Queue()
for ordinal in range(1, 12_289):
    queue.publish(ordinal)
    if len(queue.trackers) == 2:
        queue.retire(ordinal - 1)
while queue.trackers:
    queue.retire(queue.trackers[0]["ordinal"])

assert queue.generations == [[None, None] for _ in range(2)]
assert queue.hwpb_refs == 0


class SlotVisitor:
    """Cross-slot event scans must be observational to their caller."""

    def __init__(self, active):
        self.active = active

    def service(self, pending):
        caller = self.active
        try:
            for slot in pending:
                self.active = slot
        finally:
            self.active = caller


visitor = SlotVisitor(active=5)
visitor.service([1, 3, 7])
assert visitor.active == 5


def distance(base, value, capacity=0x500):
    return (value + capacity - base) % capacity


# Hardware trace regression: group 2 was published while group 1's fragment
# cursors were mixed 3/6/9.  Group 2's window starts at group 1's target (6),
# so copying that physical snapshot poisons its immutable previous state.  The
# exact predecessor target is accepted and permits the observed 9/9/9 retire.
base = 6
count = 3
physical_mid_predecessor = (3, 6, 9)
exact_predecessor_target = (base, base, base)
completed = (9, 9, 9)
assert distance(base, physical_mid_predecessor[0]) > count
assert all(distance(base, value) == 0 for value in exact_predecessor_target)
assert all(distance(base, value) == count for value in completed)

# The same identity rule is modular: the predecessor of a group beginning at
# zero is the wrapped target zero, not a numerically larger stale cursor.
wrap_base = 0
assert all(distance(wrap_base, value) == 0 for value in (0, 0, 0))
print("PASS native-two-pair/two-generation exact render ownership across 12,288 publications")
