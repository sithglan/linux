#!/usr/bin/env python3
"""v92b regression: a closing client's teardown must not wait for another client.

Failure on v92 (lab boot fc063e77): every render-client teardown logged
"G17P teardown: context N still Draining after 2000 ms", and the scheduler-state
release (0x14) was published and consumed only when the next client started.

Mechanism, on the actual source:
  * the teardown gate `render_slot_idle_for_context` needs each pair's FList
    lifecycle `Completed` (and not `hardware_buffer_owned`);
  * a sent 0x2e FList release becomes `Completed` only in
    `settle_consumed_render_usc_freelist_releases` (via
    `settle_release_witnesses` -> `observe_release_consumer`);
  * firmware does not notify on control consumption. On v92 that settle ran
    only (a) microseconds after the 0x2e doorbell and (b) before the next
    submission, never from the 1 ms teardown poll. When (a) races ahead of the
    firmware drain, nothing observes the consumed release until (b), i.e.
    until another client submits.
v92b settles in `service_logical_queue_teardowns` before the gate is read.

Checks: (1) source guards on the teardown service and on every settle site;
(2) the verbatim ring-witness predicate compiled and exercised with the
teardown numbers; (3) an event model of the exact failure sequence whose only
source-derived input is whether the teardown poll settles. With
DRIVER_ROOT=<v92 driver> the model reproduces the stall (the test fails).
"""
import os, re, subprocess, tempfile
from pathlib import Path

ROOT = Path(os.environ.get('DRIVER_ROOT', Path(__file__).resolve().parents[1]))
LIVE = (ROOT / 'g17_live_boot.rs').read_text()
MANAGER = (ROOT / 'g17_manager.rs').read_text()
DRM = (ROOT / 'g17_drm.rs').read_text()


def item(text, needle):
    start = text.index(needle)
    depth, i = 1, text.index('{', start) + 1
    while depth:
        depth += (text[i] == '{') - (text[i] == '}')
        i += 1
    return text[start:i]


def code(text):
    """Strip // comments so guards look at statements only."""
    return '\n'.join(line.split('//', 1)[0] for line in text.splitlines())


# ---------------------------------------------------------------- (1) source
service = code(item(LIVE, 'pub(crate) fn service_logical_queue_teardowns('))
SETTLE = 'settle_consumed_render_usc_freelist_releases()'
poll_settles = SETTLE in service and service.index(SETTLE) < service.index('logical_queue_teardown_render_ready(')

gate = code(item(MANAGER, 'fn render_slot_idle_for_context('))
# Shared FList: a pair is idle only without FList leases or an unretired FList
# ordinal; the teardown separately waits for each of its pools' lifecycles.
assert 'flist_leases.is_some()' in gate and 'flist_retired_ordinal.is_some()' in gate
pools = code(item(MANAGER, 'fn displaced_graphs_flists_released('))
assert 'G17PBootstrapUmaRequestState::Completed' in pools and 'hardware_buffer_owned' in pools, \
    'teardown gate still waits for a Completed FList lifecycle'
settle_fn = code(item(MANAGER, 'pub(crate) fn settle_consumed_render_usc_freelist_releases('))
assert 'settle_release_witnesses' in settle_fn and 'control_counters()' in settle_fn
# The settle is a counter snapshot: it must not publish, send, sleep or allocate.
for forbidden in ('publish_', 'send_message', 'fsleep', 'KVec', 'KBox', 'reserve('):
    assert forbidden not in settle_fn, forbidden
witness = code(item(MANAGER, 'fn settle_release_witnesses('))
assert 'observe_release_consumer' in witness
# Completed is reached only through the settle path (mark_completed callers).
production = MANAGER[:MANAGER.index('#[cfg(test)]\nmod ')] if '#[cfg(test)]\nmod ' in MANAGER else MANAGER
completed_callers = [m.start() for m in re.finditer(r'\.mark_completed\(', code(production))]
assert completed_callers and all(
    'fn observe_release_consumer' in code(production)[max(0, c - 600):c] for c in completed_callers), \
    'a Completed transition outside observe_release_consumer changes this analysis'
# Callers of the settle across the driver: post-send snapshot and pre-submission
# exist on every build; v92b adds the teardown poll.
sites = []
for name, text in (('live', LIVE), ('drm', DRM), ('manager', MANAGER)):
    for m in re.finditer(r'settle_consumed_render_usc_freelist_releases\(\)|'
                         r'settle_render_usc_freelist_release_before_submission\(\)', code(text)):
        if 'fn settle_' in code(text)[max(0, m.start() - 40):m.start()]:
            continue
        sites.append(name)
assert 'drm' in sites, 'pre-submission settle present'

# ------------------------------------------------------- (2) witness predicate
predicate = item(MANAGER, 'fn g17p_control_ring_target_consumed(')
mask = int(re.search(r'CONTROL_RING_ENTRIES: usize = (0x[0-9a-f]+|\d+)',
                     (ROOT / 'g17_initdata.rs').read_text()).group(1), 0) - 1
rust = '''#![allow(dead_code)]
mod g17_initdata { pub const CONTROL_RING_INDEX_MASK: u32 = %d; }
#[derive(Debug, PartialEq)] enum G17PBootstrapUmaLifecycleError { WrongCompletion }
%s
#[test] fn teardown_release_witness() {
    // 0x2e sent at producer_after = 200 (ring slot 199); nothing consumed yet.
    assert_eq!(g17p_control_ring_target_consumed(199, 200, 200), Ok(false));
    // Firmware drains it: consumer == producer == 200 -> consumed.
    assert_eq!(g17p_control_ring_target_consumed(200, 200, 200), Ok(true));
    // A later record from the next client does not hide the consumption.
    assert_eq!(g17p_control_ring_target_consumed(200, 201, 200), Ok(true));
    // Wrap-around at the 256-record ring.
    assert_eq!(g17p_control_ring_target_consumed(255, 1, 0), Ok(false));
    assert_eq!(g17p_control_ring_target_consumed(0, 1, 0), Ok(true));
}
''' % (mask, predicate)
with tempfile.TemporaryDirectory() as tmp:
    src = Path(tmp) / 't.rs'
    src.write_text(rust)
    rustc = os.environ.get('RUSTC', 'rustc')
    out = subprocess.run([rustc, '--edition=2021', '--test', str(src), '-o', str(Path(tmp) / 't')],
                         capture_output=True, text=True)
    assert out.returncode == 0, out.stderr
    run = subprocess.run([str(Path(tmp) / 't')], capture_output=True, text=True)
    assert run.returncode == 0, run.stdout + run.stderr


# ------------------------------------------------------------- (3) the model
def teardown_model(poll_settles, fw_drain_us, next_client_s):
    """Returns (release_published_at_s, warned). Times in seconds after close.

    t=0: the client's last render retired; its 0x2e was sent and the
    post-send snapshot ran before the firmware drained it (Sent, not
    Completed); the queue closes and the teardown is armed. The event worker
    polls every 1 ms for 2 s. Firmware drains the 0x2e after fw_drain_us.
    The next client's first submission runs the pre-submission settle.
    """
    POLL, WINDOW, WARN = 1e-3, 2.0, 2.0
    completed = False
    t = 0.0
    while t <= max(WINDOW, next_client_s) + POLL:
        consumed = t >= fw_drain_us * 1e-6
        in_poll = t <= WINDOW
        if in_poll and poll_settles and consumed:
            completed = True                    # service_logical_queue_teardowns settles
        if t >= next_client_s and consumed:
            completed = True                    # next client's pre-submission settle
        if completed and (in_poll or t >= next_client_s):
            return t, t >= WARN                 # gate open -> 0x14 published
        t += POLL
    return None, True


# Lab shape: firmware drains in ~20 us; next client 8-13 s later.
for gap in (8.0, 13.0):
    published, warned = teardown_model(poll_settles, fw_drain_us=20, next_client_s=gap)
    assert published is not None and published < 0.01 and not warned, (
        'teardown waits for the next client: release published at %.3f s (warned=%s); '
        'service_logical_queue_teardowns does not settle consumed FList releases' % (published, warned))
# Back-to-back clients (v84 runner shape, <2 s gap) never warned even without the
# fix, which is why the v84 logs show every release consumed at the next start.
published, warned = teardown_model(False, fw_drain_us=20, next_client_s=1.0)
assert published is not None and 0.99 < published < 1.01 and not warned
assert 'live' in sites, 'teardown service is a settle site'
print('v92b teardown FList settle: all checks passed (settle sites: %s)' % sorted(set(sites)))
