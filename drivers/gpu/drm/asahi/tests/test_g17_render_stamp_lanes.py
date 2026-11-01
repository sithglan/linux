#!/usr/bin/env python3
"""Each render record points at its own per-queue stamp lane.

The TA and 3D records carry two pointers into the shared "descriptor zero"
pages (A: GPU and firmware; B: firmware-private). They are per-KSM-stamp-slot
u32 lanes indexed by the record's own queue ID: valid native captures show the
3D queues 1/3/8/10 at +0x04/+0x0c/+0x20/+0x28 and the TA queue 9 -- queue 10's
own pair partner -- at +0x24. The fragment writer indexed by QID; the tiling
writer wrote lane 0 for every TA, which is correct only for TA QID 0. So the
second bounded pair (QIDs 2/3) pointed its tiler at the first pair's live TA
lane. Measured 2026-09-07 on J700: with gnome-shell rendering on pair 0, every
render on pair 1 was retired by the firmware without the tiler ever stamping
its pass start, WSI or not, ordinal 0 or 1.

This compiles the real tail writer and checks the lanes for both pairs.
"""
from pathlib import Path
import re
import subprocess
import tempfile

src = Path(__file__).resolve().parent.parent
resources = (src / "g17_resources.rs").read_text()
render = (src / "g17_render.rs").read_text()


def block(source, pattern):
    match = re.search(pattern, source, re.M)
    assert match, pattern
    start = source.index("{", match.end())
    end, depth = start + 1, 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[match.start():end]


def constant(source, name):
    return re.search(r"^(?:pub\(crate\) )?const " + name + r": .*?;", source, re.M | re.S).group()


# Both production callers hand the writer the pair's own TA and 3D queue IDs.
calls = [m.start() for m in re.finditer(r"apply_g17p_generic_descriptor_tail\(", resources)]
production = [resources[c:c + 400] for c in calls if "fn apply_g17p_generic_descriptor_tail" not in resources[c - 3:c + 40]]
production = [p for p in production if "queue_pair" in p]
assert len(production) == 2, "expected the cold and the restage callers"
for call in production:
    assert re.search(r"queue_pair\.tiling,\s*(?:self\.)?queue_pair\.fragment,", call), call[:200]

body = "\n".join([
    "#![allow(dead_code)]",
    constant(resources, "G17P_BOOTSTRAP_DESCRIPTOR_ZERO_A_VA"),
    constant(resources, "G17P_BOOTSTRAP_DESCRIPTOR_ZERO_B_VA"),
    "mod g17_render {",
    constant(render, "TA_DESCRIPTOR_SIZE"),
    constant(render, "FRAGMENT_DESCRIPTOR_SIZE"),
    "}",
    block(resources, r"^fn put_u64\("),
    block(resources, r"^fn apply_g17p_generic_descriptor_tail\("),
    r"""
fn u64_at(b: &[u8], at: usize) -> u64 { u64::from_le_bytes(b[at..at+8].try_into().unwrap()) }

fn lanes(tiling_qid: u16, fragment_qid: u16) -> [u64; 4] {
    let mut tiling = [0u8; g17_render::TA_DESCRIPTOR_SIZE];
    let mut fragment = [0u8; g17_render::FRAGMENT_DESCRIPTOR_SIZE];
    apply_g17p_generic_descriptor_tail(
        &mut tiling, &mut fragment, tiling_qid, fragment_qid, 0x1000, 0x2000, 0x3000).unwrap();
    [u64_at(&tiling, 0x08a6), u64_at(&tiling, 0x08ae), u64_at(&fragment, 0x2140), u64_at(&fragment, 0x2148)]
}

#[test]
fn the_first_pair_is_byte_identical_to_the_qualified_encoding() {
    assert_eq!(lanes(0, 1), [
        G17P_BOOTSTRAP_DESCRIPTOR_ZERO_A_VA, G17P_BOOTSTRAP_DESCRIPTOR_ZERO_B_VA,
        G17P_BOOTSTRAP_DESCRIPTOR_ZERO_A_VA + 4, G17P_BOOTSTRAP_DESCRIPTOR_ZERO_B_VA + 4,
    ]);
}

#[test]
fn the_second_pair_owns_lanes_two_and_three() {
    assert_eq!(lanes(2, 3), [
        G17P_BOOTSTRAP_DESCRIPTOR_ZERO_A_VA + 8, G17P_BOOTSTRAP_DESCRIPTOR_ZERO_B_VA + 8,
        G17P_BOOTSTRAP_DESCRIPTOR_ZERO_A_VA + 12, G17P_BOOTSTRAP_DESCRIPTOR_ZERO_B_VA + 12,
    ]);
}

#[test]
fn no_two_queues_share_a_lane() {
    let mut seen = std::collections::BTreeSet::new();
    for (ta, frag) in [(0u16, 1u16), (2, 3)] {
        for lane in lanes(ta, frag) { assert!(seen.insert(lane), "lane {lane:#x} shared"); }
    }
    assert_eq!(seen.len(), 8);
}

#[test]
fn native_capture_lanes_reproduce() {
    // 3D queues 1/3/8/10 and TA queue 9, as measured on macOS.
    for (qid, offset) in [(1u16, 0x04u64), (3, 0x0c), (8, 0x20), (10, 0x28)] {
        assert_eq!(lanes(0, qid)[2], G17P_BOOTSTRAP_DESCRIPTOR_ZERO_A_VA + offset);
    }
    assert_eq!(lanes(9, 10)[0], G17P_BOOTSTRAP_DESCRIPTOR_ZERO_A_VA + 0x24);
}
""",
])
with tempfile.TemporaryDirectory(prefix="g17-stamp-lanes-") as tmp:
    binary = Path(tmp) / "tests"
    subprocess.run(["rustc", "-Awarnings", "--edition=2021", "--test", "-", "-o", str(binary)],
                   input=body, text=True, check=True)
    subprocess.run([str(binary), "--test-threads=1"], check=True)
print("PASS: TA and 3D records each point at their own per-queue stamp lane")
