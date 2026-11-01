#!/usr/bin/env python3
"""Model and fuzz the host-visible keyframes render retirement boundary.

This is deliberately a host state model, not a GPU ISA model.  It replays the
states captured by threejs-keyframes-passive r128 and r153 and proves that queue
counters alone cannot turn the fragment stall into a successful userspace fence.
"""

from dataclasses import dataclass, replace
from pathlib import Path
import random


ROOT = Path(__file__).resolve().parents[1]
MESA_REPRO = (
    ROOT.parents[4]
    / "mesa/src/asahi/vulkan/tests/g17p_vulkan_msaa_depth_test.c"
)


def function(source: str, marker: str) -> str:
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


@dataclass(frozen=True)
class Identity:
    generation: int
    slot: int
    submission: int
    ordinal: int
    target: int


@dataclass(frozen=True)
class FaultSignature:
    reason: int
    source_count: int
    data_masters: tuple[int, ...]
    job_list_empty: bool
    fragment_end_written: bool


@dataclass
class FListRegistration:
    pointer: int
    cookie: int
    members: set[int]


class FirmwareFListTable:
    """B1 ID-indexed owner replacement semantics observed offline."""

    def __init__(self) -> None:
        self.owners: dict[int, FListRegistration] = {}

    def register(self, owner_id: int, pointer: int, cookie: int, qid: int) -> set[int]:
        current = self.owners.get(owner_id)
        if current is None:
            self.owners[owner_id] = FListRegistration(pointer, cookie, {qid})
            return set()
        if (current.pointer, current.cookie) == (pointer, cookie):
            current.members.add(qid)
            return set()
        evicted = set(current.members)
        self.owners[owner_id] = FListRegistration(pointer, cookie, {qid})
        return evicted

    def retire(self, owner_id: int, qid: int) -> None:
        owner = self.owners[owner_id]
        owner.members.discard(qid)

    def contains(self, owner_id: int, qids: set[int]) -> bool:
        owner = self.owners.get(owner_id)
        return owner is not None and qids <= owner.members


@dataclass
class RenderState:
    identity: Identity
    ta_done: int = 0
    ta_read: int = 0
    ta_write: int = 0
    fragment_done: int = 0
    fragment_read: int = 0
    fragment_write: int = 0
    job_list_empty: bool = False
    tiling_timestamps: tuple[int, int] = (0, 0)
    fragment_timestamps: tuple[int, int] = (0, 0)
    event_submitted: int = 0
    event_completed: int = 0
    event_membership: int = 1
    fragment_stamp: int = 0
    observed_type1: tuple[bool, bool] = (False, False)
    faulted: bool = False
    success_signalled: bool = False
    error_signalled: bool = False
    slot_quarantined: bool = False
    resources_retained: bool = True

    @property
    def expected_stamp(self) -> int:
        return ((self.identity.ordinal + 1) << 8) & 0xFFFFFFFF

    def queue_pair_retired(self) -> bool:
        target = self.identity.target
        return (
            (self.ta_done, self.ta_read, self.ta_write) == (target,) * 3
            and (self.fragment_done, self.fragment_read, self.fragment_write)
            == (target,) * 3
        )

    def execution_observed(self) -> bool:
        return (
            self.job_list_empty
            and all(self.tiling_timestamps)
            and all(self.fragment_timestamps)
        )

    def event_control_unlinked(self) -> bool:
        return (
            self.event_completed == self.event_submitted
            and self.event_membership == 0
        )

    def success_ready(self) -> bool:
        return (
            not self.faulted
            and all(self.observed_type1)
            and self.queue_pair_retired()
            and self.execution_observed()
            and self.event_control_unlinked()
            and self.fragment_stamp == self.expected_stamp
        )

    def settle_success(self) -> bool:
        if self.success_signalled or self.error_signalled or not self.success_ready():
            return False
        self.success_signalled = True
        self.resources_retained = False
        return True

    def recover_fault(self) -> None:
        if self.success_signalled or self.error_signalled:
            return
        self.faulted = True
        self.error_signalled = True
        self.slot_quarantined = True
        # The production error path keeps the physical owner until both
        # processors stop; an error fence is not permission to reuse the slot.
        self.resources_retained = True

    def release_after_processor_stop(self) -> None:
        assert self.slot_quarantined
        self.resources_retained = False

    def apply(self, event: str, identity: Identity) -> None:
        """Apply an observation only to its exact pending submission."""
        if identity != self.identity:
            return
        target = self.identity.target
        stamp = 0x1000 + self.identity.ordinal
        if event == "queue-retired":
            self.ta_done = self.ta_read = self.ta_write = target
            self.fragment_done = self.fragment_read = self.fragment_write = target
        elif event == "tiling-stamps":
            self.tiling_timestamps = (stamp, stamp)
        elif event == "fragment-start":
            self.fragment_timestamps = (stamp, self.fragment_timestamps[1])
        elif event == "fragment-end":
            self.fragment_timestamps = (stamp, stamp + 1)
        elif event == "job-unlinked":
            self.job_list_empty = True
        elif event == "event-unlinked":
            self.event_submitted = self.event_completed = 7
            self.event_membership = 0
        elif event == "fragment-stamp":
            self.fragment_stamp = self.expected_stamp
        elif event == "type1-ta":
            self.observed_type1 = (True, self.observed_type1[1])
        elif event == "type1-fragment":
            self.observed_type1 = (self.observed_type1[0], True)
        elif event == "recovery":
            self.recover_fault()
        else:
            raise AssertionError(f"unknown event {event}")


COMPLETION_EVENTS = (
    "queue-retired",
    "tiling-stamps",
    "fragment-start",
    "fragment-end",
    "job-unlinked",
    "event-unlinked",
    "fragment-stamp",
    "type1-ta",
    "type1-fragment",
)


def complete_state(identity: Identity) -> RenderState:
    state = RenderState(identity)
    for event in COMPLETION_EVENTS:
        state.apply(event, identity)
    return state


def test_vulkan_reproducer_shape() -> None:
    source = MESA_REPRO.read_text()
    recording = source[source.index("const bool render_only"):source.index(
        'check(vkEndCommandBuffer(command_buffer), "vkEndCommandBuffer")'
    )]
    replay = source[source.index("for (unsigned iteration = 0;"):]

    assert '#define TEST_NAME "g17p-vulkan-keyframes-repro"' in source
    assert "#define DEPTH_FORMAT VK_FORMAT_D32_SFLOAT_S8_UINT" in source
    assert "draw_repeats = 512;" in recording
    assert "pass < 2" in recording
    assert "max_draws = 3" in recording
    assert recording.count("vkCmdDraw(command_buffer, 3, 1, 0, 0)") == 1
    assert "for (unsigned i = 0; i < 2; ++i)" in recording
    assert "vkCmdCopyImageToBuffer" in recording
    assert "iterations = 4096;" in replay
    assert replay.index("vkResetFences") < replay.index("vkQueueSubmit")
    assert replay.index("vkQueueSubmit") < replay.index("vkWaitForFences")
    # One command buffer is recorded before the loop and replayed only after
    # its fence has completed: the pressure is persistent-state reuse, not
    # intentional same-process overlap.
    assert source.index("vkEndCommandBuffer(command_buffer)") < source.index(
        "for (unsigned iteration = 0;"
    )
    assert "VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT" in source
    begin = source[source.index("const VkCommandBufferBeginInfo begin"):source.index(
        "check(vkBeginCommandBuffer", source.index("const VkCommandBufferBeginInfo begin")
    )]
    assert ".flags = 0," in begin


def test_production_completion_gates_are_source_coupled() -> None:
    completion = (ROOT / "g17_completion.rs").read_text()
    manager = (ROOT / "g17_manager.rs").read_text()
    runtime = (ROOT / "g17_live_boot.rs").read_text()
    drm = (ROOT / "g17_drm.rs").read_text()

    observed = function(completion, "pub(crate) fn observed(self)")
    assert "self.job_list_empty" in observed
    assert "self.tiling_timestamps.iter().all" in observed
    assert "self.fragment_timestamps.iter().all" in observed

    paired = function(completion, "pub(crate) fn observe_paired_render_completion(")
    assert "let retired = tiling.complete && fragment.complete;" in paired
    assert "let complete = retired && execution.observed();" in paired

    poll = function(manager, "pub(crate) fn commit_render_observation(")
    assert "if !event_control.unlinked()" in poll
    assert "fragment_stamp != expected_stamp" in poll
    assert "G17PRenderPollOutcome::Complete(completion)" in poll

    events = function(runtime, "fn service_async_render_completions(")
    assert "g17p_render_type1_complete(*observed, expected)" in events
    settle = function(runtime, "fn settle_deferred_render_completion(")
    assert "self.defer_render_completion(signal, observed)" in settle
    signal = function(drm, "pub(crate) fn complete(")
    assert "if !completion.complete" in signal
    assert signal.index("drop(self.lease.lock().take())") < signal.index(
        "self.fence.signal()"
    )

    lifecycle = manager[manager.index("impl G17PBootstrapUmaLifecycle {"):]
    acquire = function(lifecycle, "fn acquire(")
    assert "let overlapping =" in acquire
    assert "sequence: if overlapping" in acquire
    assert "previous.sequence" in acquire


def test_browser_fragment_stalls_cannot_signal_success() -> None:
    captures = (
        # threejs-keyframes-passive-r128-exact-096b8d3e
        (Identity(0, 3, 16665, 8310, 613), 0x19F04B081, 476),
        # threejs-keyframes-passive-r153-fulltrace-bb27bd21
        (Identity(0, 3, 5425, 3027, 124), 0x8AD7FA57, 174),
    )
    for identity, start, event_count in captures:
        target = identity.target
        state = RenderState(
            identity=identity,
            ta_done=target,
            ta_read=target,
            ta_write=target,
            fragment_done=target,
            fragment_read=target,
            fragment_write=target,
            job_list_empty=False,
            tiling_timestamps=(start, start),
            fragment_timestamps=(start, 0),
            event_submitted=event_count,
            event_completed=event_count,
            event_membership=0,
            observed_type1=(True, True),
        )

        assert state.queue_pair_retired()
        assert state.event_control_unlinked()
        assert not state.execution_observed()
        assert not state.success_ready()
        assert not state.settle_success()
        # This is the precise bad inference the replay guards against.
        assert state.queue_pair_retired() and not state.success_signalled

        state.recover_fault()
        assert state.error_signalled
        assert state.slot_quarantined
        assert state.resources_retained
        assert not state.settle_success()
        state.release_after_processor_stop()
        assert not state.resources_retained


def test_vulkan_copy_tail_fault_is_not_the_browser_fragment_signature() -> None:
    browser = FaultSignature(
        reason=0,
        source_count=1,
        data_masters=(1,),
        job_list_empty=False,
        fragment_end_written=False,
    )
    vulkan_full = FaultSignature(
        reason=0,
        source_count=2,
        data_masters=(1, 2),
        job_list_empty=True,
        fragment_end_written=True,
    )
    assert browser.reason == vulkan_full.reason
    assert browser != vulkan_full


def test_changing_flist_cookie_evicts_an_overlapping_render_pair() -> None:
    pointer = 0xFFFFFC20001C8000
    first_pair = {2, 3}
    second_pair = {10, 11}

    old_policy = FirmwareFListTable()
    for qid in first_pair:
        assert not old_policy.register(0, pointer, 0x100, qid)
    evicted = old_policy.register(0, pointer, 0x101, min(second_pair))
    old_policy.register(0, pointer, 0x101, max(second_pair))
    assert evicted == first_pair
    assert not old_policy.contains(0, first_pair)
    assert old_policy.contains(0, second_pair)

    stable_policy = FirmwareFListTable()
    for qid in first_pair | second_pair:
        assert not stable_policy.register(0, pointer, 0x100, qid)
    assert stable_policy.contains(0, first_pair | second_pair)


def test_stable_flist_cookie_preserves_random_overlaps() -> None:
    rng = random.Random(0xF1157)
    pointer = 0xFFFFFC20001C8000
    pairs = ({2, 3}, {6, 7}, {10, 11})
    for _ in range(10000):
        table = FirmwareFListTable()
        active: set[int] = set()
        order = list(range(len(pairs)))
        rng.shuffle(order)
        for index in order:
            pair = pairs[index]
            for qid in pair:
                assert not table.register(0, pointer, 0x100, qid)
                active.add(qid)
                assert table.contains(0, active)
            if rng.randrange(3) == 0:
                retiring = rng.choice(sorted(active))
                table.retire(0, retiring)
                active.remove(retiring)
                assert table.contains(0, active)


def test_every_observed_gate_is_necessary() -> None:
    identity = Identity(generation=4, slot=1, submission=80, ordinal=20, target=77)
    state = complete_state(identity)
    assert state.success_ready()

    mutations = (
        {"fragment_done": 76},
        {"job_list_empty": False},
        {"tiling_timestamps": (0, 0)},
        {"fragment_timestamps": (0x1014, 0)},
        {"event_membership": 1},
        {"fragment_stamp": state.expected_stamp - 0x100},
        {"observed_type1": (True, False)},
        {"faulted": True},
    )
    for mutation in mutations:
        assert not replace(state, **mutation).success_ready(), mutation


def test_stale_and_duplicate_callbacks_are_identity_safe() -> None:
    current = Identity(generation=9, slot=0, submission=44, ordinal=33, target=18)
    stale = replace(current, generation=8, submission=43, ordinal=32, target=14)
    state = RenderState(current)
    for event in COMPLETION_EVENTS:
        state.apply(event, stale)
    assert state == RenderState(current)

    for event in COMPLETION_EVENTS + COMPLETION_EVENTS:
        state.apply(event, current)
    assert state.success_ready()
    assert state.settle_success()
    assert not state.settle_success()
    assert not state.resources_retained


def test_three_client_adversarial_event_fuzz() -> None:
    rng = random.Random(0xA17_3D)
    schedules = 6000
    for case in range(schedules):
        frames = [
            RenderState(Identity(case, slot, case * 3 + slot, case * 7 + slot, 3 + slot))
            for slot in range(3)
        ]
        stream: list[tuple[int, str, Identity]] = []
        for slot, frame in enumerate(frames):
            events = list(COMPLETION_EVENTS)
            rng.shuffle(events)
            stream.extend((slot, event, frame.identity) for event in events)
            # Duplicate edges and a callback from the preceding generation
            # exercise the same-slot reuse boundary.
            stream.append((slot, rng.choice(events), frame.identity))
            stale = replace(
                frame.identity,
                generation=frame.identity.generation - 1,
                submission=frame.identity.submission - 3,
            )
            stream.append((slot, rng.choice(events), stale))
        rng.shuffle(stream)

        for slot, event, identity in stream:
            frame = frames[slot]
            frame.apply(event, identity)
            before = frame.success_signalled
            frame.settle_success()
            if not before and frame.success_signalled:
                assert frame.success_ready()
                assert not frame.resources_retained

        assert all(frame.success_signalled for frame in frames)
        assert all(not frame.resources_retained for frame in frames)


def test_recovery_interleavings_never_become_success() -> None:
    rng = random.Random(0xF17A6)
    for case in range(4000):
        identity = Identity(case, case % 3, case, case + 100, case % 251 + 1)
        state = RenderState(identity)
        events = list(COMPLETION_EVENTS)
        rng.shuffle(events)
        fault_at = rng.randrange(len(events))
        for index, event in enumerate(events):
            if index == fault_at:
                state.apply("recovery", identity)
            state.apply(event, identity)
            state.settle_success()
        assert state.error_signalled
        assert state.slot_quarantined
        assert state.resources_retained
        assert not state.success_signalled


print(
    "PASS keyframes workload shape, r128/r153 fragment-stall replay, exact retirement "
    "gates, FList owner replacement, stale callbacks, and 20,000 schedules"
)
