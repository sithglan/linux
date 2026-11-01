#!/usr/bin/env python3
"""v92c regression: never take a render pair's FList over from another owner
while that owner's 0x2e release is still unobserved.

Fault on v92 (boot fc063e77, Q-NUDGE-d2, 1.2 s into the client):
  halted GMMU reason3 va=0x7010f35e80 unit=0xd9 DM1 (3D) vm-slot=10, repeated
  932 times across recoveries. The page is unmapped in the new client's VM
  (vm=157, context 10). Its pools are 134 (0x70157ec000..) and 135
  (0x7013fd8000..). The hole below pair A's retained contexts (0x7012744000)
  is exactly one pool (0x1810000) long and starts at 0x7010f34000. That is
  where the predecessor client's pool, with its page list at the start,
  sat. The fault is page_list+0x1e80 of that pool, in the successor's VM.
Mechanism (source):
  * finish_warm_render_pair_storage / begin_translated_render_with_hardware_buffer
    call acquire_owner(); released_for_reacquire() admits a predecessor in
    state Sent (0x2e published, consumption unobserved), carries the witness,
    then repopulates and stages the NEW pool for the same FList ID;
  * the predecessor's 0x2e for that ID can still be unprocessed by firmware:
    teardowns showed "still Draining" on every exit, and the lab read the
    control ring at consumer = producer - 1;
  * the successor's first 3D then runs against the ID's old pool state (the
    old page list, mapped only in the old VM). Recovery repairs no FList
    state, so the same command faults again after every recovery.
v92c: (a) the teardown poll settles consumed releases (bit20 disables);
(b) the submit path waits (200 us polls, 1 s bound, then ETIMEDOUT) before
taking over a pair with an unobserved witness from another owner (bit21
disables); (c) unconsumed control records get the EP 0x21 announcement again
after 2 ms, rate-limited (bit22 disables). All default-on; knobs 0x9000 and
0x4009000 leave them on.

With DRIVER_ROOT=<v92/v92b driver> the source guards fail and the model
reproduces the stale-pool access.
"""
import os, re
from pathlib import Path

ROOT = Path(os.environ.get('DRIVER_ROOT', Path(__file__).resolve().parents[1]))
LIVE = (ROOT / 'g17_live_boot.rs').read_text()
MANAGER = (ROOT / 'g17_manager.rs').read_text()
DRM = (ROOT / 'g17_drm.rs').read_text()
TB = (ROOT / 'g17_timing_buffer.rs').read_text()
RES = (ROOT / 'g17_resources.rs').read_text()


def item(text, needle):
    start = text.index(needle)
    depth, i = 1, text.index('{', start) + 1
    while depth:
        depth += (text[i] == '{') - (text[i] == '}')
        i += 1
    return text[start:i]


def code(text):
    return '\n'.join(line.split('//', 1)[0] for line in text.splitlines())


# ------------------------------------------------ inert suspects under 0x9000
default = int(re.search(r'DIAG_PRODUCTION_DEFAULT: u32 = \(1 << 12\) \| \(1 << 15\)', TB) and 0x9000)
fam = code(item(TB, 'pub(crate) fn fw_cached_family_mask()'))
assert '>> 27' in fam and '& 0xf' in fam
for knobs in (0x9000, 0x4009000):
    assert (knobs >> 27) & 0xf == 0, 'v90 cached families are off under the lab knobs'
pooled = code(item(RES, 'fn new_fw_pooled('))
assert re.search(r'fw_cached_family_mask\(\) & family == 0 \{\s*return alloc\(\);', pooled), \
    'v90: families off -> the unmodified allocation'
drop = code(item(RES, 'impl Drop for MappedObject'))
assert re.search(r'if self\.fw_pool == 0 \{ return; \}', drop), 'v90: a non-pooled object drops as before'
# v91 proof cache validates only user translation ranges (kernel aliases are not requested)
proof = code(item(DRM, 'fn g17p_translate_with_proof'))
assert 'VALIDATE_PROOF' in proof

# ------------------------------------------------------------- v92c guards
bits = {name: int(v) for name, v in re.findall(r'const (DIAG_(?:TEARDOWN_SETTLE|FLIST_TAKEOVER_GUARD|CONTROL_REANNOUNCE)_OFF): u32 = 1 << (\d+);', TB)}
assert bits == {'DIAG_TEARDOWN_SETTLE_OFF': 20, 'DIAG_FLIST_TAKEOVER_GUARD_OFF': 21,
                'DIAG_CONTROL_REANNOUNCE_OFF': 22}, bits
for knobs in (0x9000, 0x4009000):
    assert all(not (knobs >> b) & 1 for b in bits.values()), 'lab knob writes keep every guard on'

blocked = code(item(MANAGER, 'pub(crate) fn render_flist_takeover_blocked('))
# Shared FList: each pool leases its own FList ID and keeps it, with its
# context or teardown, through its release ACK. A successor therefore never
# takes over a predecessor's ID, which removes the stale-pool hazard above at
# its source; the per-pair guard is inert (always false) but stays wired in.
assert blocked.split('{', 1)[1].split() == ['false', '}'], blocked
assert 'Ok(Self(129 + bank as u16 * 64 + bit as u16))' in RES
assert 'keep the pool and ID alive through release ACK' in RES
ready = code(item(LIVE, 'pub(crate) fn render_flist_takeover_ready('))
assert ready.index('DIAG_FLIST_TAKEOVER_GUARD_OFF') < ready.index('settle_consumed_render_usc_freelist_releases()')
assert 'reannounce_pending_primary_control' in ready

publish = code(item(DRM, 'fn publish_with('))
guard_at = publish.index('render_flist_takeover_ready(render_slot, sibling, &self.context)')
warm_at = publish.index('take_warm_render_graph(')
assert guard_at < warm_at, 'the guard runs before any pair is warmed or taken over'
tail = publish[guard_at:warm_at]
assert 'drop(runtime)' in tail and 'fsleep(' in tail and 'continue' in tail and 'ETIMEDOUT' in tail, \
    'waiting drops Runtime, sleeps, retries, and fails closed'

teardown = code(item(LIVE, 'pub(crate) fn service_logical_queue_teardowns('))
assert teardown.index('settle_consumed_render_usc_freelist_releases()') < teardown.index('logical_queue_teardown_render_ready(')
assert 'DIAG_TEARDOWN_SETTLE_OFF' in teardown
assert re.search(r'if !recovery_pending \{\s*self\.reannounce_pending_primary_control', teardown), \
    'no announcement while a recovery holds the consumer'
reann = code(item(LIVE, 'fn reannounce_pending_primary_control('))
assert 'DIAG_CONTROL_REANNOUNCE_OFF' in reann and 'consumer == producer' in reann
for forbidden in ('publish_', 'fsleep', 'KVec'):
    assert forbidden not in reann, forbidden

# The takeover itself still admits a Sent predecessor (native behaviour), which
# is exactly why the guard sits in front of it.
reacq = code(item(MANAGER, 'fn released_for_reacquire('))
assert 'G17PBootstrapUmaRequestState::Sent' in reacq


# ------------------------------------------------------------------- model
def run(guard, fw_drains_on_own_doorbell, reannounce, recoveries=3):
    """One predecessor->successor handoff of FList ID N; returns faults seen.

    fw_drains_on_own_doorbell: the predecessor's 0x2e doorbell was acted on
    (release consumed, maybe unobserved). False reproduces the lab's
    consumer = producer - 1 (announcement lost, record pending).
    """
    fw_state = {'N': 'pool_X'}                  # firmware per-ID pool (X: predecessor)
    pending = [] if fw_drains_on_own_doorbell else [('release', 'N')]
    if fw_drains_on_own_doorbell:
        fw_state['N'] = None                    # released
    vm_s = {'pool_Y'}                           # successor VM maps only its own pool

    def drain():
        while pending:
            kind, fid = pending.pop(0)
            if kind == 'release':
                fw_state[fid] = None
            elif kind == 'register':
                fw_state[fid] = 'pool_Y'
    # successor takeover
    waited = 0
    if guard:
        while pending and waited < 5000:        # witness unobserved
            if reannounce:
                drain()                         # re-announcement makes firmware drain
            waited += 1
        if pending:
            return 'ETIMEDOUT'
    pending.append(('register', 'N'))           # stage/grow registration for pool Y
    faults = 0
    # successor's first 3D runs before the firmware drains the control ring
    for _ in range(1 + recoveries):
        pool = fw_state['N'] or 'pool_Y'
        if pool not in vm_s:
            faults += 1                         # GMMU: old page list unmapped in VM_S
            continue                            # recovery re-runs the same command; FList untouched
        break
    drain()
    return faults


assert run(guard=False, fw_drains_on_own_doorbell=False, reannounce=False) == 4, \
    'model reproduces the repeated stale-pool fault of the old code'
assert run(guard=True, fw_drains_on_own_doorbell=False, reannounce=True) == 0
assert run(guard=True, fw_drains_on_own_doorbell=False, reannounce=False) == 'ETIMEDOUT', \
    'without re-announcement the guard fails closed instead of faulting'
assert run(guard=False, fw_drains_on_own_doorbell=True, reannounce=False) == 0, \
    'a consumed release is harmless: the stall and the fault need the unconsumed record'
print('v92c FList takeover guard: all checks passed')
