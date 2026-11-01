#!/usr/bin/env python3
"""iso4 recovery-isolation REPLAY and inherited-error rules.

Runs the production pure functions from g17_recovery_isolation.rs (through
the stub builder) and checks the ordering of the replay path in
g17_async_compute.rs: a spared kick is published again only after the
retirement witness returned its queue to service, with its completion words
reset, its admission share re-armed and its fence still unsignalled.
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


compute = (root / "g17_async_compute.rs").read_text()
scan = item(compute, "    fn retire_witnessed_quarantined_compute(")
top = item(compute, "    fn release_firmware_retired_quarantined_compute(")
nested = item(compute, "    fn release_firmware_retired_quarantined_compute_nested(")
witness = scan + top
replay = item(compute, "    fn replay_spared_compute(")
native = item(compute, "    pub(crate) fn async_compute_native_dependency(")

# Kept kicks are published only after this scan, and only for a queue that
# the witness returned to service; otherwise they fail exactly as before.
assert witness.index("return_witnessed_compute_queue(") < witness.index("replays.push_within_capacity(")
assert "let kept = if returned {" in witness
assert witness.index("for (owner, kept) in replays.into_iter()") > witness.index("Err(error) => {\n                    q.awaiting_retirement_witness = false;")
# Only the top-level witness publishes. A nested scan (event/recovery service,
# possibly inside a publication) skips a replay candidate before changing it,
# leaving it awaiting the next top-level witness.
assert "replay_spared_compute" in top and "replay_spared_compute" not in nested
assert "retire_witnessed_quarantined_compute(reason, true)" in top
assert "retire_witnessed_quarantined_compute(reason, false)" in nested
skip = scan.index("if !replay && decision == crate::g17_recovery_isolation::ComputeReplay::Replay {")
assert skip < scan.index("q.awaiting_retirement_witness = false;\n                    let replay_candidate")
assert "replay_spared_compute" not in scan
# The decision sees every kept kick's own words and any native consumer.
assert "completion.replay_witness_words()" in witness
assert "completion.has_native_consumer()" in witness
# Per kick: budget, then reset words, then re-arm admission, then publish.
assert replay.index("begin_replay()") < replay.index("reset_for_replay()") \
    < replay.index("rearm_admission()") < replay.index("publish_async_compute(")
# A failed republication fails every later kick of the queue, in order.
assert "Some(error) => Err(error)," in replay
assert "completion.complete(Err(ENODATA));" in replay
# The class is never reset: a replay stays spared (no VM status).
assert "set_recovery_class" not in replay
# A render ordered on a kick's native token marks it; a spared queue that is
# still waiting for its witness fails the render without a device-loss code.
assert native.index("completion.note_native_consumer()") > native.index("if state > 0 { return Ok(None); }")
assert native.index("return Err(ECANCELED);") < native.index("if q.quarantined || handshake.state != 0")

tests = r'''
use g17_recovery_isolation as iso;
const SPARED: iso::ComputeReplayQueue = iso::ComputeReplayQueue {
    spared_deferred: true, spared_quarantine: true, released: false, owner_matches: true,
    native_consumer: false,
};
#[test] fn replays_only_provably_unexecuted_spared_kicks() {
    use iso::ComputeReplay::*;
    let mask = iso::DEFAULT_MASK;
    assert_eq!(iso::DEFAULT_MASK & iso::REPLAY, iso::REPLAY);
    // Zero-duration front kick plus never-reached kicks behind it.
    assert_eq!(iso::compute_replay_decision(mask, SPARED,
        [Some([2263456667u64, 2263456667]), Some([0, 0]), Some([0, 0])]), Replay);
    // Any executed, started or unprovable kick refuses the whole queue.
    assert_eq!(iso::compute_replay_decision(mask, SPARED, [Some([0u64, 0]), Some([1, 2])]), Refused);
    assert_eq!(iso::compute_replay_decision(mask, SPARED, [Some([7u64, 0])]), Refused);
    assert_eq!(iso::compute_replay_decision(mask, SPARED, [Some([0u64, 0]), None]), Refused);
    // A render already released against a cancelled token refuses it too.
    let consumed = iso::ComputeReplayQueue { native_consumer: true, ..SPARED };
    assert_eq!(iso::compute_replay_decision(mask, consumed, [Some([0u64, 0])]), Refused);
    // Not a spared deferred, returning, owned queue: the old path.
    for q in [iso::ComputeReplayQueue { spared_deferred: false, ..SPARED },
              iso::ComputeReplayQueue { spared_quarantine: false, ..SPARED },
              iso::ComputeReplayQueue { released: true, ..SPARED },
              iso::ComputeReplayQueue { owner_matches: false, ..SPARED }] {
        assert_eq!(iso::compute_replay_decision(mask, q, [Some([0u64, 0])]), NotCandidate);
    }
    assert_eq!(iso::compute_replay_decision(mask & !iso::REPLAY, SPARED, [Some([0u64, 0])]), NotCandidate);
    assert_eq!(iso::compute_replay_decision(mask & !iso::INNOCENT, SPARED, [Some([0u64, 0])]), NotCandidate);
    assert_eq!(iso::compute_replay_decision(mask, SPARED, []), NotCandidate);
}
#[test] fn replay_budget_is_bounded() {
    assert_eq!(iso::MAX_REPLAYS, 3);
    assert!(iso::replay_budget_left(0) && iso::replay_budget_left(2));
    assert!(!iso::replay_budget_left(3));
}
#[test] fn inherited_device_loss_is_not_the_consumers_own() {
    for e in [-5, -110, -19] {
        assert_eq!(iso::inherited_errno(iso::INNOCENT, e), -125);
        assert_eq!(iso::inherited_errno(0, e), e);
    }
    for e in [-125, -61, -16, -4, -512] {
        assert_eq!(iso::inherited_errno(iso::INNOCENT, e), e);
    }
}
'''

with tempfile.TemporaryDirectory() as d:
    f = Path(d) / "replay.rs"
    f.write_text(recovery_isolation_mod() + tests)
    subprocess.run(["rustc", "--edition=2021", "--test", str(f), "-o", str(Path(d) / "replay")], check=True)
    subprocess.run([str(Path(d) / "replay")], check=True)
print("ok")
