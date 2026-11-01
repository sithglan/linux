#!/usr/bin/env python3
"""A departed compute client must not free what the firmware still names.

The firmware keeps a per-slot record pointer for every installed QID and sweeps
it periodically, dereferencing byte 0 of every non-NULL entry. The driver's only
clearing transaction, `unregister_compute_sksm_queue`'s disable pair, is a
suppressed no-op whenever `g17p_sksm_mmio` is 0 -- its default, because the SKSM
port raises an SError on this SoC. So nothing tells the firmware to forget a
queue, and a released client's SKSM entry mapping and context root have to
outlive it.

Preparing aliases does not reinstall firmware roots. A replacement must retain
the previous binding until its exact successful publication completes in the
same recovery epoch, or until both CPUs have stopped.
"""

from pathlib import Path
import subprocess
import sys
import tempfile


root = Path(__file__).resolve().parents[1]
asyncc = (root / "g17_async_compute.rs").read_text()
limits = (root / "g17_queue_limits.rs").read_text()
live = (root / "g17_live_boot.rs").read_text()
if "--negative-control" in sys.argv:
    asyncc = asyncc.replace("                q.owner = None;\n",
                            "                q.binding = None; q.context = None; q.owner = None;\n")


def item(text: str, marker: str) -> str:
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos]


release = item(asyncc, "    pub(crate) fn release_async_compute_queue(")
prepare = item(asyncc, "    pub(crate) fn prepare_async_compute_queue(")
completions = item(asyncc, "    fn service_async_compute_completions_matching(")
post_stop = item(asyncc, "    fn release_async_compute_after_processor_stop(")

# --- no client-teardown path may free firmware-visible state --------------
for name, body in (("release", release), ("completions", completions)):
    assert "q.binding = None" not in body, name
    assert "q.context = None" not in body, name
    assert "q.owner = None;" in body, name

# --- the two lawful boundaries still free it ------------------------------
# 1. prepare replacement aliases before moving old ownership, then retain it
assert prepare.index("bind_independent_compute_user_vm") < prepare.index("q.binding.take()")
assert "q.previous_binding = Some(G17PAsyncComputePreviousBinding" in prepare
assert "drop(q.binding.take())" not in prepare
assert "drop(q.previous_binding.take())" in completions
assert "handshake.epoch != ticket.recovery_epoch" in completions
assert "previous.publication" in completions
assert "ticket.submission, ticket.recovery_epoch" in completions
# 2. after both firmware CPUs have stopped
assert "async_compute_queues.clear()" in post_stop

# --- a retained graph must still be findable for reuse --------------------
assert "rebindable_released_compute_graph(" in prepare
assert "q.queue.requires_first_install()" in prepare
assert "rebindable_released_compute_graph(" in limits

# --- the rule this mirrors is still written down where it came from -------
owner = item(live, "    pub(crate) fn release_compute_owner(")
assert "q.binding = None" not in owner
assert "self.compute_owner = None;" in owner

rust = r'''
// Model the firmware's side: an installed slot holds a record pointer, the
// sweep dereferences every non-NULL slot, and nothing but a rebind or a
// processor stop ever changes what a slot names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page { Mapped(u32), Unmapped }

struct Fw { slot: Option<u32> }

impl Fw {
    // The periodic per-slot sweep: ldrb w9, [x0] on every non-NULL entry.
    fn sweep(&self, pages: &dyn Fn(u32) -> Page) -> Result<(), u32> {
        match self.slot {
            None => Ok(()),
            Some(record) => match pages(record) {
                Page::Mapped(_) => Ok(()),
                Page::Unmapped => Err(record),
            },
        }
    }
}

struct Driver { fw: Fw, live: Option<u32>, retained: Option<u32>, next: u32 }

impl Driver {
    fn new() -> Self { Driver { fw: Fw { slot: None }, live: None, retained: None, next: 1 } }

    fn open(&mut self) -> u32 {
        // The replacement becomes visible and completes before old ownership
        // is released. Production transition gaps/failure paths are exercised
        // by test_g17_async_compute_rebind and test_g17_async_retirement.
        let record = self.next;
        self.next += 1;
        self.live = Some(record);
        self.fw.slot = Some(record);   // install; never cleared by the driver
        self.retained = None;          // successful superseding completion
        record
    }

    fn close(&mut self) {
        // Give up the claim, keep what the firmware can still name.
        self.retained = self.live.take();
    }

    fn mapped(&self) -> impl Fn(u32) -> Page + '_ {
        move |record| {
            if self.live == Some(record) || self.retained == Some(record) {
                Page::Mapped(record)
            } else {
                Page::Unmapped
            }
        }
    }
}

#[test]
fn one_open_and_close_survives_the_sweep() {
    let mut d = Driver::new();
    d.open();
    d.close();
    assert_eq!(d.fw.sweep(&d.mapped()), Ok(()));
}

#[test]
fn many_terminals_opened_and_closed_survive_every_sweep() {
    // The user's reproducer: many short-lived clients on a running desktop.
    let mut d = Driver::new();
    for _ in 0..64 {
        d.open();
        assert_eq!(d.fw.sweep(&d.mapped()), Ok(()));
        d.close();
        assert_eq!(d.fw.sweep(&d.mapped()), Ok(()));
    }
}

#[test]
fn the_rebind_is_what_makes_dropping_the_old_record_safe() {
    let mut d = Driver::new();
    let first = d.open();
    d.close();
    let second = d.open();
    assert_ne!(first, second);
    // The slot now names the new record, so the old one is unreachable and its
    // pages may go.
    assert_eq!(d.fw.slot, Some(second));
    assert_eq!(d.fw.sweep(&d.mapped()), Ok(()));
}

#[test]
fn freeing_at_close_is_what_used_to_fault() {
    // The old behaviour, kept as the counter-example: dropping the binding at
    // close leaves the slot naming an unmapped page.
    let mut d = Driver::new();
    let record = d.open();
    d.live = None;
    d.retained = None;                  // q.binding = None; q.context = None
    assert_eq!(d.fw.sweep(&d.mapped()), Err(record));
}
'''

with tempfile.TemporaryDirectory() as directory:
    directory = Path(directory)
    source = directory / "retain.rs"
    source.write_text(rust)
    executable = directory / "retain"
    subprocess.run(
        ["rustc", "--edition=2021", "--test", str(source), "-o", str(executable)],
        check=True,
    )
    subprocess.run([str(executable), "--test-threads=1"], check=True, timeout=60)

print(
    "PASS async release retains firmware state: no client-teardown path frees a "
    "record the firmware still has installed; rebind and processor stop still do"
)
