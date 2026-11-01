#!/usr/bin/env python3
"""Lock down the real PB-owner delta; --all also runs legacy fixture tests.

Some old m1n1 fixture tests already fail at HEAD. --all reports those failures
unchanged rather than refreshing expected hashes or silently excluding them.
"""
from pathlib import Path
from g17_config_test_support import with_g17_config
import argparse
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--all", action="store_true", help="also run every legacy builder fixture")
args = parser.parse_args()
source = r'''
#![allow(dead_code, non_upper_case_globals)]
mod module_parameters {
    pub struct Param(u32);
    impl Param { pub fn value(&self) -> &u32 { &self.0 } }
    pub static g17p_ta_oracle: Param = Param(0);
    pub static g17p_frag_oracle: Param = Param(0);
    pub static g17p_render_ta_qid_field: Param = Param(1);
}
mod g17_submission { pub const G17P_NATIVE_RENDER_DOORBELL_PRIORITY: u32 = 0; }
'''
names = [
    "tpc_extent_survives_cold_and_retained_descriptor_construction",
    "rce_boundaries_survive_retained_descriptor_rebuilds",
    "shared_usc_flist_id_drives_both_stage_registers_independently_of_pm_id",
    "descriptor_context_and_state_word_drive_every_native_work_key",
    "fragment_parameter_buffer_owner_is_independent_of_perf_map_and_tail",
]
with tempfile.TemporaryDirectory(prefix="g17-fragment-pb-owner-") as tmp:
    exe = str(Path(tmp) / "test")
    source += '#[path = "' + str(root / "g17_render.rs") + '"] mod g17_render;\n'
    source += r'''
#[test]
fn changing_pb_owner_changes_only_the_fragment_rce_owner_value() {
    use g17_render::*;
    let mut p = G17pRenderParameters::source_rsrc8();
    p.ta_hardware_buffer_id = 0;
    p.gpc_perf_state_map = 7;
    let objects = RenderDescriptorObjects { record_a: 0x1000, shared: 0x2000,
                                           record_b: 0x3000, zero: 0x4000 };
    let metadata = RenderDescriptorMetadata::source(RenderDescriptorKind::Fragment);
    for dimensions in [32, 512] {
        p.width = dimensions;
        p.height = dimensions;
        p.ta_hardware_buffer_id = 0;
        let mut original = [0u8; FRAGMENT_DESCRIPTOR_SIZE];
        build_fragment_descriptor(&p, objects, metadata, &mut original).unwrap();
        for owner in [1, 3, 5, 7, 127] {
            p.ta_hardware_buffer_id = owner;
            let mut changed = [0u8; FRAGMENT_DESCRIPTOR_SIZE];
            build_fragment_descriptor(&p, objects, metadata, &mut changed).unwrap();
            assert_eq!(&changed[..0x458], &original[..0x458]);
            assert_eq!(&changed[0x458..0x460], &(owner as u64).to_le_bytes());
            assert_eq!(&changed[0x460..], &original[0x460..]);
            let reference = changed;
            for perf in [0, 1, 3, 7, 15] {
                p.gpc_perf_state_map = perf;
                build_fragment_descriptor(&p, objects, metadata, &mut changed).unwrap();
                assert_eq!(changed, reference, "performance map must not select PB owner");
            }
        }
    }
}
'''
    source += r'''
#[test]
fn fragment_ptc_probe_changes_only_ptc_high_word() {
    use g17_render::*;
    for dimension in [32, 512, 1024] {
        let mut p = G17pRenderParameters::source_rsrc8();
        p.width = dimension; p.height = dimension;
        p.native_ta_registers = 0;
        let ta = build_ta_registers(&p).unwrap();
        let frag = build_fragment_registers(&p).unwrap();
        p.native_ta_registers = 8;
        assert_eq!(ta, build_ta_registers(&p).unwrap());
        let changed = build_fragment_registers(&p).unwrap();
        let mut deltas = 0;
        for (before, after) in frag.iter().zip(changed.iter()) {
            assert_eq!(before.number, after.number);
            if before != after {
                deltas += 1;
                assert_eq!(before.number, 0x0a5a9);
                assert_eq!(before.value, 0x0000006000400020);
                assert_eq!(after.value, 0x000000d500400020);
            }
        }
        assert_eq!(deltas, 1);
    }
}
'''
    subprocess.run(["rustc", "-Awarnings", "--edition=2021", "--test", "-", "-o", exe],
                   input=with_g17_config(source), text=True, check=True)
    listing = subprocess.check_output([exe, "--list"], text=True)
    for name in names:
        qualified = "g17_render::tests::" + name
        assert qualified + ": test" in listing
    assert "changing_pb_owner_changes_only_the_fragment_rce_owner_value: test" in listing
    if args.all:
        subprocess.run([exe], check=True)
    else:
        for name in names:
            subprocess.run([exe, "g17_render::tests::" + name, "--exact"], check=True)
        subprocess.run([exe, "changing_pb_owner_changes_only_the_fragment_rce_owner_value", "--exact"], check=True)
        subprocess.run([exe, "fragment_ptc_probe_changes_only_ptc_high_word", "--exact"], check=True)
print("actual PB-owner builders, exact delta and independent hardware-tail sources PASS")
