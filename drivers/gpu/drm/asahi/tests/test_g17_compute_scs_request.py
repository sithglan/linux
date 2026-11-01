#!/usr/bin/env python3
"""The per-kick ray-traversal (SCS) scratch request must reach three registers.

AGXMetalG17P sizes a compute pass's shader-context-state section and the kext's
register builder (generateCLRegisterListG16Common) programs that request as
0x101d1 = pages | MiB << 16, 0xd479 = extras, and 0x107a1 = window << 16 |
0x0100_0000 whenever any page is requested, falling back to 0x00ff_0000 when
none is. The connection between the previously unconditional zero-page form
and zero-valued SCS readbacks remains a hardware-validation question.

This test pins both halves: an empty request still reproduces the historical
constants byte for byte, and a real request packs the fields the way the kext
does.
"""
from pathlib import Path
from g17_config_test_support import with_g17_config
import json
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
compute = (root / "g17_compute.rs").read_text()
params = (root / "asahi.rs").read_text()


def block(text, marker):
    a = text.index(marker)
    i = text.index("{", a) + 1
    depth = 1
    while depth:
        depth += (text[i] == "{") - (text[i] == "}")
        i += 1
    return text[a:i]


# The register program must consume the request, not the old constants.
program = block(compute, "pub(crate) fn build_compute_registers_with_scs(")
assert "ComputeRegister::new(0x101d1, scs.size_word)" in program
assert "ComputeRegister::new(0x0d479, scs.extras)" in program
assert "ComputeRegister::new(0x107a1, scs.window_word)" in program
assert "0x101d1, 0)" not in program
assert "0x107a1, 0xff_0000)" not in program
# A request also has to turn the enable register on. generateCLRegisterListG16Common
# prepends 0x17d9 = 1 as the list's FIRST entry whenever
# AGXCLCommandDescriptor+0x5c8 is set (mac17p 0xfffffe0008a5fa90 branch,
# 0xfffffe0008a5faa0 selector, 0xfffffe0008a5fab0 value, 0xfffffe0008a5fac4
# cursor); the clear arm at 0xfffffe0008a5facc leaves the cursor at 0.
assert "ComputeRegister::new(0x17d9, 1)" in program
assert "if scs.enabled {" in program
# Both 0xa599 emissions read one accelerator constant in the kext
# (0xfffffe0008a6030c via CL+0x548, 0xfffffe0008a60b4c straight from
# accel[0x10790] ?: accel[0x10778]), so they can never disagree.
assert program.count("ComputeRegister::new(0x0a599, 0x0000_0060_0040_0020)") == 2
assert "0x0000_0132_0040_0020" not in program

# The qualified path keeps its zero scratch request. The pure encoder below
# still verifies nonzero requests without reintroducing runtime ABI switches.
config = (root / "g17_config.rs").read_text()
for name in ("COMPUTE_SCS_PAGES", "COMPUTE_SCS_MIB", "COMPUTE_SCS_EXTRAS", "COMPUTE_SCS_WINDOW"):
    assert f"const {name}: u32 = 0;" in config
assert "default: 0," in block(params, "        g17p_compute_usage_slot: u32 {")

# Production requests are command-scoped: the extensible compute payload
# carries the packed layout and the modern G17 UAPI path derives registers from
# that field. This must not regress to one global diagnostic choice.
uapi = (root / "g17_uapi.rs").read_text()
assert "pub(crate) const UAPI_COMPUTE_SIZE: usize = 72;" in uapi
assert "scs_layout: get_u64(&raw, 0x40)" in uapi
assert "build_compute_registers_for_layout(" in uapi
assert "command.scs_layout," in uapi
assert "pub(crate) const fn scs_enabled(&self) -> bool" in uapi
assert "self.scs_layout >> 32 != 0" in uapi
assert "descriptor_flag_5c8: self.scs_enabled()" in uapi
assert "pub(crate) descriptor_flag_5c8: bool" not in block(
    uapi, "pub(crate) struct ComputeEntryContext<'a> {"
)

# Native passes descriptor +0x18's first boundary into the primary RCE binding.
# The descriptor and authored SKSM entry must consume the same derived value;
# fixed 0x22 would recreate the four-entry skew.
submission = (root / "g17_submission.rs").read_text()
command_state = block(submission, "pub(crate) fn prepare_g17p_compute_cl_command_state(")
assert "primary_checkpoint: u8" in command_state
assert "tag: primary_checkpoint" in command_state
manager = (root / "g17_manager.rs").read_text()
assert manager.count("u8::try_from(resume_checkpoints[0])") == 3
assert manager.count("descriptor_flag_5c8: command.scs_enabled()") == 2
assert "context.descriptor_flag_5c8" not in manager

# Compile the actual complete module, including its historical 40-register
# hash test. Stub only module-parameter access, using the real defaults.
declarations = []
for name in sorted(set(re.findall(r"crate::module_parameters::(\w+)\.value", compute))):
    declaration = block(params, f"        {name}: u32 {{")
    default = re.search(r"default:\s*(0x[0-9a-fA-F_]+|[0-9_]+)\s*,", declaration)
    assert default, f"cannot resolve the real default for {name}"
    declarations.append(f"pub static {name}: Param = Param({default.group(1)});")

rust = "#![allow(dead_code, non_upper_case_globals)]\n"
rust += "mod module_parameters {\npub struct Param(u32);\n"
rust += "impl Param { pub fn value(&self) -> &u32 { &self.0 } }\n"
rust += "\n".join(declarations) + "\n}\n"
rust += f"#[path = {json.dumps(str(root / 'g17_compute.rs'))}] mod g17_compute;\n"
rust += "use g17_compute::*;\n"
rust += r'''
#[test] fn an_empty_request_is_the_historical_register_program() {
 let scs = scs_request_registers(0, 0, 0, 0).unwrap();
 assert_eq!(scs.size_word, 0);
 assert_eq!(scs.extras, 0);
 assert_eq!(scs.window_word, 0xff_0000);
}
#[test] fn all_overrides_are_ignored_while_no_pages_are_requested() {
 assert_eq!(scs_request_registers(0, u32::MAX, u32::MAX, u32::MAX),
            scs_request_registers(0, 0, 0, 0));
}
#[test] fn a_request_packs_pages_and_mib_into_one_register() {
 // 89 pages is the four-section floor (0x58000) plus Apple's guard page, and
 // one MiB covers it: (89 * 4 + 0x3ff) >> 10 == 1.
 let scs = scs_request_registers(89, 1, 0, 0).unwrap();
 assert_eq!(scs.size_word, 0x0001_0059);
 assert_eq!(scs.window_word, 0x0100_0000);
}
#[test] fn extras_and_window_take_their_own_places() {
 let scs = scs_request_registers(815, 4, 2, 0x20).unwrap();
 assert_eq!(scs.size_word, 0x0004_032f);
 assert_eq!(scs.extras, 2);
 assert_eq!(scs.window_word, 0x0120_0000);
}
#[test] fn fields_cannot_overlap_or_silently_truncate() {
 for (pages, mib, window) in [(0x10000, 0, 0), (1, 0x10000, 0), (1, 0, 0x100)] {
  assert_eq!(scs_request_registers(pages, mib, 0, window),
             Err(ComputeBuildError::ScsRequestOutOfRange));
 }
 let max = scs_request_registers(0xffff, 0xffff, u32::MAX, 0xff).unwrap();
 assert_eq!(max.size_word, 0xffff_ffff);
 assert_eq!(max.extras, u32::MAX as u64);
 assert_eq!(max.window_word, 0x01ff_0000);
}
#[test] fn the_enable_register_leads_the_program_only_when_pages_are_requested() {
 let parameters = ComputeRegisterParameters {
  preempt_base: 0x100_0404_0000, cdm_base: 0x100_0410_0000,
  usc_exec_base: 0x1_0000_0000, helper_binary: 0, helper_data: 0,
  helper_config: 0, dispatch_identity: 1, context_id: 3, work_ordinal: 7,
  robustness: 0x100_0430_0000, operand_state_base: 0x70_0022_8000,
  usc_flist_id: 2,
 };
 let off = build_compute_registers(parameters).unwrap();
 assert_eq!(off.len(), COMPUTE_REGISTER_COUNT);
 assert!(off.as_slice().iter().all(|r| r.number != 0x17d9));
 let on = build_compute_registers_with_scs(
  parameters, scs_request_registers(89, 1, 0, 0).unwrap()).unwrap();
 assert_eq!(on.len(), COMPUTE_REGISTER_MAX);
 assert_eq!(on.as_slice()[0].number, 0x17d9);
 assert_eq!(on.as_slice()[0].value, 1);
 // Everything else keeps its order; only the three request registers move.
 let names: Vec<u32> = on.as_slice()[1..].iter().map(|r| r.number).collect();
 let baseline: Vec<u32> = off.as_slice().iter().map(|r| r.number).collect();
 assert_eq!(names, baseline);
}
#[test] fn derived_mib_rounds_up_at_every_page_boundary() {
 for pages in 1..=0xffff {
  let scs = scs_request_registers(pages, 0, 0, 0).unwrap();
  assert_eq!(scs.size_word & 0xffff, pages as u64);
  assert_eq!(scs.size_word >> 16, ((pages as u64 * 4096) + 0xfffff) >> 20);
 }
}
#[test] fn command_layout_drives_the_production_request() {
 let parameters = ComputeRegisterParameters {
  preempt_base: 0x100_0404_0000, cdm_base: 0x100_0410_0000,
  usc_exec_base: 0x1_0000_0000, helper_binary: 0, helper_data: 0,
  helper_config: 0, dispatch_identity: 1, context_id: 3, work_ordinal: 7,
  robustness: 0x100_0430_0000, operand_state_base: 0x70_0022_8000,
  usc_flist_id: 2,
 };
 let layout = 0x70u64 | (96u64 << 24) | (0x48u64 << 48);
 let on = build_compute_registers_for_layout(parameters, layout).unwrap();
 assert_eq!(on.as_slice()[0].number, 0x17d9);
 assert_eq!(on.as_slice().iter().find(|r| r.number == 0x101d1).unwrap().value,
            0x0004_032f);
 let off = build_compute_registers_for_layout(parameters, 0x70 | (96 << 24)).unwrap();
 assert_eq!(off.len(), COMPUTE_REGISTER_COUNT);
}
'''

with tempfile.TemporaryDirectory() as d:
    source = Path(d) / "test.rs"
    source.write_text(with_g17_config(rust))
    exe = Path(d) / "test"
    subprocess.run(["rustc", "--edition=2021", "--test", str(source), "-o", str(exe)], check=True)
    subprocess.run([str(exe), "--test-threads=1"], check=True)

print("PASS the SCS scratch request reaches 0x101d1/0xd479/0x107a1 and defaults to off")
