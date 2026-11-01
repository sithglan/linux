#!/usr/bin/env python3
"""Compile actual render FList lease/staging methods with mapping mocks.

Wire encoders and lifecycle are real source. Pool initializer bodies are stubbed
only in the mapping-mock test: its assertion is that warm paths invoke none and
preserve all owned bytes, not that those stubs model firmware growth.
"""
from pathlib import Path
import re
import subprocess
import tempfile

src = Path(__file__).resolve().parent.parent
manager = (src / "g17_manager.rs").read_text()
resources = (src / "g17_resources.rs").read_text()
submission = (src / "g17_submission.rs").read_text()
initdata = (src / "g17_initdata.rs").read_text()
runtime = (src / "g17_live_boot.rs").read_text()


def block(source, pattern):
    match = re.search(pattern, source, re.M)
    assert match, pattern
    at = source.index("{", match.end())
    depth, end = 1, at + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[match.start():end]


def constant(source, name):
    return re.search(r"(?:pub\(crate\) )?const " + name + r": .*?;", source, re.S).group()


derive = "#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n"
submission_part = "\n".join([
    "#[derive(Debug)] pub(crate) enum SubmissionError { PartialOpeningGraphObjectSize, PartialOpeningAddressOverflow }",
    *[constant(submission, n) for n in [
        "G17P_USC_FREELIST_LOW_VA", "G17P_USC_FREELIST_LOW_END",
        "G17P_USC_FREELIST_GROW_DESCRIPTOR_BYTES", "G17P_DEVCTL_USC_FREELIST"]],
    block(submission, r"^pub\(crate\) const fn g17p_render_flist_generation\("),
    derive + block(submission, r"^pub\(crate\) struct G17PUscFreelistSequence\s*"),
    block(submission, r"^pub\(crate\) fn encode_g17p_usc_freelist_control\("),
    block(submission, r"^fn put_u32_le\("),
    block(submission, r"^fn put_u64_le\("),
])
initdata_part = "\n".join([
    "#[derive(Debug)] pub(crate) enum InitdataError { BufferSize }",
    *[constant(initdata, n) for n in [
        "CONTROL_SHARED_ADDRESS", "CONTROL_OPERAND_TABLE_ADDRESS",
        "PARTIAL_OPENING_FREELIST_HARDWARE_BUFFER_ID", "CONTROL_SHARED_INNER_PRESENTED",
        "CONTROL_RECORD_SIZE", "CONTROL_BOOTSTRAP_UMA_RELEASE_OPCODE",
        "CONTROL_RING_ENTRIES",
        "CONTROL_RING_INDEX_MASK"]],
    block(initdata, r"^pub\(crate\) fn encode_bootstrap_uma_release_record\("),
    block(initdata, r"^fn put_u32\("), block(initdata, r"^fn put_u64\("),
    # Used solely by the actual staging method tests below.
    "pub fn encode_render_control_shared(b: &mut [u8]) -> Result<(), ()> { b.fill(0x12); Ok(()) }",
    "pub fn encode_compute_partial_opening_control_shared(b: &mut [u8]) -> Result<(), ()> { b.fill(0x21); Ok(()) }",
])
lease_part = "\n".join([
    *[constant(manager, n) for n in [
        "G17P_BOOTSTRAP_UMA_SEQUENCE", "G17P_BOOTSTRAP_UMA_HARDWARE_BUFFER_ID",
        "G17P_BOOTSTRAP_UMA_STAMP", "G17P_BOOTSTRAP_UMA_COOKIE", "G17P_BOOTSTRAP_UMA_COUNT"]],
    *[derive + block(manager, r"^(?:pub\(crate\) )?(?:struct|enum) " + n + r"\s*(?=\{)") for n in [
        "G17PBootstrapUmaCompletion", "G17PBootstrapUmaRelease", "G17PBootstrapUmaRequestState",
        "G17PBootstrapUmaLifecycleError", "G17PBootstrapUmaLifecycle"]],
    block(manager, r"^fn g17p_control_ring_target_consumed\("),
    block(manager, r"^impl G17PBootstrapUmaLifecycle\s*"),
    *["#[test]\n" + block(manager, r"^    fn " + n + r"\(\)\s*") for n in [
        "bootstrap_async_flist_releases_only_after_type13_and_paired_render",
        "bootstrap_async_flist_rejects_raw_or_duplicate_type13_records",
        "retained_flist_leases_grow_once_and_release_every_captured_generation",
        "private_render_flist_owner_grows_once_then_reacquires_warm",
        "private_render_flist_replacement_waits_for_exact_old_owner_retirement",
        "private_render_flist_same_owner_overlap_keeps_identity_until_last_pair"]],
])
pool_methods = "\n".join(block(resources, r"^    pub\(crate\) fn " + n + r"\(") for n in [
    "stage_render_flist_state", "render_flist_owned"])
pool_part = r"""
type Result<T = ()> = std::result::Result<T, i32>;
const EINVAL: i32 = 22;
#[derive(Clone, Debug, PartialEq, Eq)] struct Bytes(Vec<u8>);
impl Bytes {
    fn with_bytes_mut<T>(&mut self, f: impl FnOnce(&mut [u8]) -> Result<T>) -> Result<T> {
        f(&mut self.0)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)] struct Backings {
    control_operand_page_lists: Bytes, control_operand_table: Bytes,
    control_shared: Bytes, control_shared_inner: Bytes,
    uma_page_pool_descriptor_table: Bytes,
}
fn encode_g17p_initial_flist_page_list(b: &mut [u8]) -> bool { b.fill(0x17); true }
fn encode_g17p_pre_qid_flist_runs(b: &mut [u8]) -> bool { b.fill(0x22); true }
fn encode_g17p_compute_flist_runs(b: &mut [u8]) -> bool { b.fill(0x21); true }
struct Owner { resources: Backings, flist_backing_owner: G17PFListBackingOwner }
""" + derive + block(resources, r"^enum G17PFListBackingOwner\s*") + r"""
impl Owner {
fn stage_render_uma_page_pool_descriptor(&mut self) -> Result {
    self.resources.uma_page_pool_descriptor_table.0.fill(0x33);
    Ok(())
}
""" + pool_methods + r"""
fn alias_creation(&mut self, bootstrap_vdm: bool) -> Result {
let _ = bootstrap_vdm;
Ok(())
}
}
""" + r"""
#[test]
fn actual_staging_preserves_all_grown_backings_and_other_vm_creation() {
    let b = Bytes(vec![0xa5; 0x4000]);
    let mut owner = Owner {
        resources: Backings { control_operand_page_lists: b.clone(),
            control_operand_table: b.clone(), control_shared: b.clone(),
            control_shared_inner: b.clone(), uma_page_pool_descriptor_table: b },
        flist_backing_owner: G17PFListBackingOwner::Opening,
    };
    owner.stage_render_flist_state().unwrap();
    let cold = owner.resources.clone();
    // Model positive firmware updates throughout every owned object, including
    // the grown page-list tail and shared counters, not merely their first page.
    for (index, b) in [&mut owner.resources.control_operand_page_lists,
        &mut owner.resources.control_operand_table, &mut owner.resources.control_shared,
        &mut owner.resources.control_shared_inner].into_iter().enumerate() {
        for (offset, byte) in b.0.iter_mut().enumerate() {
            *byte = (offset.wrapping_mul(17) + index * 37) as u8;
        }
    }
    let grown = owner.resources.clone();
    for _ in 0..3 {
        owner.stage_render_flist_state().unwrap();
        assert_eq!(owner.resources, grown);
        owner.alias_creation(false).unwrap();
        owner.alias_creation(true).unwrap();
        assert_eq!(owner.resources, grown);
    }
    // This state transition is made only by the real stopped-processor reset.
    owner.flist_backing_owner = G17PFListBackingOwner::Opening;
    owner.stage_render_flist_state().unwrap();
    assert_eq!(owner.resources, cold);
}

#[test]
fn actual_cold_wire_and_three_release_wires_are_exact() {
    let lease = G17PBootstrapUmaLifecycle::new();
    let mut raw = [0xa5; 64];
    g17_submission::encode_g17p_usc_freelist_control(
        g17_initdata::CONTROL_SHARED_ADDRESS,
        g17_submission::G17P_USC_FREELIST_LOW_VA,
        g17_submission::G17P_USC_FREELIST_LOW_END,
        g17_submission::G17PUscFreelistSequence {
            word_04: 1, word_0c: lease.sequence as u32, word_30: lease.hardware_buffer_id,
        }, &mut raw).unwrap();
    assert_eq!(raw, [
        0x20,0,0,0, 1,0,0,0, 0x3f,0,0,0, 0,0,0,0,
        0,0,0,0, 0,0,0x83,0xc0, 0x20,0xfc,0xff,0xff, 0,0x80,0x20,0,
        0x70,0,0,0, 0x40,0x84,0x20,0, 0x70,0,0,0, 0x28,0,0,0,
        0,0,0,0, 1,0,0,0, 0,0,0,0, 0,0,0,0,
    ]);
    for sequence in 0..3u64 {
        g17_initdata::encode_bootstrap_uma_release_record(sequence, 0, &mut raw).unwrap();
        let mut expected = [0u8; 64];
        expected[0] = 0x2e;
        expected[4] = sequence as u8;
        assert_eq!(raw, expected);
    }
}
"""
body = "#![allow(dead_code)]\nmod g17_submission {" + submission_part + "}\nmod g17_initdata {" + initdata_part + "}\n" + lease_part + "\n" + pool_part
with tempfile.TemporaryDirectory(prefix="g17-retained-flist-") as temp:
    binary = Path(temp) / "tests"
    subprocess.run(["rustc", "--edition=2021", "--test", "-", "-o", str(binary)], input=body, text=True, check=True)
    subprocess.run([str(binary)], check=True)

# Production caller/ordering checks accompany, but do not replace, real methods.
begin = block(manager, r"^    fn begin_translated_render_with_hardware_buffer\(")
assert "G17PBootstrapUmaLifecycle::acquire_owner" in begin
assert ".stage_render_flist_state()" not in begin
assert "storage.render_flist_id()" in begin
assert "storage.render_flist_pointer()" in begin
assert "storage.render_flist_operand_table()" in begin
assert "storage.render_flist_cookie()" in begin
assert "g17_submission::g17p_render_flist_generation(slot.submission_ordinal)" in begin
publish = block(manager, r"^    pub\(crate\) fn publish_render_usc_freelist_request\(")
assert "self.render_slots.get(pair_slot).bootstrap_uma.is_some()" not in publish
assert "self.render_slots.get(pair_slot).prepared_render_flist.ok_or(EIO)?" in publish
assert publish.index("lifecycle.request.is_none()") < publish.index("encode_g17p_usc_freelist_control")
for identity in ["lifecycle.shared_control", "lifecycle.operand_table", "lifecycle.cookie"]:
    assert identity in publish
alias = block(resources, r"^    pub\(crate\) fn install_user_vm_aliases\(")
assert "with_bytes_mut" not in alias
assert "stage_render_flist_state" not in alias
assert "stage_compute_flist_state" not in alias
reset = block(resources, r"^    pub\(crate\) fn reset_session_after_processor_stop\(")
assert "self.flist_backing_owner = G17PFListBackingOwner::Opening" in reset
compute = block(runtime, r"^    pub\(crate\) fn submit_translated_compute\(")
assert compute.index(".stage_compute_flist_state()?") < compute.index("self.ensure_compute_queue()?")
print("actual FList lifecycle, wire, retained mapping bytes and publication/handoff checks PASS")
