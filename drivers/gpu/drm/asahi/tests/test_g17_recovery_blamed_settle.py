#!/usr/bin/env python3
"""A recovery-blamed settle must be explained, and must not be asked the wrong question.

`retire_abandoned_render_if_firmware_retired` reaches its `true` through
`!completion.retired || !state.job_list_empty || execution.observed()`. The
guard therefore separately demands an unlinked list and no exact timestamps,
which is satisfiable only when at least one timestamp is zero. It is a nop-retire
classifier -- it can only ever answer `true` for a group that did NOT execute.

The recovery-blamed settle asks it anyway. A stamp-blamed recovery serviced with
a live render scheduler deliberately skips the generation-0 timestamp wipe "so
the render's progress markers survive", so the timestamps are non-zero by
design, `observed()` is true, and `firmware-retired=false` is arithmetic rather
than evidence -- after which the caller poisons the client's VM and marks the
session crashed.

Two properties are pinned here: the settle path is always explained, and the
relaxation keeps BOTH ownership proofs so it is inert on the outcome that would
refute the diagnosis.
"""

from pathlib import Path
import subprocess
import sys
import tempfile


root = Path(__file__).resolve().parents[1]
live = (root / "g17_live_boot.rs").read_text()
manager = (root / "g17_manager.rs").read_text()
completion = (root / "g17_completion.rs").read_text()
if "--negative-control" in sys.argv:
    manager = manager.replace("|| (execution.observed() && !accept_executed)",
                              "|| execution.observed()")


def item(text: str, marker: str) -> str:
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos]


retire = item(live, "    fn retire_failed_deferred_render(")
decide = item(manager, "    pub(crate) fn retire_abandoned_render_if_firmware_retired(")
observed = item(completion, "    pub(crate) fn observed(")

# --- the classifier runs on BOTH settle paths, and says which one ---------
assert "report_render_wait_timeout(" in retire
report = retire.index("report_render_wait_timeout(")
assert retire.index('"recovery-blamed"', report) > report
assert retire.index('"inline-wait-expired"', report) > report
# and it is no longer gated behind the timed-out branch
assert "if timed_out {\n            self.manager" not in retire

# --- the relaxation drops observed() and NOTHING else --------------------
assert "|| (execution.observed() && !accept_executed)" in decide
assert "!completion.retired" in decide          # cursors at the published target
assert "|| !state.job_list_empty" in decide     # unlinked from the firmware list
# The recovery relaxation remains disabled in the qualified configuration.
assert "!timed_out" in retire
assert "crate::g17_config::RENDER_RECOVERY_RETIRE" in retire
assert "const RENDER_RECOVERY_RETIRE: u32 = 0;" in (root / "g17_config.rs").read_text()

# A delayed copy of an already-serviced recovery event from the other RTKit
# role must be discarded before its numeric stamp/QID can be attributed to the
# new owner that reused that slot.
drain = item(live, "fn drain_primary_firmware_event_batch(")
stale = item(drain, "if recovery.generation < accepted {")
assert "continue;" in stale
assert "batch.blamed_stamp" not in stale
assert drain.index("if recovery.generation < accepted {") < drain.index(
    "if recovery.stamp_index != -1 {"
)


def attribute(accepted, generation, stamp, owners):
    if generation < accepted:
        return None
    if generation != accepted:
        raise ValueError("future recovery generation")
    return owners[stamp]


owners = {17: "old-owner"}
assert attribute(4, 4, 17, owners) == "old-owner"
# The first role's callback completed recovery and the physical QID was reused.
owners[17] = "new-owner"
# The second role's delayed generation-4 callback cannot blame that new owner.
assert attribute(5, 4, 17, owners) is None

# --- the premise the whole argument rests on -----------------------------
assert "job_list_empty" not in observed
assert "tiling_timestamps" in observed and "fragment_timestamps" in observed

rust = r'''
#[derive(Clone, Copy)]
struct Execution { job_list_empty: bool, tiling: [u64; 2], fragment: [u64; 2] }

impl Execution {
    fn observed(self) -> bool {
        self.tiling.iter().all(|v| *v != 0)
            && self.fragment.iter().all(|v| *v != 0)
    }
}

// The guard, both modes.
fn retires(retired: bool, job_list_empty: bool, e: Execution, accept_executed: bool) -> bool {
    !(!retired || !job_list_empty || (e.observed() && !accept_executed))
}

const EXECUTED: Execution =
    Execution { job_list_empty: true, tiling: [1, 1], fragment: [1, 2] };
const NOP_RETIRED: Execution =
    Execution { job_list_empty: true, tiling: [0, 0], fragment: [0, 0] };
const STILL_LINKED: Execution =
    Execution { job_list_empty: false, tiling: [1, 1], fragment: [1, 2] };

#[test]
fn strict_mode_can_only_ever_accept_a_group_that_did_not_execute() {
    // The guard independently requires list unlink and no exact execution.
    assert!(!retires(true, true, EXECUTED, false));
    assert!(retires(true, true, NOP_RETIRED, false));
}

#[test]
fn the_relaxation_accepts_the_executed_group() {
    assert!(retires(true, true, EXECUTED, true));
}

#[test]
fn the_relaxation_is_inert_when_the_firmware_still_holds_the_group() {
    // This is the outcome that would refute the diagnosis. Retaining the
    // job_list_empty term makes the parameter change nothing in that case.
    assert!(!retires(true, false, STILL_LINKED, true));
    assert!(!retires(true, false, STILL_LINKED, false));
}

#[test]
fn the_relaxation_never_overrides_the_cursor_proof() {
    // completion.retired is "both queue cursors at the published target".
    assert!(!retires(false, true, EXECUTED, true));
    assert!(!retires(false, true, NOP_RETIRED, true));
}

#[test]
fn nop_retire_still_works_in_the_relaxed_mode() {
    assert!(retires(true, true, NOP_RETIRED, true));
}
'''

with tempfile.TemporaryDirectory() as directory:
    directory = Path(directory)
    source = directory / "settle.rs"
    source.write_text(rust)
    executable = directory / "settle"
    subprocess.run(
        ["rustc", "--edition=2021", "--test", str(source), "-o", str(executable)],
        check=True,
    )
    subprocess.run([str(executable), "--test-threads=1"], check=True, timeout=60)

print(
    "PASS recovery-blamed settle: explained on both paths; the relaxation drops "
    "only observed() and stays inert while the firmware still holds the group"
)
