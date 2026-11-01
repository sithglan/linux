#!/usr/bin/env python3
"""A completed render fence must not depend on fallible retained cleanup."""

from pathlib import Path
import subprocess
import tempfile


root = Path(__file__).resolve().parents[1]
live = (root / "g17_live_boot.rs").read_text()
manager = (root / "g17_manager.rs").read_text()


def function(text: str, marker: str) -> str:
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos]


poll_manager = function(manager, "    pub(crate) fn commit_render_observation(")
poll_runtime = function(live, "    pub(crate) fn poll_deferred_render_completion(")
settle = function(live, "    fn settle_deferred_render_completion(")
service = function(live, "    fn service_async_render_completions(")
timeout = function(live, "    fn settle_failed_deferred_render(")

# Manager completion is a one-way commit: a cleanup failure cannot re-poll it.
assert poll_manager.index("self.render_slots.get_mut(pair_slot).tracker = self.render_slots.get_mut(pair_slot).queued_tracker.take();") < poll_manager.index(
    "return Ok(G17PRenderPollOutcome::Complete(completion));"
)
assert "retire_render_usc_freelist_at_completion" not in poll_runtime
assert "service_render_usc_freelist_release" not in poll_runtime

# Both event and watchdog completion paths share the same exact-once boundary.
assert "self.settle_deferred_render_completion(slot, observed)?" in service
assert service.index("self.settle_deferred_render_completion(slot, observed)?") < service.rindex(
    "if !self.render_fence_state.pending(slot)"
)
assert "self.settle_deferred_render_completion(render_slot, observed)" in timeout
assert settle.index("self.pending_render.get(index)") < settle.index(
    "self.render_fence_state.settle(render_slot)"
)
# The proven completion is committed (deferred for signalling with the runtime
# mutex released) before the fallible FList cleanup and any rebuild.
signal = settle.index("self.defer_render_completion(signal, observed);")
assert "signal.complete(Ok(" not in settle
retire = settle.index("retire_render_usc_freelist_at_completion(render_slot)")
# Deferred to the recovery worker with admission closed (stack depth).
recover = settle.index("self.request_session_rebuild(EAGAIN)")
assert signal < retire < recover

# Executable state-machine oracle. The manager first commits paired retirement
# and loses its tracker. The new host boundary then signals the exact job before
# FList cleanup. A cleanup failure recovers the session: a second client's job
# is failed exactly once and remains retained if processor stop is not proven.
rust = r'''
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FenceResult { Success, Error }

#[derive(Debug)]
struct Job {
    fence: Option<FenceResult>,
    mapping_retained: bool,
    active: bool,
}

impl Job {
    fn active() -> Self {
        Self { fence: None, mapping_retained: true, active: true }
    }

    fn complete(&mut self, result: FenceResult, release: bool) {
        if self.fence.is_some() { return; }
        self.fence = Some(result);
        if release { self.mapping_retained = false; }
        self.active = false;
    }

    fn fail_while_owned(&mut self) {
        if self.fence.is_none() { self.fence = Some(FenceResult::Error); }
        self.active = false;
    }
}

struct Runtime {
    tracker: bool,
    pending: bool,
    fence_armed: bool,
    flist_cleanup_fails: bool,
    stop_succeeds: bool,
    recovery_count: u32,
    current: Job,
    peer: Job,
}

impl Runtime {
    fn new(cleanup_fails: bool, stop_succeeds: bool) -> Self {
        Self {
            tracker: true, pending: true, fence_armed: true,
            flist_cleanup_fails: cleanup_fails, stop_succeeds,
            recovery_count: 0, current: Job::active(), peer: Job::active(),
        }
    }

    fn poll_paired_completion(&mut self) -> bool {
        if !self.tracker { return false; }
        // The production manager clears this before returning Complete.
        self.tracker = false;
        true
    }

    fn recover(&mut self) -> Result<(), ()> {
        self.recovery_count += 1;
        if !self.stop_succeeds {
            self.peer.fail_while_owned();
            return Err(());
        }
        self.peer.complete(FenceResult::Error, true);
        Ok(())
    }

    fn settle_completion(&mut self) -> Result<bool, ()> {
        if !self.pending { return Err(()); }
        if !self.fence_armed { return Ok(false); }
        self.fence_armed = false;
        self.pending = false;

        // The architectural change: this permission to recycle user BOs is
        // published before device-global FList housekeeping can fail.
        self.current.complete(FenceResult::Success, true);
        if self.flist_cleanup_fails { self.recover()?; }
        Ok(true)
    }
}

#[test]
fn cleanup_failure_cannot_turn_completed_client_into_watchdog_error() {
    let mut r = Runtime::new(true, true);
    assert!(r.poll_paired_completion());
    assert!(!r.poll_paired_completion(), "manager completion is not retryable");
    assert_eq!(r.settle_completion(), Ok(true));
    assert_eq!(r.current.fence, Some(FenceResult::Success));
    assert!(!r.current.mapping_retained);
    assert_eq!(r.peer.fence, Some(FenceResult::Error));
    assert!(!r.peer.mapping_retained);
    assert_eq!(r.recovery_count, 1);
}

#[test]
fn failed_processor_stop_keeps_peer_mapping_but_not_current_waiter() {
    let mut r = Runtime::new(true, false);
    assert!(r.poll_paired_completion());
    assert_eq!(r.settle_completion(), Err(()));
    assert_eq!(r.current.fence, Some(FenceResult::Success));
    assert!(!r.current.mapping_retained);
    assert_eq!(r.peer.fence, Some(FenceResult::Error));
    assert!(r.peer.mapping_retained, "unproven stop released firmware owner");
}

#[test]
fn successful_cleanup_neither_recovers_nor_disturbs_peer() {
    let mut r = Runtime::new(false, true);
    assert!(r.poll_paired_completion());
    assert_eq!(r.settle_completion(), Ok(true));
    assert_eq!(r.current.fence, Some(FenceResult::Success));
    assert_eq!(r.peer.fence, None);
    assert!(r.peer.active && r.peer.mapping_retained);
    assert_eq!(r.recovery_count, 0);
}
'''

with tempfile.TemporaryDirectory() as directory:
    directory = Path(directory)
    source = directory / "cleanup.rs"
    source.write_text(rust)
    executable = directory / "cleanup"
    subprocess.run(
        ["rustc", "--edition=2021", "--test", str(source), "-o", str(executable)],
        check=True,
    )
    subprocess.run([str(executable), "--test-threads=1"], check=True, timeout=60)

print(
    "PASS render completion cleanup: success fence precedes fallible FList work; "
    "recovery settles peers and failed stop retains ownership"
)
