#!/usr/bin/env python3
"""Check that every user descriptor route binds its trusted context epoch.

Behavior is exercised by test_g17_execution_contexts and the complete render
and compute module tests. These checks cover route selection, which those
standalone modules cannot execute without a live DRM device.
"""
from pathlib import Path

root = Path(__file__).resolve().parents[1]
manager = (root / 'g17_manager.rs').read_text()
resources = (root / 'g17_resources.rs').read_text()

def block(source, marker):
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]

for route in ['fn submit_translated_first_bind_channel(',
              'fn submit_translated_repeat_channel(',
              'pub(crate) fn submit_translated_compute(']:
    body = block(manager, route)
    build = body.index('g17_uapi::build_compute_descriptor(')
    epoch = body.index('g17_compute::set_context_generation(raw, binding.context_generation())')
    assert build < epoch, route
    assert body.index('let kick_timestamp = queue.producer.current_timestamp;') < build, route
    assert epoch < body.index('g17_compute::set_hardware_timestamp(raw, kick_timestamp)'), route

for route in ['pub(crate) fn begin_translated_render(',
              'pub(crate) fn warm_render_pair_storage(']:
    body = block(manager, route)
    assert 'command.parameters.context_generation = execution_context.generation();' in body, route

assert 'translated.parameters.context_generation = command_context.generation();' in resources
assert 'context_generation' not in (root / 'g17_uapi.rs').read_text(), 'UAPI must not supply the trusted epoch'
print('PASS trusted context epoch wiring: cold/warm render and all three compute publication routes')
