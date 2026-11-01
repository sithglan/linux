#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Run the production M5 depth guard without the kernel manager test graph.

The guard, range checker, field types and test assertions are extracted intact.
Only the unrelated render-parser fixture setup and error enum are narrowed.
This verifies admission, not GPU execution or the rest of the UAPI parser.
"""
from pathlib import Path
import re
import subprocess
import tempfile

source = (Path(__file__).resolve().parent.parent / 'g17_uapi.rs').read_text()


def block(pattern):
    match = re.search(pattern, source, re.M)
    assert match, pattern
    start = source.index('{', match.start())
    cursor, depth = start + 1, 1
    while depth:
        depth += (source[cursor] == '{') - (source[cursor] == '}')
        cursor += 1
    return source[match.start():cursor]


types = ['UapiHelperProgram', 'UapiProgram', 'UapiZlsBuffer', 'UapiTimestamp',
         'UapiTimestamps', 'UapiRenderCommand']
declarations = ['#[derive(Default, Copy, Clone)]\n' + block(
    r'^pub\(crate\) struct ' + name + r' \{') for name in types]
test = block(r'^    fn m5_d32_envelope_rejects_wrong_layout_and_incomplete_backing\(')
start = test.index('        payload.width = 256;')
test = test[:test.index('{') + 1] + (
    '\n        let mut payload = UapiRenderCommand::default();\n'
    '        payload.layers = 1;\n') + test[start:]
constants = re.findall(
    r'^pub\(crate\) const (?:M5_MAX_RENDER_DIMENSION|PAGE_SIZE)[^;]*;', source, re.M)
assert constants, 'the guard\'s constants moved'

harness = '\n'.join([
    '#![allow(dead_code)]', *constants, *declarations,
    '#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n' +
    block(r'^pub\(crate\) enum GpuAccess \{'),
    block(r'^pub\(crate\) trait GpuAddressSpace \{'),
    '#[derive(Debug, PartialEq, Eq)] enum UapiTranslateError { ZlsFields, AddressRange }',
    block(r'^fn require_range<'), block(r'^pub\(crate\) fn validate_m5_reference_depth<'),
    'struct TestAddressSpace;', block(r'^    impl GpuAddressSpace for TestAddressSpace \{'),
    '#[test]\n' + test,
])
with tempfile.TemporaryDirectory(prefix='m5-depth-admission-') as temporary:
    root = Path(temporary)
    (root / 'test.rs').write_text(harness)
    subprocess.run(['rustc', '--edition=2021', '--test', str(root / 'test.rs'),
                    '-o', str(root / 'test')], check=True)
    subprocess.run([str(root / 'test'), '--nocapture'], check=True)
