#!/usr/bin/env python3
"""Execute production UAPI translation and phase-specific ZLS register tests."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]

def item(source, marker):
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]


manager = (root / 'g17_manager.rs').read_text()
geometry = '#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n'
geometry += item(manager, 'pub(crate) struct G17SksmScratchGeometry')
geometry += '\n#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n'
geometry += item(manager, 'pub(crate) struct G17SksmScratchOffsets')
geometry += '\n#[derive(Debug)]\n' + item(manager, 'pub(crate) enum ConstructionError')
geometry += '\n' + item(manager, 'impl G17SksmScratchGeometry')
source = '#![allow(dead_code)]\nmod g17_manager {\n' + geometry + '\n}\n'
names = (
    'g17_uapi::tests::partial_zls_preserves_depth_and_stencil_across_internal_splits',
    'g17_render::tests::partial_render_uses_phase_specific_zls_control',
)
with tempfile.TemporaryDirectory(prefix='g17-partial-zls-') as temporary:
    directory = Path(temporary)
    # Keep the production pure modules intact; replace only their standalone
    # test imports so they do not pull the kernel-only manager into rustc.
    for module in ('g17_config', 'g17_render', 'g17_compute', 'g17_submission', 'g17_uapi'):
        text = (root / (module + '.rs')).read_text()
        if module == 'g17_submission':
            start = text.index('#[cfg(not(test))]\nuse crate::g17_manager::')
            end = text.index('use g17_manager::G17SksmScratchGeometry;', start)
            end += len('use g17_manager::G17SksmScratchGeometry;')
            text = text[:start] + 'use crate::g17_manager::G17SksmScratchGeometry;' + text[end:]
            text = text.replace('#[cfg(test)]\n' + item(text, 'mod tests {'), '')
        if module == 'g17_uapi':
            start = text.index('#[cfg(not(test))]\nuse crate::{g17_compute')
            end = text.index('\nuse g17_render::', start)
            text = text[:start] + 'use crate::{g17_compute, g17_render, g17_submission};\n' + text[end:]
            # Select the exact UAPI regression plus its real fixture/helpers.
            # Unrelated historical unit tests use obsolete producer structs.
            tests = item(text, 'mod tests {')
            focused = tests[:tests.index('    #[test]')]
            focused += '#[test]\n' + item(tests, 'fn partial_zls_preserves_depth_and_stencil_across_internal_splits(') + '\n}'
            text = text.replace(tests, focused)
        text = re.sub(r'(include_(?:bytes|str)!\(")([^"]+)("\))',
                      lambda m: m[1] + str(root / m[2]) + m[3], text)
        (directory / (module + '.rs')).write_text(text)
        source += f'mod {module};\n'
    test = Path(temporary) / 'test.rs'
    binary = Path(temporary) / 'test'
    test.write_text(source)
    subprocess.run(['rustc', '--edition=2021', '--test', '-Awarnings', str(test),
                    '-o', str(binary)], check=True)
    listing = subprocess.check_output([str(binary), '--list'], text=True)
    for name in names:
        assert name + ': test' in listing
        subprocess.run([str(binary), name, '--exact'], check=True)
