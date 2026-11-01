#!/usr/bin/env python3
"""Compile both profiles and check priority propagation without changing IDs."""
from pathlib import Path
import subprocess
import tempfile
import test_g17_compute_scs_encoding as codec

HARNESS = r'''
use g17_submission::*;
fn main() {
    let priority = if cfg!(g17p_render_priority_probe) { 2 } else { 0 };
    let subpriority: u16 = if priority == 2 { 2 } else { 1 };
    assert_eq!(G17P_NATIVE_RENDER_DOORBELL_PRIORITY, priority);
    assert_eq!(G17P_NATIVE_3D_DOORBELL, encode_tx_doorbell(PipeType::Fragment, priority).unwrap());
    assert_eq!(G17P_NATIVE_TA_DOORBELL, encode_tx_doorbell(PipeType::Vertex, priority).unwrap());
    assert_eq!(G17P_PARTIAL_OPENING_TILING_CHANNEL_TABLE_INDEX, priority * 3);
    assert_eq!(G17P_PARTIAL_OPENING_FRAGMENT_CHANNEL_TABLE_INDEX, priority * 3 + 1);
    let addresses = G17PColdOpeningOptionalAddresses {
        context_scratch: 0x10000, firmware_scratch: 0x20000,
        usc_priv_mem_flist: 0x30000, usc_freelist_hardware_buffer_id: 131,
        channel_control: 0x40000, tiling_shared_object: 0x50000,
        parameter_buffer_token: 0x60000, owner_pid: 1995,
        ta_hardware_buffer_id: 24,
    };
    for id in [0, 5, 6, 31] {
        for stage in [G17PColdOpeningStage::Tiling, G17PColdOpeningStage::Fragment] {
            let mut raw = [0; G17P_COLD_OPENING_OPTIONAL_SIZE];
            apply_g17p_cold_opening_optional(stage, addresses, 9, 10, id, 1, 8, &mut raw).unwrap();
            let word = |at| u16::from_le_bytes([raw[at], raw[at+1]]);
            assert_eq!(word(0x1e), u16::from(priority));
            assert_eq!(word(0x5e), subpriority);
            assert_eq!(word(0x56), id);
            assert_eq!(word(0x46), 131);
            assert_eq!(u32::from_le_bytes(raw[0x5a..0x5e].try_into().unwrap()), 1995);
        }
    }
    println!("PASS render profile priority={priority}: paired tags, rings, doorbells and independent scheduler IDs");
}
'''
with tempfile.TemporaryDirectory() as directory:
    src = Path(directory) / 'check.rs'
    src.write_text(codec.rust_source().removesuffix(codec.HARNESS) + HARNESS)
    for probe in (False, True):
        exe = Path(directory) / ('probe' if probe else 'default')
        flags = ['--cfg', 'g17p_render_priority_probe'] if probe else []
        subprocess.run(['rustc', '--edition=2021', *flags, str(src), '-o', str(exe)], check=True)
        subprocess.run([str(exe)], check=True)
