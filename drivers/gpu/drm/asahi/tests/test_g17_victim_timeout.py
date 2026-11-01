#!/usr/bin/env python3
"""t-1179 recovery-isolation bit7 VICTIM_DEFER.

A host scheduler timeout of a compute queue that is head-of-line blocked
behind another VM's older unfinished kick must not poison its VM. Runs the
production decision from g17_recovery_isolation.rs (through the stub builder)
on the 2026-10-06 06:47:56 field case and checks how the compute scheduler
job and the runtime use it.
"""
from pathlib import Path
import subprocess
import tempfile
from g17_recovery_isolation_test_support import recovery_isolation_mod

root = Path(__file__).resolve().parents[1]


def item(text, marker):
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos]


job = (root / "g17_job.rs").read_text()
compute = (root / "g17_async_compute.rs").read_text()
drm = (root / "g17_drm.rs").read_text()

# The scheduler job asks before the opt-in progress probe and renews only on
# Defer, counting its own renewals; replay credit keeps precedence.
false_timeout = item(job, "impl<B: G17PQueueBackend> sched::JobImpl for G17PScheduledJob<B> {")
false_timeout = item(false_timeout, "    fn false_timeout(")
assert false_timeout.index("take_replay_timeout_credit()") < false_timeout.index("victim_timeout(") \
    < false_timeout.index("g17p_progress_timeout_probe")
assert "== crate::g17_recovery_isolation::VictimTimeout::Defer" in false_timeout
assert "job.victim_renewals += 1;" in false_timeout
assert "victim_renewals: 0," in job
# The async backend routes it to the runtime for its own owner.
assert "classify_compute_timeout(self.owner, packet.submission_id(), renewals)" in drm

classify = item(compute, "    pub(crate) fn classify_compute_timeout(")
# Gated, retirement pass first, never for a quarantined queue or a job the
# queue no longer holds, and only while the queue's own front is unfinished.
assert classify.index("iso::enabled(iso::VICTIM_DEFER)") < classify.index("service_async_compute_completions()") \
    < classify.index("iso::victim_timeout(")
assert "if q.quarantined || !q.inflight.iter().any(" in classify
assert ".filter(|front| front.observed.is_none())" in classify
# Other queues only, compared by their oldest kick; a queue quarantined
# without a pending retirement witness never counts as a blocker.
assert ".filter(|(other, _)| *other != index)" in classify
assert ".filter(|_| !other.quarantined || other.awaiting_retirement_witness)" in classify
# Only Spare reclassifies kicks, and only as spared (no VM status on failure).
spare = classify.index("if decision == iso::VictimTimeout::Spare {")
assert classify.index("set_recovery_class(iso::CLASS_SPARED)") > spare
assert classify.count("set_recovery_class") == 1
# Publication order is taken before the kick becomes visible in `inflight`.
publish = item(compute, "    fn publish_async_compute_inner(")
assert publish.index("COMPUTE_PUBLICATION") < publish.index("q.inflight.push(G17PAsyncComputeActive {")
assert "observed: None, published," in publish
# The quarantine that follows Spare takes the existing all-spared path.
quarantine = item(compute, "    pub(crate) fn quarantine_async_compute(")
assert "q.spared_quarantine = all_spared && q.awaiting_retirement_witness;" in quarantine
assert "terminal_status = None;" in quarantine

tests = r'''
use g17_recovery_isolation as iso;
use iso::VictimTimeout::*;
fn head(vm: u64, published: u64, finished: bool) -> iso::ComputeHead {
    iso::ComputeHead { vm, published, finished }
}
#[test] fn default_on_and_bounded() {
    assert_eq!(iso::DEFAULT_MASK & iso::VICTIM_DEFER, iso::VICTIM_DEFER);
    assert_eq!(iso::DEFAULT_MASK & iso::GUILTY_GRACE, 0);
    assert_eq!(iso::VICTIM_MAX_RENEWALS, 8);
}
#[test] fn field_case_gnome_shell_behind_vkd3d() {
    // vkd3d test: vm 25834, oldest kick published first and never finished.
    // gnome-shell: vm 5, its kick published right after.
    let mask = iso::DEFAULT_MASK;
    let culprit = head(25834, 100, false);
    for renewals in 0..iso::VICTIM_MAX_RENEWALS {
        assert_eq!(iso::victim_timeout(mask, 5, 101, renewals, [culprit]), (Defer, Some(100)));
    }
    // The blocker retired (it leaves `inflight`): the next check is Own and the
    // still-pending kick has simply been waiting -- it completes normally.
    assert_eq!(iso::victim_timeout(mask, 5, 101, 2, []), (Own, None));
    // Budget gone with the blocker still there: spared, never poisoned.
    assert_eq!(iso::victim_timeout(mask, 5, 101, iso::VICTIM_MAX_RENEWALS, [culprit]), (Spare, Some(100)));
    // The culprit itself has no older blocker: it keeps today's behaviour.
    assert_eq!(iso::victim_timeout(mask, 25834, 100, 0, [head(5, 101, false)]), (Own, None));
}
#[test] fn only_older_unfinished_kicks_of_other_vms_block() {
    let mask = iso::DEFAULT_MASK;
    // Same VM (another queue of the same client) never excuses it.
    assert_eq!(iso::victim_timeout(mask, 5, 101, 0, [head(5, 100, false)]), (Own, None));
    // A finished (latched, not yet retired) head does not block.
    assert_eq!(iso::victim_timeout(mask, 5, 101, 0, [head(7, 100, true)]), (Own, None));
    // Newer work of others does not block.
    assert_eq!(iso::victim_timeout(mask, 5, 101, 0, [head(7, 102, false)]), (Own, None));
    // The oldest blocker is reported.
    assert_eq!(iso::victim_timeout(mask, 5, 101, 0,
        [head(7, 90, false), head(8, 60, false), head(9, 80, true)]), (Defer, Some(60)));
}
#[test] fn bits_off_restore_old_behaviour() {
    let culprit = [head(25834, 100, false)];
    assert_eq!(iso::victim_timeout(iso::DEFAULT_MASK & !iso::VICTIM_DEFER, 5, 101, 0, culprit), (Own, None));
    assert_eq!(iso::victim_timeout(0, 5, 101, 0, culprit), (Own, None));
    // Without INNOCENT the deferral still runs, but exhaustion is the old path.
    let no_innocent = iso::DEFAULT_MASK & !iso::INNOCENT;
    assert_eq!(iso::victim_timeout(no_innocent, 5, 101, 0, culprit), (Defer, Some(100)));
    assert_eq!(iso::victim_timeout(no_innocent, 5, 101, iso::VICTIM_MAX_RENEWALS, culprit), (Own, Some(100)));
}
'''

with tempfile.TemporaryDirectory() as d:
    f = Path(d) / "victim.rs"
    f.write_text(recovery_isolation_mod() + tests)
    subprocess.run(["rustc", "--edition=2021", "--test", str(f), "-o", str(Path(d) / "victim")], check=True)
    subprocess.run([str(Path(d) / "victim")], check=True)
print("ok")
