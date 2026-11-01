#!/usr/bin/env python3
"""Recovery must replay records whose shared notification was already consumed."""

from pathlib import Path
import subprocess
import tempfile


root = Path(__file__).resolve().parents[1]
live = (root / "g17_live_boot.rs").read_text()


def function(text: str, marker: str) -> str:
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos]


drain = function(live, "fn drain_primary_firmware_event_batch(")
# The service body is shared by the top-level and nested entries (stack depth).
recovery = function(live, "    fn service_primary_recovery_body(")
assert "self.service_primary_recovery_body()" in function(live, "    pub(crate) fn service_primary_recovery(")
assert "self.service_primary_recovery_body()" in function(live, "    fn service_primary_recovery_nested(")
assert ".firmware_event_notifications\n        .swap(0, Ordering::AcqRel)" in drain
type4 = drain.index("g17_completion::G17P_FIRMWARE_EVENT_RECOVERY_TYPE")
assert drain.index("break;", type4) > drain.index("batch.recovery_generation = Some", type4)

replay = "crate::driver::queue_g17p_firmware_event_worker(state.dev.clone());"
assert recovery.count(replay) == 1
assert recovery.index("complete_pending_primary_recovery") < recovery.index(replay)
# The failure handler must come before the replay requeue and must leave the
# function, so a failed recovery never re-arms the drain. Spelled as a match on
# the (step, error) pair since the crashed transition began naming its step.
fail = recovery.index("note_state_crashed(")
assert fail < recovery.index(replay)
assert recovery.index("return Err(error);", fail) < recovery.index(replay)

# Model the exact work handoff: one notification can cover a type-4 record and
# later records in the same producer snapshot. The type-4 consumer is advanced,
# the pending count is atomically cleared, and the ring is intentionally left
# nonempty until recovery finishes. A bool models queue_work's coalescing.
rust = r'''
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event { Recovery, Completion(u8), Uma }

struct Runtime {
    ring: [Option<Event>; 256],
    consumer: u8,
    producer: u8,
    notifications: u32,
    recovery_pending: bool,
    event_work_queued: bool,
    masks: u128,
    uma: u32,
    drain_passes: u32,
}

impl Runtime {
    fn new(consumer: u8) -> Self {
        Self {
            ring: [None; 256], consumer, producer: consumer,
            notifications: 0, recovery_pending: false,
            event_work_queued: false, masks: 0, uma: 0, drain_passes: 0,
        }
    }

    fn publish(&mut self, event: Event, notify: bool) {
        self.ring[self.producer as usize] = Some(event);
        self.producer = self.producer.wrapping_add(1);
        if notify {
            self.notifications = self.notifications.wrapping_add(1);
            self.event_work_queued = true;
        }
    }

    fn drain_event_batch(&mut self) {
        self.drain_passes += 1;
        self.notifications = 0; // AtomicU32::swap(0, AcqRel)
        while self.consumer != self.producer {
            let index = self.consumer;
            self.consumer = self.consumer.wrapping_add(1);
            match self.ring[index as usize].take().unwrap() {
                Event::Recovery => {
                    self.recovery_pending = true;
                    break; // the production type-4 restartWorkQueue boundary
                }
                Event::Completion(qid) => self.masks |= 1u128 << qid,
                Event::Uma => self.uma += 1,
            }
        }
    }

    fn run_event_work(&mut self) {
        assert!(self.event_work_queued);
        self.event_work_queued = false;
        self.drain_event_batch();
    }

    fn complete_recovery(&mut self) {
        assert!(self.recovery_pending);
        self.recovery_pending = false;
        // Production queues one continuation unconditionally. If an IRQ
        // already queued it, queue_work coalesces on the same bool.
        self.event_work_queued = true;
    }
}

#[test]
fn two_client_completions_behind_type4_survive_the_cleared_edge() {
    let mut r = Runtime::new(10);
    r.publish(Event::Recovery, false);
    r.publish(Event::Completion(4), false);
    r.publish(Event::Completion(65), false);
    r.notifications = 1; // one coalesced RTKit edge for the producer snapshot
    r.event_work_queued = true;

    r.run_event_work();
    assert!(r.recovery_pending);
    assert_eq!(r.notifications, 0);
    assert_ne!(r.consumer, r.producer);
    assert!(!r.event_work_queued); // the old conditional predicate loses here

    r.complete_recovery();
    r.run_event_work();
    assert_eq!(r.masks, (1u128 << 4) | (1u128 << 65));
    assert_eq!(r.consumer, r.producer);
    assert_eq!(r.drain_passes, 2);
}

#[test]
fn recovery_boundary_replay_preserves_ring_wrap_and_type13() {
    let mut r = Runtime::new(255);
    r.publish(Event::Recovery, false);      // slot 255
    r.publish(Event::Completion(127), false); // slot 0
    r.publish(Event::Uma, false);           // slot 1
    r.notifications = 3;
    r.event_work_queued = true;

    r.run_event_work();
    assert_eq!(r.consumer, 0);
    assert_eq!(r.notifications, 0);
    r.complete_recovery();
    r.run_event_work();
    assert_eq!(r.consumer, 2);
    assert_eq!(r.masks, 1u128 << 127);
    assert_eq!(r.uma, 1);
}

#[test]
fn concurrent_irq_and_recovery_continuation_coalesce_without_loss() {
    let mut r = Runtime::new(40);
    r.publish(Event::Recovery, true);
    r.publish(Event::Completion(4), false); // already behind type 4
    r.run_event_work();

    // A second client publishes while recovery owns the command gates. Its
    // callback queues work, and the successful recovery queues the same work.
    r.publish(Event::Completion(65), true);
    r.complete_recovery();
    assert!(r.event_work_queued);
    r.run_event_work();
    assert_eq!(r.masks, (1u128 << 4) | (1u128 << 65));
    assert_eq!(r.drain_passes, 2);
    assert!(!r.event_work_queued);
}
'''

with tempfile.TemporaryDirectory() as directory:
    directory = Path(directory)
    source = directory / "replay.rs"
    source.write_text(rust)
    executable = directory / "replay"
    subprocess.run(
        ["rustc", "--edition=2021", "--test", str(source), "-o", str(executable)],
        check=True,
    )
    subprocess.run([str(executable), "--test-threads=1"], check=True, timeout=60)

print(
    "PASS recovery event replay: cleared coalesced edge, wrap/type13, "
    "two-client QIDs, and concurrent IRQ coalescing"
)
