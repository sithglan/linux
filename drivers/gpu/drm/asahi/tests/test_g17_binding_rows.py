#!/usr/bin/env python3
"""Execute production firmware-row bounds and replacement detection with mock reads."""
from pathlib import Path
import re
import subprocess
import tempfile
import sys
sys.dont_write_bytecode = True
from test_g17_compute_scs_encoding import block
root = Path(__file__).resolve().parents[1]
s = (root / 'g17_manager.rs').read_text()
constants = '\n'.join(re.findall(r'^const G17P_GFX_(?:LINK_DATA_BASE|LINK_DATA_END|PHYSICAL_DATA_BASE): u64 = .*?;', s, re.M))
functions = '\n'.join(block(s, marker) for marker in [
    'pub(crate) fn g17p_gfx_link_to_physical(',
    'fn g17p_binding_row_address(',
    'fn g17p_read_binding_row<const N: usize>(',
    'pub(crate) fn diagnostic_accounting_clocks(',
    'fn diagnostic_gupm_request(',
])
mock = r'''
type Result<T> = std::result::Result<T, i32>;
const EINVAL:i32 = 22; const EAGAIN:i32 = 11; const EOVERFLOW:i32 = 75;
mod pgtable {
    use std::sync::atomic::{AtomicUsize, Ordering};
    pub static READS: AtomicUsize = AtomicUsize::new(0);
    pub static CHANGE: AtomicUsize = AtomicUsize::new(usize::MAX);
    pub struct LiveFirmwareU64Probe(u64);
    pub struct Sample { pub first:u64 }
    impl LiveFirmwareU64Probe {
        pub fn new(address:u64)->super::Result<Self> { Ok(Self(address)) }
        pub fn observe_mask(&self,_mask:u64,_samples:u32)->super::Result<Sample> {
            let index = READS.fetch_add(1,Ordering::Relaxed);
            Ok(Sample{first:self.0 + u64::from(index >= CHANGE.load(Ordering::Relaxed))})
        }
    }
}
#[test]
fn actual_row_addresses_bounds_and_identity_races() {
    use std::sync::atomic::Ordering;
    assert_eq!(g17p_binding_row_address(0xfffffc000005b778,133,0xe8),Some(0xfffffc0000063000));
    assert_eq!(g17p_binding_row_address(0xfffffc000006bd18,2,0x50),Some(0xfffffc000006bdb8));
    assert_eq!(g17p_gfx_link_to_physical(0xfffffc0000063000),0x10001d93000);
    assert_eq!(g17p_gfx_link_to_physical(0xfffffc000006bdb8),0x10001d9bdb8);
    assert_eq!(g17p_binding_row_address(G17P_GFX_LINK_DATA_END-0x50,0,0x50),Some(G17P_GFX_LINK_DATA_END-0x50));
    for (base,id,stride) in [
        (G17P_GFX_LINK_DATA_BASE-8,0,0x50),
        (G17P_GFX_LINK_DATA_END-0x48,0,0x50),
        (u64::MAX-7,1,0x50),
        (G17P_GFX_LINK_DATA_BASE,0,0),
        (G17P_GFX_LINK_DATA_BASE+1,0,0x50),
        (G17P_GFX_LINK_DATA_BASE,u32::MAX,0xe8),
        (G17P_GFX_LINK_DATA_BASE,0,7),
    ] { assert_eq!(g17p_binding_row_address(base,id,stride),None); }
    assert_eq!(g17p_read_binding_row::<29>(0xfffffc000005b778,133,0x50),Err(EINVAL));
    assert_eq!(pgtable::READS.load(Ordering::Relaxed),0);
    let row = g17p_read_binding_row::<29>(0xfffffc000005b778,133,0xe8).unwrap();
    assert_eq!(row[0],0x10001d93000);
    assert_eq!(row[28],0x10001d930e0);
    assert_eq!(pgtable::READS.load(Ordering::Relaxed),32);
    pgtable::READS.store(0,Ordering::Relaxed);
    pgtable::CHANGE.store(29,Ordering::Relaxed);
    assert_eq!(g17p_read_binding_row::<29>(0xfffffc000005b778,133,0xe8),Err(EAGAIN));
    pgtable::READS.store(0,Ordering::Relaxed);
    pgtable::CHANGE.store(usize::MAX,Ordering::Relaxed);
    assert_eq!(diagnostic_accounting_clocks().unwrap(), [
        0x10001e54ad8,0x10001e54af8,0x10001e54b18,0x10001e54b38,
        0x10001d9e598,0x10001d9e5b0,0x10001d9e5d0,0x10001ea9320]);
    assert_eq!(pgtable::READS.load(Ordering::Relaxed),12);
    pgtable::READS.store(0,Ordering::Relaxed);
    pgtable::CHANGE.store(8,Ordering::Relaxed);
    assert_eq!(diagnostic_accounting_clocks(),Err(EAGAIN));

    pgtable::READS.store(0,Ordering::Relaxed);
    pgtable::CHANGE.store(usize::MAX,Ordering::Relaxed);
    assert_eq!(diagnostic_gupm_request().unwrap(), [
        0x10001ea9598,0x10001ea95a0,0x10001ea95a8,0x10001ea95b0,
        0x10001ea95b8,0x10001ea95c0,0x10001ea95c8,0x10001ea95d0]);
    assert_eq!(pgtable::READS.load(Ordering::Relaxed),22);
    pgtable::READS.store(0,Ordering::Relaxed);
    pgtable::CHANGE.store(11,Ordering::Relaxed);
    assert_eq!(diagnostic_gupm_request(),Err(EAGAIN));

}
'''
with tempfile.TemporaryDirectory() as tmp:
    exe = str(Path(tmp) / 'test')
    subprocess.run(['rustc', '--edition=2021', '--test', '-', '-o', exe], input=constants+functions+mock, text=True, check=True)
    subprocess.run([exe], check=True)
