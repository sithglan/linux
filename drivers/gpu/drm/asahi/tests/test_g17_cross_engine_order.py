#!/usr/bin/env python3
"""Check explicit-prefix integration with the production DRM entity FIFO.

Behavioral native-token/prefix tests live in test_g17_native_cross_engine.py.
The previous test modeled an unconditional last-engine fence, which is the
serialization removed here rather than a Vulkan ordering requirement.
"""
from pathlib import Path

root = Path(__file__).resolve().parents[1]
source = (root / 'g17_drm.rs').read_text()
manager = (root / 'g17_manager.rs').read_text()


def function(text, marker):
    start = text.index(marker)
    end = text.index('{', start) + 1
    depth = 1
    while depth:
        depth += (text[end] == '{') - (text[end] == '}')
        end += 1
    return text[start:end]


assert 'last_submission: Option<Fence>' not in source
assert 'last_publication: Option<Fence>' not in source
for engine, frontier, prefix in [
    ('compute', 'render_frontier', 'render_wait_through'),
    ('render', 'compute_frontier', 'compute_wait_through'),
]:
    body = function(source, '    fn submit_' + engine + '_async(')
    assert '&self.' + frontier + ', translated.' + prefix in body
    # v84: the render enqueue also takes the early-publication decision.
    enqueue = '.enqueue(packet, dependencies)?' if engine == 'compute' else '.enqueue(packet, dependencies, early_allowed)?'
    assert body.index('append_render_prefix_dependencies(') < body.index(enqueue)
    assert 'for sync in syncs {' in body
    assert 'checked_dependencies.push(fence.clone()' in body
    assert 'dependencies.push(fence.clone()' in body
    assert 'last_submission' not in body

# Requested prefixes wait for publication, with hardware errors checked separately.
append = function(source, 'fn append_render_prefix_dependencies(')
assert 'checked_dependencies.push(fence.completed.clone()' in append
assert 'dependencies.push(fence.ready.clone()' in append  # j700 b23cfb38f: ready = completed for host timestamps
for trait, implementation, publish in [
    ('G17PQueueBackend', 'G17PAsyncQueueBackend', 'publish_async_compute('),
    ('G17PRenderQueueBackend', 'G17PRenderQueueBackend', 'submit_translated_render('),
]:
    body = function(source, 'impl g17_job::' + trait + ' for ' + implementation)
    if implementation == 'G17PRenderQueueBackend':
        # v84: the trait publish delegates to publish_with (early ioctl lease).
        body = function(source, '    fn publish_with(')
    assert body.index('for fence in packet.input_fences()') < body.index(publish)
    assert body.index('for fence in packet.firmware_fences()') < body.index(publish)

# Same-engine publication is FIFO even with multiple hardware credits.
entity = (root.parent / 'scheduler/sched_entity.c').read_text()
pop = function(entity, 'struct drm_sched_job *drm_sched_entity_pop_job(')
peek = pop.index('drm_sched_entity_queue_peek(entity)')
dependency = pop.index('drm_sched_job_dependency(sched_job, entity)')
blocked = pop.index('return NULL;', dependency)
dequeue = pop.index('spsc_queue_pop(&entity->job_queue)')
assert peek < dependency < blocked < dequeue

# Both physical render pairs receive the resolved native prefix.
stage = function(manager, '    fn stage_render_groups_with_sksm(')
assert 'previous_fragment_barriers: &[g17_submission::G17PClBarrierDependency]' in stage
assert 'queues.fragment.producer.last_add_kicks_timestamp' not in stage
print('PASS explicit cross-engine prefixes, whole-ioctl input waits and same-engine publication FIFO')
