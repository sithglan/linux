#!/usr/bin/env python3
"""Verify every compute queue advertises its retained private firmware FList."""
from pathlib import Path
from g17_config_test_support import with_g17_config
import json
import re
import subprocess
import tempfile

src = Path(__file__).resolve().parent.parent
initdata = (src / "g17_initdata.rs").read_text()
submission = (src / "g17_submission.rs").read_text()
resources = (src / "g17_resources.rs").read_text()
manager = (src / "g17_manager.rs").read_text()


def block(source, marker):
    start = source.index(marker)
    end = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


def constant(source, name):
    return re.search(r"pub\(crate\) const " + name + r": .*?;", source, re.S).group()


stage = block(resources, "pub(crate) fn stage_compute_flist_state(")
assert stage.index("if self.resources.compute_flist_initialized") < stage.index("encode_compute_flist_page_list")
assert stage.index("encode_compute_flist_page_list") < stage.index("Ordering::SeqCst")
prepare = block(manager, "pub(crate) fn prepare_g17p_compute_queue(")
assert "G17PComputeFlist::new(dev, self.uat(), queue_id)" in prepare
assert "initialize_compute_pool_uma(&flist)" in prepare
runtime = block(resources, "pub(crate) struct G17PClRuntimeStorage")
assert "flist: Arc<G17PComputeFlist>" in runtime

code = "#![allow(dead_code)]\n" + "\n".join([
    "#[derive(Debug)] enum InitdataError { BufferSize }",
    "#[derive(Debug)] enum SubmissionError { KickRetainedStorageBufferTooSmall }",
    *[constant(initdata, name) for name in [
        "COMPUTE_FLIST_RELOCATION", "COMPUTE_FLIST_PAGE_LIST_ADDRESS",
        "COMPUTE_DISPATCH_RECORD_SIZE", "COMPUTE_FLIST_BUFFER_BASE",
        "CONTROL_OPERAND_BUFFER_BASE", "CONTROL_OPERAND_BUFFER_STRIDE",
        "CONTROL_OPERAND_BUFFER_SIZE", "COMPUTE_READINESS_OPERAND_ENTRY_COUNT"]],
    *[block(initdata, marker) for marker in [
        "fn put_u32(", "fn put_u64(",
        "pub(crate) fn encode_compute_flist_page_list(",
        "pub(crate) fn encode_compute_dispatch_record("]],
    *[block(submission, marker) for marker in [
        "fn put_u32_le(", "fn put_u64_le(",
        "pub(crate) struct G17PClStorageRange",
        "pub(crate) struct G17PClRetainedStorageLayout",
        "pub(crate) fn apply_g17p_cl_queue_static_state(",
        "pub(crate) fn apply_g17p_cl_channel_control_static_state(",
        "pub(crate) fn apply_g17p_cl_retained_static_state("]],
    constant(submission, "G17P_CL_RETAINED_STORAGE_LAYOUT"),
    "#[derive(Default)] " + block(submission, "pub(crate) struct G17PUmaPagePoolState"),
    block(submission, "pub(crate) const fn encode_g17p_umadesc("),
]) + r'''
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
#[test]
fn compute_descriptor_names_relocated_page_list() {
    let mut bytes = [0xa5; COMPUTE_DISPATCH_RECORD_SIZE];
    encode_compute_dispatch_record(&mut bytes).unwrap();
    let expected = encode_g17p_umadesc([0; 4], G17PUmaPagePoolState {
        page_list: COMPUTE_FLIST_PAGE_LIST_ADDRESS,
        unit_pages: 0x40000, counter_b: 0x1500, total_pages: 0x1500,
        ..Default::default()
    });
    for i in 0..4 { assert_eq!(u64_at(&bytes, i * 8), expected[i]); }
    assert_eq!((u64_at(&bytes, 0) & ((1 << 41) - 1)) << 7,
               COMPUTE_FLIST_PAGE_LIST_ADDRESS);
}
#[test]
fn queue_support_names_same_pool_as_compute_descriptor() {
    let mut shared = [0xa5; 0x4000];
    let mut support = [0xa5; 0x4000];
    let mut scheduler = [0xa5; 0x4000];
    let mut channel = [0xa5; 0x40];
    let operand_table = 0xffff_fc20_1234_0000;
    apply_g17p_cl_retained_static_state(
        &mut shared, &mut support, &mut scheduler, &mut channel,
        COMPUTE_FLIST_PAGE_LIST_ADDRESS, operand_table,
        0xffff_fc20_1235_0000, 0xffff_fc20_1236_0000,
    ).unwrap();
    assert_eq!(u64_at(&shared, 0x14), COMPUTE_FLIST_PAGE_LIST_ADDRESS);
    assert_eq!(u32_at(&shared, 0x08), 1);
    assert_eq!(u32_at(&shared, 0x1c), 0x40000);
    assert_eq!(u32_at(&shared, 0x20), 0);
    assert_eq!(u32_at(&shared, 0x24), 0x1500);
    assert_eq!(u32_at(&shared, 0x2c), 0x1500);
    assert_eq!(u64_at(&shared, 0x30), operand_table);
}
#[test]
fn every_advertised_page_is_initialized_in_the_compute_aperture() {
    let mut descriptor = [0; COMPUTE_DISPATCH_RECORD_SIZE];
    encode_compute_dispatch_record(&mut descriptor).unwrap();
    let count = u64_at(&descriptor, 0x10) as usize;
    assert_eq!(count, 21 * 256);
    let mut pages = vec![0xa5; 0x20_0000];
    encode_compute_flist_page_list(&mut pages).unwrap();
    for i in 0..count {
        let expected = 0x70_0222_0000 + (i / 256) as u64 * 0x10_8000
            + (i % 256) as u64 * 0x1000;
        assert_eq!(u64_at(&pages, i * 8), expected);
    }
    assert!(pages[count * 8..].iter().all(|b| *b == 0));
    assert!(encode_compute_flist_page_list(&mut pages[..count * 8 - 1]).is_err());
}
'''

code += '\n#[path = ' + json.dumps(str(src / "g17_compute.rs")) + '] mod g17_compute;\n'
code = with_g17_config(code)
code += r'''
#[test]
fn distinct_queues_name_their_private_flist_row() {
    use g17_compute::*;
    for hardware_buffer_id in [1u8, 2, 3, 31] {
        for first in [true, false] {
            let mut graph = [0; COMPUTE_QUEUE_GRAPH_SIZE];
            encode_compute_config_update(
                &mut graph,
                ComputeSksmEntryAliases::new(0x7004000000, 0xfffffc2000400000, 0x20000, 0x4000).unwrap(),
                0xfffffc2000010000, 0xfffffc2000014000, 6, hardware_buffer_id, first,
                ComputeConfigUpdates { qos: first, context: first }, Some(7),
            ).unwrap();
            let tag15 = &graph[COMPUTE_OPTIONAL..];
            assert_eq!(u16::from_le_bytes(tag15[0x46..0x48].try_into().unwrap()),
                       g17_config::compute_flist_id(6));
            assert_eq!(u16::from_le_bytes(tag15[0x56..0x58].try_into().unwrap()), u16::from(hardware_buffer_id));
        }
    }
}
'''

with tempfile.TemporaryDirectory(prefix="g17-compute-flist-") as tmp:
    source = Path(tmp) / "test.rs"
    binary = Path(tmp) / "test"
    source.write_text(code)
    subprocess.run(["rustc", "--edition=2021", "--test", str(source), "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
