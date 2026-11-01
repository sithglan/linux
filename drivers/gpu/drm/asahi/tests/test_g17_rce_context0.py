#!/usr/bin/env python3
"""RCE bindings must resolve through each render job's own command GART."""
from pathlib import Path
from g17_config_test_support import with_g17_config
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
resources = (root / "g17_resources.rs").read_text()
submission = (root / "g17_submission.rs").read_text()
manager = (root / "g17_manager.rs").read_text()


def function(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


getter = function(resources, "pub(crate) fn descriptor_low_va(")
audit = function(resources, "pub(crate) fn audit_fragment_rce_mapping(")
encoder = function(submission, "pub(crate) fn prepare_g17p_render_cl_command_state(")
mcache_encoder = function(submission, "fn encode_g17p_cl_mcache(")

assert "self.descriptor_3d_low" in getter
assert "descriptor_3d_render" not in getter
assert ".audit_fragment_rce_mapping(dev, vm)" in manager
assert "let base = self.descriptor_low_va(false)?;" in audit
assert "rce_vm.read_bytes_with_physical(address" in audit
assert "return Ok(())" not in audit
assert "install_t8140_job_context_aliases(vm)?" in resources
assert "install_t8140_native_context_aliases(vm)" not in function(
    manager, "fn begin_translated_render_with_hardware_buffer("
)

unit = r'''
#[derive(Debug)] enum SubmissionError { ComputeRceAddressOverflow, KickMcacheFieldOutOfRange, KickMcacheAddressUnencodable }
#[derive(Clone,Copy,Debug,PartialEq)] struct G17PClRceBinding { address: u64, tag: u8 }
struct G17PComputeClCommandState { event_mask: [u64;4], rce_bindings: [G17PClRceBinding;4] }
struct G17PClMcacheAperture { address: u64, index: u8, count: u8 }
const G17P_CL_PACKED_ADDRESS_MASK: u64 = (1u64 << 38) - 1;
mod module_parameters {
    pub struct Param(u32);
    impl Param { pub fn value(&self) -> &u32 { &self.0 } }
    pub static g17p_render_3d_rce: Param = Param(4);
}
#[test]
fn job_low_descriptor_drives_all_four_rce_bindings() {
    let command = prepare_g17p_render_cl_command_state(0x7000098000, false).unwrap();
    assert_eq!(command.rce_bindings.map(|b| b.address),
               [0x70000980a0, 0x70000987c0, 0x7000098ee0, 0x7000099600]);
}
#[test]
fn mcache_pointer_uses_the_same_job_low_descriptor() {
    let aperture = G17PClMcacheAperture { address: 0x7000098000 + 0x6a0, index: 0, count: 2 };
    assert_eq!(encode_g17p_cl_mcache(aperture).unwrap(), 0x004000e000130d40);
}
'''

with tempfile.TemporaryDirectory(prefix="g17-rce-job-root-") as tmp:
    test = Path(tmp) / "test.rs"
    test.write_text(with_g17_config(unit + encoder + mcache_encoder))
    exe = Path(tmp) / "test"
    subprocess.run(["rustc", "-Awarnings", "--edition=2021", "--test", str(test), "-o", str(exe)], check=True)
    subprocess.run([str(exe)], check=True)

print("RCE job-root source integration checks passed")
