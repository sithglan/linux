#!/usr/bin/env python3
"""Run the real retained-ring planner tests without kernel-only dependencies."""
from pathlib import Path
import re
import subprocess
import tempfile
from g17_native_channel_test_support import native_channel_helpers

source_dir = Path(__file__).resolve().parent.parent
source = (source_dir / "g17_resources.rs").read_text()


def block(pattern):
    match = re.search(pattern, source, re.M)
    assert match, pattern
    start = match.start()
    at = source.index("{", match.end())
    depth = 1
    end = at + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


constant = re.search(
    r"pub\(crate\) const G17P_RETAINED_RENDER_ORDINALS: u64 = .*?;", source
).group()
names = [
    "G17P_RENDER_QUEUE_METADATA_BASE",
    "G17P_RENDER_QUEUE_GRAPH_SIZE",
    "G17P_USER_TILING_RING", "G17P_USER_TILING_POINTERS",
    "G17P_USER_TILING_OPTIONAL", "G17P_USER_TILING_EVENT",
    "G17P_USER_TILING_ENTRY_SIGNAL", "G17P_RENDER_SHARED_CONTROL_INNER",
]
constants = "\n".join(
    re.search(r"(?:pub\(crate\) )?const " + name + r": usize = .*?;", source).group()
    for name in names
)
submission = (source_dir / "g17_submission.rs").read_text()
sizes = "\n".join(
    re.search(r"pub\(crate\) const " + name + r": usize = .*?;", submission).group()
    for name in ["G17P_COMPUTE_OPTIONAL_EVENT_SIZE", "G17P_COLD_OPENING_OPTIONAL_SIZE", "G17P_COLD_OPENING_EVENT_SIZE"]
)
body = "\n".join([
    "#![allow(dead_code)]",
"type Result<T=()> = core::result::Result<T,i32>;",
"type LiveResult<T=()> = core::result::Result<T,i32>;", "const EINVAL:i32=22;",
    block(r"^fn g17p_live_word_bounds\("),
    block(r"^fn g17p_live_read_u32\("),
    block(r"^fn g17p_live_read_u64\("),
    block(r"^fn g17p_live_write_u32\("),
    block(r"^fn g17p_live_write_u64\("),

    constant,
    constants,
    "mod g17_submission {" + sizes + native_channel_helpers(submission) + "}",
    "#[derive(Debug, Copy, Clone, PartialEq, Eq)]",
    block(r"^struct G17PRenderItemPlan\s*"),
    block(r"^impl G17PRenderItemPlan\s*"),
    block(r"^struct G17PLateTilingBytes\s*"),
    block(r"^impl G17PLateTilingBytes\s*"),
    block(r"^fn put_u32\("),
    "#[test]",
    block(r"^    fn retained_render_item_plan_matches_native_append_and_bounds\(\)\s*"),
    "#[test]",
    block(r"^    fn retained_render_item_copies_preserve_prior_groups_and_firmware_cursors\(\)\s*"),
    "#[test]",
    block(r"^    fn retained_tiling_phases_publish_modular_producers\(\)\s*"),
])
with tempfile.TemporaryDirectory(prefix="g17-render-item-append-") as temp:
    binary = Path(temp) / "tests"
    subprocess.run(
        ["rustc", "--edition=2021", "--test", "-", "-o", str(binary)],
        input=body, text=True, check=True,
    )
    subprocess.run([str(binary)], check=True)

# Tie the planner bound to the actual advertised firmware geometry. Test the
# production callers too: no first-submit-only prefix call may remain.
assert "put_u64_le(out, 0x60, 0x500);" in submission
manager = (source_dir / "g17_manager.rs").read_text()
assert "g17p_render_published_prefix()" not in manager
assert ".stage_fragment_kick(fragment_stamp)" not in manager
assert source.count("plan.copy_items(raw, Self::FRAGMENT_RING") == 2
print("actual planner bounds, append copies, firmware cursor preservation and caller checks PASS")
