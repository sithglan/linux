#!/usr/bin/env python3
"""G17P boot failures must stop CPUs before releasing firmware owners."""

from pathlib import Path
import subprocess
import sys
import tempfile


root = Path(__file__).resolve().parents[1]
live = (root / "g17_live_boot.rs").read_text()
lifecycle = (root / "g17_lifecycle.rs").read_text()
if "--negative-control" in sys.argv:
    live = live.replace("                core::mem::forget(manager);\n", "")


def item(text, marker, start=0):
    begin = text.index(marker, start)
    end = text.index("{", begin) + 1
    depth = 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[begin:end]


finish = item(live, "fn finish_boot_session(")
rebuild = item(live, "    fn rebuild_session_in_place(")
recovery = item(live, "impl g17_lifecycle::PersistentRuntimeRecovery for G17PLiveRuntime")
is_crashed = item(live, "    pub(crate) fn is_crashed(")
teardown = item(
    live,
    "    fn stop_cpu_pair(&mut self) -> bool {",
    live.index("impl g17_lifecycle::PersistentRuntimeTeardown"),
)
runtime_start = item(
    live,
    "    pub(crate) fn start(",
    live.index("impl G17PLiveRuntime"),
)

# The fallible tail borrows the owner graph; it cannot consume and implicitly
# drop the CPU pair or either RTKit transport on an error.
assert "Result<G17PBootSessionConfig>" in finish
assert "StartedCpuPair" not in finish
assert "rtkit::RtKit" not in finish

for marker, minimum_rollbacks in (
    ("fn boot_session(", 2),
    ("fn boot_initial_session(", 5),
):
    body = item(live, marker)
    assert "start_staged_in_place" in body
    assert (
        body.count("rollback_starting_session")
        + body.count("rollback_boxed_starting_session")
    ) >= minimum_rollbacks

# Initial boot has no persistent outer runtime to retain its manager, while a
# failed rebuild records the same condition for final teardown.
initial = item(live, "fn boot_initial_session(")
assert "if !rollback_boxed_starting_session(pdev, session)" in initial
assert "core::mem::forget(manager);" in initial
assert "&mut stop_unproven" in rebuild
assert "self.boot_stop_unproven = true;" in rebuild
assert teardown.index("if self.boot_stop_unproven") < teardown.index("self.cpus.as_mut()")

# A cleanly stopped replacement boot can fail after its role states were
# removed. Keep that state retryable even though neither role has a crash bit.
pause = item(recovery, "    fn pause_submissions(")
resume = item(recovery, "    fn resume_submissions(")
# Raised through the helper that logs the transition -- the flag is one of the
# three routes into is_crashed() and was the only silent one.
assert "note_session_recovery_pending(" in pause
assert "&mut self.session_recovery_pending," in pause
assert "self.session_recovery_pending = false;" in resume
assert "persistent_session_needs_recovery(" in is_crashed
assert "self.session_recovery_pending" in is_crashed

# No allocation failure may enter Rust's declaration-order partial drop after
# a successful firmware boot. Both fallible host allocations precede it.
boot_call = runtime_start.index("boot_initial_session(")
assert runtime_start.index("G17PCompletionSignal::new()?") < boot_call
assert runtime_start.index("KVec::with_capacity(") < boot_call
assert "?" not in runtime_start[runtime_start.index("Ok(Self {") :]

# Exercise the actual generic state machine, including partial start and both
# rollback outcomes, rather than a transcription of it.
with tempfile.TemporaryDirectory() as tmp:
    exe = Path(tmp) / "g17_lifecycle"
    subprocess.run(
        [
            "rustc",
            "--edition=2021",
            "--test",
            str(root / "g17_lifecycle.rs"),
            "-o",
            str(exe),
        ],
        check=True,
    )
    subprocess.run([str(exe), "--test-threads=1"], check=True)

print("PASS: G17P boot rollback retains every owner unless both CPUs stop")
