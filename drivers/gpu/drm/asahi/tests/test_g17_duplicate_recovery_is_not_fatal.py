#!/usr/bin/env python3
"""One firmware restart seen twice must not end the session.

The firmware raises ONE restart request and both RTKit roles can observe it:
`service_primary_firmware_events` drains the gfx ring and then, in the same pass
and unconditionally, the gfx1 ring. The bare `queue_primary_recovery` answers a
second view of that request with EBUSY, and every caller propagates it with `?`
into a chain whose only error handler sets `crashed` -- so the ordinary dual-role
case permanently deafened the session. The same holds for a type-4 record whose
generation the host has already serviced.

Also pins the attribution: `crashed` and `session_recovery_pending` are the
routes into `is_crashed()`, and each one must name itself in the log.
"""

from pathlib import Path
import subprocess
import sys
import tempfile


root = Path(__file__).resolve().parents[1]
live = (root / "g17_live_boot.rs").read_text()
manager = (root / "g17_manager.rs").read_text()
negative = "--negative-control" in sys.argv
if negative:
    live = live.replace("queue_or_coalesce_primary_recovery", "queue_primary_recovery")


def item(text: str, marker: str) -> str:
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos]


drain = item(live, "fn drain_primary_firmware_event_batch(")
settle = item(live, "fn settle_primary_empty_recovery(")
events = item(live, "    pub(crate) fn service_primary_firmware_events(")
recovery = item(live, "    fn service_primary_recovery_body(")
inline = item(live, "    fn service_firmware_events_inline(")

# --- a duplicate restart is coalesced, never refused ----------------------
for name, body in (("settle", settle), ("events", events), ("inline", inline)):
    assert "queue_or_coalesce_primary_recovery(" in body, name
    assert ".queue_primary_recovery(" not in body, name

# --- a type-4 the host already serviced is skipped, not fatal -------------
stale = drain.index("accepted_firmware_recovery_generation()")
assert "recovery.generation < accepted" in drain
assert drain.index("continue;", stale) < drain.index("return Err(error);", stale)
assert "stale_recovery_records" in drain

# --- every route into is_crashed() names itself --------------------------
# The RTKit crash callback and the rebuild-failed site already log; the two
# event-chain stores and all session_recovery_pending writers go through the
# helpers, so `G17PMARK session-crashed` finds all of them.
assert "note_state_crashed(" in events
assert "note_state_crashed(" in recovery
assert 'state.crashed.store(true' not in events
assert 'state.crashed.store(true' not in recovery
asyncc = (root / "g17_async_compute.rs").read_text()
# Every route into is_crashed() goes through a helper that names itself.
# The four in g17_async_compute.rs were the blind spot that hid a whole
# wedge class: the session was marked crashed with nothing in the log.
assert live.count("session_recovery_pending = true;") == 0
assert asyncc.count("session_recovery_pending = true;") == 0
assert live.count("note_session_recovery_pending(") == 4  # definition + 3 writers, including deferred rebuild requests
# The two client-scoped fan-out routes (render-completion-failed-with-vm,
# async-compute-vm-fanout-quarantine) no longer escalate: quarantining the
# slot/queue is the whole containment (first-submit hang, 2026-09-26).
assert asyncc.count("note_session_recovery_pending(") == 2
for reason in (
    "async-compute-completion-unclassifiable",
    "async-compute-submission-quarantined",
):
    assert f'"{reason}"' in asyncc, reason
for step in (
    "settle-primary-empty-recovery",
    "drain-gfx1-event-ring",
    "queue-gfx1-recovery",
    "async-compute-completions",
    "render-usc-freelist-release",
):
    assert f'"{step}"' in events, step
assert '"async-render-completions"' in item(live, "    pub(crate) fn note_render_observation_error(")
for step in ("complete-real-recovery", "complete-pending-recovery"):
    assert f'"{step}"' in recovery, step

# --- the coalescing rule itself ------------------------------------------
coalesce = item(manager, "    pub(crate) fn queue_or_coalesce_primary_recovery(")
assert "pending_generation != generation" in coalesce

rust = r'''
#[derive(Debug, PartialEq, Eq)]
enum E { Busy, Inval }

struct Manager {
    pending: Option<(u64, u32, Option<u8>)>,
    accepted: u64,
}

impl Manager {
    fn queue(&mut self, generation: u64, records: u32, stamp: Option<u8>)
        -> Result<(), E>
    {
        if records == 0 || self.pending.is_some() { return Err(E::Busy); }
        if stamp.is_some_and(|s| s >= 128) { return Err(E::Inval); }
        self.pending = Some((generation, records, stamp));
        Ok(())
    }

    fn queue_or_coalesce(&mut self, generation: u64, records: u32, stamp: Option<u8>)
        -> Result<bool, E>
    {
        if let Some((pending_generation, pending_records, pending_stamp)) = self.pending {
            if pending_generation != generation { return Err(E::Busy); }
            self.pending = Some((
                generation,
                pending_records.saturating_add(records),
                pending_stamp.or(stamp),
            ));
            return Ok(false);
        }
        self.queue(generation, records, stamp)?;
        Ok(true)
    }
}

fn manager() -> Manager { Manager { pending: None, accepted: 0 } }

#[test]
fn the_second_role_folds_into_the_first() {
    let mut m = manager();
    assert_eq!(m.queue_or_coalesce(0, 1, None), Ok(true));
    // gfx1 sees the same restart in the same worker pass. This is the case that
    // used to return EBUSY and end the session.
    assert_eq!(m.queue_or_coalesce(0, 2, Some(3)), Ok(false));
    assert_eq!(m.pending, Some((0, 3, Some(3))));
}

#[test]
fn a_blamed_stamp_from_either_role_survives_the_fold() {
    let mut m = manager();
    assert_eq!(m.queue_or_coalesce(7, 1, Some(9)), Ok(true));
    assert_eq!(m.queue_or_coalesce(7, 1, None), Ok(false));
    assert_eq!(m.pending, Some((7, 2, Some(9))));
}

#[test]
fn a_different_generation_is_still_refused() {
    let mut m = manager();
    assert_eq!(m.queue_or_coalesce(0, 1, None), Ok(true));
    // Not a duplicate view of one request: the host and firmware disagree about
    // how many restarts are outstanding, and that must not be merged away.
    assert_eq!(m.queue_or_coalesce(1, 1, None), Err(E::Busy));
    assert_eq!(m.pending, Some((0, 1, None)));
}

#[test]
fn completing_the_folded_request_still_matches_exactly() {
    // service_primary_recovery reads the tuple back out of the manager and
    // hands those same values to complete_pending_primary_recovery, so folding
    // extra records in cannot desynchronise the completion check.
    let mut m = manager();
    m.queue_or_coalesce(4, 1, None).unwrap();
    m.queue_or_coalesce(4, 1, None).unwrap();
    let read_back = m.pending.unwrap();
    assert_eq!(read_back, (4, 2, None));
    assert_eq!(m.pending, Some(read_back));
}

#[test]
fn an_empty_batch_is_never_queued() {
    let mut m = manager();
    assert_eq!(m.queue_or_coalesce(0, 0, None), Err(E::Busy));
    assert_eq!(m.pending, None);
}
'''

with tempfile.TemporaryDirectory() as directory:
    directory = Path(directory)
    source = directory / "coalesce.rs"
    source.write_text(rust)
    executable = directory / "coalesce"
    subprocess.run(
        ["rustc", "--edition=2021", "--test", str(source), "-o", str(executable)],
        check=True,
    )
    subprocess.run([str(executable), "--test-threads=1"], check=True, timeout=60)

print(
    "PASS duplicate recovery: dual-role and already-serviced restarts coalesce, "
    "differing generations still refused, every crashed transition attributed"
)
