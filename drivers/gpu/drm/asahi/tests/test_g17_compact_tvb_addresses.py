#!/usr/bin/env python3
"""Run the real PM encoder on allocator-selected, noncontiguous TVB addresses."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / "g17_resources.rs").read_text()


def item(marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


functions = "\n".join(item(marker) for marker in [
    "fn g17p_compact_tvb_page_id(", "fn g17p_pm_page_align(", "fn page_align(",
    "fn g17p_parameter_management_layout(", "fn encode_g17p_parameter_management(",
    "fn put_u32(", "fn put_u64(",
])
constants = {}
pending = set(re.findall(r"\bG17P_[A-Z0-9_]+\b", functions))
while pending:
    name = pending.pop()
    match = re.search(r"(?:pub\(crate\) )?const " + name + r": [^;]+;", source)
    assert match, name
    constants[name] = match.group()
    pending |= set(re.findall(r"\bG17P_[A-Z0-9_]+\b", match.group())) - constants.keys()

body = "\n".join([
    "#![allow(dead_code)]",
    "type Result<T=()> = core::result::Result<T, i32>;",
    "const EINVAL:i32=-22; const EOVERFLOW:i32=-75; const ERANGE:i32=-34;",
    *constants.values(),
    "#[derive(Default)]", item("struct G17PRenderStateLayout {"),
    item("struct G17PParameterManagementLayout {"), functions,
    r'''
fn read32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at+4].try_into().unwrap())
}
fn encode(addresses: &[u64], raw: &mut [u8]) -> Result {
    let layout = g17p_parameter_management_layout().unwrap();
    let render = G17PRenderStateLayout { tvb_blocks: 32, ..Default::default() };
    encode_g17p_parameter_management(
        raw, 0xffff_fc20_c091_0000, addresses,
        [0xffff_fc20_c097_8000, 0x10_0100_0000],
        [0x70_0200_0000, 0xffff_fc20_016d_0000],
        addresses.first().copied().unwrap_or(0),
        0x10_0300_0000, 0x10_0300_8000, 5, 7, &render, &layout,
    )
}
#[test] fn native_page_list_uses_actual_scattered_allocations() {
    let layout = g17p_parameter_management_layout().unwrap();
    let addresses: Vec<u64> = (0..32).map(|index|
        G17P_RENDER_USER_BASE + 0x0400_0000 + ((index * 13) % 32) * 0x80000
    ).collect();
    let mut raw = vec![0xa5; layout.total_size];
    encode(&addresses, &mut raw).unwrap();
    for (index, address) in addresses.iter().enumerate() {
        let first = ((address - G17P_RENDER_USER_BASE) / 0x8000) as u32;
        assert_eq!(read32(&raw, layout.block_page_bases + index * 8), first);
        for page in 0..4 {
            assert_eq!(read32(&raw, layout.page_list + (index*4 + page)*4), first + page as u32);
        }
    }
}
#[test] fn invalid_address_tables_are_rejected_before_any_pm_write() {
    let layout = g17p_parameter_management_layout().unwrap();
    let addresses: Vec<u64> = (0..32).map(|index|
        G17P_RENDER_USER_BASE + 0x0400_0000 + index * 0x80000
    ).collect();
    for bad in [0, G17P_RENDER_USER_BASE - 0x8000,
        G17P_RENDER_USER_BASE + 1, G17P_RENDER_USER_END - 0x8000, u64::MAX] {
        let mut input = addresses.clone();
        input[31] = bad;
        let mut raw = vec![0xa5; layout.total_size];
        assert!(encode(&input, &mut raw).is_err());
        assert!(raw.iter().all(|byte| *byte == 0xa5));
    }
    for count in [0, 1, 31] {
        let mut raw = vec![0xa5; layout.total_size];
        assert!(encode(&addresses[..count], &mut raw).is_err());
        assert!(raw.iter().all(|byte| *byte == 0xa5));
    }
}
''',
])
with tempfile.TemporaryDirectory(prefix="g17-compact-tvb-") as tmp:
    src = Path(tmp) / "test.rs"
    binary = Path(tmp) / "test"
    src.write_text(body)
    subprocess.run(["rustc", "--edition=2021", "--test", str(src), "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
