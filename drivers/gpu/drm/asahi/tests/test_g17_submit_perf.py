#!/usr/bin/env python3
"""Run throughput arithmetic/backoff tests and check retained staging/logging.

The production render path returns a scheduler fence before completion. The
output-fence harness checks that asynchronous ownership separately; this test
must not resurrect the removed synchronous submit/wait sequence.
"""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
perf = (root / "g17_perf.rs").read_text()
drm = (root / "g17_drm.rs").read_text()
resources = (root / "g17_resources.rs").read_text()
manager = (root / "g17_manager.rs").read_text()
asahi = (root / "asahi.rs").read_text()


def block(source, marker):
    start = source.index(marker)
    brace = source.index("{", start)
    depth = 1
    end = brace + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


# --- the module's own unit tests, actually executed -------------------------
with tempfile.TemporaryDirectory() as directory:
    source = Path(directory) / "g17_perf.rs"
    source.write_text(perf)
    executable = Path(directory) / "g17_perf"
    subprocess.run(
        ["rustc", "--edition=2021", "--test", "-A", "dead_code", str(source),
         "-o", str(executable)],
        check=True,
    )
    completed = subprocess.run(
        [str(executable), "--test-threads=1"],
        check=True, capture_output=True, text=True,
    )
ran = re.search(r"(\d+) passed", completed.stdout)
assert ran and int(ran.group(1)) >= 6, completed.stdout
assert "0 failed" in completed.stdout, completed.stdout

# --- descriptors are encoded straight into the selected generation ---------
# v91 direct descriptors replace j700's staging image and copy
# (test_g17_direct_render_descriptors.py covers the encoding). Builders still
# zero exactly their prefixes, so no byte carries a previous submission.
restage = block(resources, "    pub(crate) fn restage_command(")
assert "staging_descriptors" not in restage
assert "self.select_render_command_generation(submission_ordinal)?" in restage
render = (root / "g17_render.rs").read_text()
assert "raw[..TA_DESCRIPTOR_SIZE].fill(0);" in block(render, "pub(crate) fn build_ta_descriptor(")
assert "raw[..FRAGMENT_DESCRIPTOR_SIZE].fill(0);" in block(render, "pub(crate) fn build_fragment_descriptor(")
assert "render_state_va = self.render_state.iova()" in restage

# --- nothing per-submission logs unconditionally ---------------------------
headers = block(drm, "    fn log_cmdbuf_headers(")
assert headers.index("g17p_verbose()") < headers.index("while offset")
async_submit = block(drm, "    fn submit_compute_async(")
queued = async_submit.index('"G17P async submit: QUEUED')
gate = async_submit.rindex("crate::util::g17p_verbose()", 0, queued)
assert queued - gate < 120, "the async QUEUED line is not behind the verbose gate"

# Throughput reporting takes a device-wide mutex on every render and compute
# submission, so production must leave it opt-in.
perf_param = asahi[asahi.index("g17p_perf_window:"):]
perf_param = perf_param[:perf_param.index("},")]
assert "default: 0" in perf_param

# With accounting disabled, the hot path must not even sample the host clock.
# These reads are cheap individually but occur twice for every render and once
# for every compute submission, so merely skipping the reporting mutex is not
# sufficient.
guarded = block(drm, "    fn submit_guarded(")
assert "let perf_enabled = *module_parameters::g17p_perf_window.value() != 0;" in guarded
assert "let mut entered = if perf_enabled" in guarded
assert "entered: Option<Instant<Monotonic>>" in async_submit
render_async = block(drm, "    fn submit_render_async(")
assert "entered: Option<Instant<Monotonic>>" in render_async
assert "sample.ioctl_ns = entered.elapsed()" in render_async

print(
    "PASS submit perf: backoff schedule and window arithmetic execute; "
    "asynchronous fence ownership is covered separately; "
    "staging reuse is scratch-only and zeroed; no ungated log or clock sample"
)
