#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Test M5 register/descriptor encoding with Neo diagnostic overrides off/on."""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = r'''
#![allow(dead_code, non_upper_case_globals)]
mod g17_config {
    pub const TA_ORACLE: u32 = ORACLE_MASK;
    pub const FRAG_ORACLE: u32 = ORACLE_MASK;
    pub const RENDER_TA_QID_FIELD: u32 = 1;
}
mod g17_submission {
    pub const G17P_NATIVE_RENDER_DOORBELL_PRIORITY: u32 = 0;
}
'''
with tempfile.TemporaryDirectory(prefix='m5-render-') as tmp:
    test = Path(tmp) / 'test.rs'
    modules = ''.join(
        '#[path = "' + str(root / (module + '.rs')) + '"] mod ' + module + ';\n'
        for module in ('g17_render', 'm5_render'))
    exe = Path(tmp) / 'test'
    for mask in (0, 255):
        test.write_text(source.replace('ORACLE_MASK', str(mask)) + modules)
        subprocess.run(['rustc', '-Awarnings', '--edition=2021', '--test', str(test),
                        '-o', str(exe)], check=True)
        listing = subprocess.check_output([str(exe), '--list'], text=True)
        for required in ('native_m5_register_lists_match_every_write',
                         'native_m5_partial_programs_match_every_write',
                         'descriptor_programs_counts_aliases_and_bounds_match_native_layout'):
            assert 'm5_render::tests::' + required + ': test' in listing
        print('Neo oracle mask:', mask, flush=True)
        subprocess.run([str(exe), 'm5_render::'], check=True)
