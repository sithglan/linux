#!/usr/bin/env python3
"""g17p_exit_kill: a closed context's compute kicks that will never retire
(the Steam G2a quarantines, boot 6915cf0d) no longer pin the client's VM for
the boot. Runs the production decision module (scope, kick classification,
release decision, acknowledgement table) on the hardware page bytes, the
exact 0x29 record and type-12 event layouts, the render-pair hold in the
real lease policy, and checks the wiring: teardown turn -> 250 ms -> hold ->
publish; event 12 -> release only with ack + page store + no running kick;
packets/VM pins released with the graph kept quarantined; processor stop
clears everything; default on."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
src = {name: (root / name).read_text() for name in [
    "g17_exit_kill.rs", "g17_initdata.rs", "g17_completion.rs", "g17_drm.rs",
    "g17_async_compute.rs", "g17_live_boot.rs", "g17_manager.rs", "g17_resources.rs",
    "g17_queue_limits.rs", "asahi.rs",
]}


def item(source, marker):
    start = source.index(marker)
    end = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


def const(source, name):
    match = re.search(r"(?:pub\(crate\) )?const " + name + r": [^=]+= ([^;]+);", source)
    assert match, name
    return match.group(0)


def run(rust, name):
    with tempfile.TemporaryDirectory(prefix="g17-exit-kill-") as directory:
        path = Path(directory) / (name + ".rs")
        path.write_text(rust)
        binary = Path(directory) / name
        subprocess.run(["rustc", "--edition=2021", "--test", "-Awarnings", str(path), "-o", str(binary)],
                       check=True)
        subprocess.run([str(binary), "--test-threads=1"], check=True)


# 1. The decision module itself, with its unit tests on the Steam page bytes.
with tempfile.TemporaryDirectory(prefix="g17-exit-kill-unit-") as directory:
    binary = Path(directory) / "unit"
    subprocess.run(["rustc", "--edition=2021", "--test", "-Awarnings", str(root / "g17_exit_kill.rs"),
                    "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)

# 2. Record and event layouts, extracted verbatim.
initdata, completion = src["g17_initdata.rs"], src["g17_completion.rs"]
layouts = "\n".join([
    "#![allow(dead_code)]",
    "#[derive(Debug, PartialEq)] pub(crate) enum InitdataError { BufferSize }",
    "#[derive(Debug, PartialEq)] pub(crate) enum CompletionDecodeError { RecordSize, FirmwareEventType }",
    const(initdata, "CONTROL_RECORD_SIZE"),
    const(initdata, "CONTROL_SCHEDULER_STATE_RELEASE_OPCODE"),
    const(initdata, "CONTROL_CONTEXT_KILL_OPCODE"),
    item(initdata, "fn put_u32("), item(initdata, "fn put_u64("),
    item(initdata, "pub(crate) fn encode_context_kill_record("),
    item(initdata, "pub(crate) fn encode_scheduler_state_release_record("),
    item(initdata, "pub(crate) const fn device_control_asserts_gpu_power("),
    const(completion, "G17P_FIRMWARE_EVENT_ENTRY_SIZE"),
    const(completion, "G17P_FIRMWARE_EVENT_PROCESS_EXIT_TYPE"),
    item(completion, "fn get_u32("), item(completion, "fn get_u64("),
    item(completion, "pub(crate) fn decode_g17p_process_exit_event("),
    r'''
#[test] fn kill_record_is_processexittimeouthandlers_stack_record() {
    // Native builds {u32 0x29 @sp+8, u64 queue+0x8b8 @sp+0xc, u64 id @sp+0x14}
    // over a zeroed 0x40-byte record; firmware 0x49cc reads [x24,#4], [x24,#0xc].
    let mut raw = [0xa5u8; CONTROL_RECORD_SIZE];
    encode_context_kill_record(0xffff_fc20_0325_c000, 0x1234_5678_9abc, &mut raw).unwrap();
    assert_eq!(u32::from_le_bytes(raw[0..4].try_into().unwrap()), 0x29);
    assert_eq!(u64::from_le_bytes(raw[4..12].try_into().unwrap()), 0xffff_fc20_0325_c000);
    assert_eq!(u64::from_le_bytes(raw[12..20].try_into().unwrap()), 0x1234_5678_9abc);
    assert!(raw[20..].iter().all(|byte| *byte == 0));
    assert_eq!(encode_context_kill_record(1, 1, &mut [0; 0x3f]), Err(InitdataError::BufferSize));
    // Like the 0x14 release, 0x29 is a power-asserting control (flag byte 1).
    assert_eq!(device_control_asserts_gpu_power(CONTROL_CONTEXT_KILL_OPCODE), Some(true));
    assert_ne!(CONTROL_CONTEXT_KILL_OPCODE, CONTROL_SCHEDULER_STATE_RELEASE_OPCODE);
}
#[test] fn type12_carries_the_cookie_at_four() {
    // fw 0x5110 / 0x26c1c: w8=0xc stored at sp+0x18, cookie stur at sp+0x1c.
    let mut raw = [0u8; G17P_FIRMWARE_EVENT_ENTRY_SIZE];
    raw[0..4].copy_from_slice(&12u32.to_le_bytes());
    raw[4..12].copy_from_slice(&77u64.to_le_bytes());
    assert_eq!(decode_g17p_process_exit_event(&raw), Ok(77));
    raw[0] = 13;
    assert_eq!(decode_g17p_process_exit_event(&raw), Err(CompletionDecodeError::FirmwareEventType));
    assert_eq!(decode_g17p_process_exit_event(&raw[..0x40]), Err(CompletionDecodeError::RecordSize));
}
''',
])
run(layouts, "layouts")

# 3. The render-pair hold in the production lease policy.
drm, resources, limits = src["g17_drm.rs"], src["g17_resources.rs"], src["g17_queue_limits.rs"]
slot_count = re.search(r"pub\(crate\) const SLOT_COUNT: u8 = (\d+);", resources).group(1)
lease = r'''
#![allow(dead_code)]
mod g17_resources { pub struct G17PRenderQueuePair; impl G17PRenderQueuePair { pub const SLOT_COUNT: u8 = @SLOTS@; } }
mod g17_queue_limits {
 pub const MAX_IN_FLIGHT_PER_QUEUE: u32 = @MAX@;
 pub const RENDER_KICKS_PER_PAIR: u32 = @KICKS@;
}
mod module_parameters {
 pub struct P; impl P { pub fn value(&self) -> &u32 { &0 } }
 #[allow(non_upper_case_globals)] pub static g17p_render_single_pair: P = P;
}
const G17P_RENDER_SLOTS: usize = g17_resources::G17PRenderQueuePair::SLOT_COUNT as usize;
const G17P_ACTIVE_RENDER_SLOTS: usize = G17P_RENDER_SLOTS;
''' + "\n".join([item(drm, "struct G17PQueueLeaseState {"),
                 item(drm, "impl Default for G17PQueueLeaseState {"),
                 item(drm, "impl G17PQueueLeaseState {")]) + r'''
/// `G17PQueueRegistry::release_render_owner` for a closed owner whose kicks
/// all completed.
fn close(s: &mut G17PQueueLeaseState, owner: u64) -> u64 {
    let mut slots = 0;
    for slot in 0..G17P_RENDER_SLOTS {
        if s.render_owner[slot] == owner {
            while s.render_refs[slot] != 0 { s.release_render_slot(slot as u8); }
            s.render_released |= 1u64 << slot;
            slots |= 1u64 << slot;
        }
    }
    slots
}
#[test] fn killed_pairs_are_never_handed_to_a_successor() {
    let mut s = G17PQueueLeaseState::default();
    s.acquire_render_slot(7).unwrap();
    let dead = close(&mut s, 7);
    assert_eq!(dead.count_ones(), 2);
    assert!(s.hold_for_exit_kill(7, dead));
    // Fill the whole namespace with new owners: none lands on a held pair.
    let mut owner = 100;
    while let Some(slot) = s.acquire_render_slot(owner) {
        assert_eq!(dead & (1u64 << slot), 0, "owner {} got held slot {}", owner, slot);
        owner += 1;
    }
    for slot in 0..G17P_RENDER_SLOTS {
        if dead & (1u64 << slot) != 0 { assert_eq!(s.render_owner[slot], 7); }
    }
}
#[test] fn without_a_hold_the_closed_pairs_are_reused_first() {
    let mut s = G17PQueueLeaseState::default();
    s.acquire_render_slot(7).unwrap();
    let dead = close(&mut s, 7);
    let first = s.acquire_render_slot(9).unwrap();
    assert_ne!(dead & (1u64 << first), 0, "control: reuse of a closed owner's pair is the default");
}
#[test] fn hold_is_all_or_nothing_and_only_for_the_departed_owner() {
    let mut s = G17PQueueLeaseState::default();
    s.acquire_render_slot(7).unwrap();
    let live = s.acquire_render_slot(8).unwrap();
    let dead = close(&mut s, 7);
    // A successor already took one of the pairs: refuse, hold nothing.
    let taken = s.acquire_render_slot(9).unwrap();
    assert_ne!(dead & (1u64 << taken), 0);
    assert!(!s.hold_for_exit_kill(7, dead));
    assert_eq!(s.render_kill_hold, 0);
    // A live owner's pair, slot 0, owner 0: refused.
    assert!(!s.hold_for_exit_kill(8, 1u64 << live));
    assert!(!s.hold_for_exit_kill(7, 1));
    assert!(!s.hold_for_exit_kill(0, dead));
}
'''
lease = (lease.replace("@SLOTS@", slot_count)
         .replace("@MAX@", re.search(r"pub\(crate\) const MAX_IN_FLIGHT_PER_QUEUE: [^=]+= ([^;]+);", limits).group(1))
         .replace("@KICKS@", re.search(r"pub\(crate\) const G17P_RENDER_MAX_IN_FLIGHT: [^=]+= ([^;]+);", resources).group(1)))
run(lease, "lease")

# 4. Wiring.
compute, live, manager = src["g17_async_compute.rs"], src["g17_live_boot.rs"], src["g17_manager.rs"]

# Registry: holds exclude pairs from every hand-out and from the ENOSPC test;
# processor stop drops them.
acquire = item(drm, "    fn acquire_render_slot(")
sibling = item(drm, "    fn claim_sibling_render_slot(")
assert "& !self.render_kill_hold" in acquire and "& !self.render_kill_hold" in sibling
assert "& !held.render_kill_hold" in item(drm, "    fn acquire_lease(")
stop = item(drm, "    pub(crate) fn release_retained_owners_after_processor_stop(")
assert "held.render_kill_hold = 0;" in stop
assert "self.lease.lock().hold_for_exit_kill(owner, slots)" in \
    item(drm, "    pub(crate) fn hold_render_slots_for_exit_kill(")

# Teardown turn: acknowledgements first, then a non-idle compute context gets
# the kill, never a scheduler-state release.
teardown = item(live, "    pub(crate) fn service_logical_queue_teardowns(")
assert teardown.index("self.release_exit_killed_compute();") < teardown.index("let outstanding")
gate = teardown[teardown.index("if !self.compute_idle_for_context(&context) {"):]
gate = gate[:gate.index("if self.primary_device_control_backpressured()")]
assert "self.publish_exit_kill_for_context(&context, age)" in gate and gate.rstrip().endswith("}")
assert "continue;" in gate and "publish_logical_queue_release" not in gate

# Publication: native timeout, released owner, scope, hold before the record,
# hold undone if nothing was published, announcement on EP 0x21.
publish = item(compute, "    pub(crate) fn publish_exit_kill_for_context(")
assert "exit_kill_candidate(q, context) && kill::kill_due(teardown_age_ns, q.quarantined)" in publish
candidate = item(compute, "fn exit_kill_candidate(")
assert "q.released" in candidate and "(q.quarantined || q.retired_by_teardown)" in candidate
assert "q.exit_kill.is_none()" in candidate and "q.inflight.is_some()" in candidate
# A cancelled (not quarantined) kick is killed only after the compute
# scheduler timeout, the point at which a live client's job is quarantined.
timeout = re.search(r"const SCHEDULER_TIMEOUT_MS: kernel::time::Msecs =\s*if cfg!\(g17p_short_timeout_probe\) \{ 100 \} else \{ (\d+) \};",
                    (root / "g17_job.rs").read_text())
assert timeout and int(timeout.group(1)) * 1_000_000 == int(
    re.search(r"const COMPUTE_TIMEOUT_NS: i64 = ([\d_]+);", src["g17_exit_kill.rs"]).group(1).replace("_", ""))
# The event worker keeps polling (bounded) while a kill becomes due or
# awaits its acknowledgement.
polling = item(live, "    pub(crate) fn logical_queue_teardown_polling(&self)")
assert "|| self.exit_kill_polling()" in polling
kill_polling = item(compute, "    pub(crate) fn exit_kill_polling(&self)")
assert kill_polling.count("< kill::POLL_NS") == 2
assert publish.index("kill::kill_scope(") < publish.index("hold_render_slots_for_exit_kill(")
assert publish.index("hold_render_slots_for_exit_kill(") < publish.index("manager.publish_exit_kill(")
assert "registry.release_exit_kill_hold(hold)" in publish
assert publish.index("manager.publish_exit_kill(") < publish.index("ENDPOINT_GFX_INTERRUPTS")
kill_record = item(manager, "    pub(crate) fn publish_exit_kill(")
assert "self.pending_primary_control.is_some()" in kill_record
assert "encode_context_kill_record(context.channel_control.gpu_va(), cookie" in kill_record
assert "publish_primary_device_control_raw(&record)" in kill_record
assert "0x14" not in kill_record

# Event 12 is recorded, never fatal.
drain = item(live, "fn drain_primary_firmware_event_batch(")
arm = drain[drain.index("G17P_FIRMWARE_EVENT_PROCESS_EXIT_TYPE =>"):drain.index("            unknown => {")]
assert "crate::g17_exit_kill::note_ack(cookie)" in arm
assert "?" not in arm.replace("{:?}", "") and "return Err" not in arm

# Release: only for an acknowledged cookie, decided on the page store and the
# kicks' own words; packets go, the graph stays quarantined.
release = item(compute, "    pub(crate) fn release_exit_killed_compute(")
assert release.index("kill::acked(exit_kill.cookie)") < release.index("kill::release_decision(")
assert "view.killed, recovery_closed" in release
assert "halted_timestamp_snapshot()" in release
assert release.count("release_exit_killed_packets(") == 1
released_arm = release[release.index("kill::Release::Release {"):]
assert "release_exit_killed_packets(q, pdev)" in released_arm
for arm_name in ["kill::Release::Pending", "kill::Release::AckWithoutKill", "kill::Release::Retain {"]:
    body = release[release.index(arm_name):]
    body = body[:body.index("kill::Release::", len(arm_name))] if "kill::Release::" in body[len(arm_name):] else body
    assert "release_exit_killed_packets" not in body, arm_name
packets = item(compute, "fn release_exit_killed_packets(")
assert "completion.release_after_retirement();" in packets
assert "q.retired_leases.push(lease)" in packets
assert "q.quarantined = true;" in packets
assert "ECANCELED" in packets and "ENODATA" in packets and "EIO" not in packets

# The teardown gate itself is unchanged: a context is released only once its
# compute queue holds no kick.
idle = item(live, "    fn compute_idle_for_context(")
assert "q.inflight.is_none()" in idle

# Processor stop forgets outstanding kills.
stop = item(compute, "    fn release_async_compute_after_processor_stop(")
assert stop.index("self.async_compute_queues.clear();") < stop.index("forget_all_acks()")

# Default on; 0 restores the previous retention exactly.
param = item(src["asahi.rs"], "        g17p_exit_kill: u32 {")
assert "default: 1," in param
assert "fn enabled()" in src["g17_exit_kill.rs"] and "g17p_exit_kill.value() & ENABLE != 0" in src["g17_exit_kill.rs"]
assert "if !kill::enabled()" in publish

# The page view reads the live firmware-owned bytes of the scheduler page.
view = item(resources, "    pub(crate) fn exit_kill_view(")
assert "snapshot_bytes::<{ crate::g17_exit_kill::STATE_KILL_BYTES }>()" in view

print("PASS G17P exit kill: 0x29 record/type-12 layout, page-scoped kill, render-pair hold, "
      "release on ack+page store+no running kick, graph stays quarantined, default on")
