#!/usr/bin/env python3
"""g17p_tile_backstop: in-flight renders whose attachment footprints overlap
are ordered by the kernel when userspace cannot order them (tilewaw `queues`
and `xproc`). Runs the production decision module, the production pair
selection for same-owner affinity, a model of two queues and two processes
writing one image through the real `plan`, and checks the wiring: footprints
from validated attachments (and shareable-object bindings), records held for
exactly the lease, bounded waits, the non-waiting early path, default on."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
src = {name: (root / name).read_text() for name in [
    "g17_tile_backstop.rs", "g17_drm.rs", "g17_job.rs", "mmu.rs", "file.rs", "asahi.rs",
    "g17_resources.rs", "g17_queue_limits.rs", "g17_submission.rs",
]}


def item(source, marker, start=0):
    begin = source.index(marker, start)
    end = source.index("{", begin) + 1
    depth = 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[begin:end]


def run(rust, name):
    with tempfile.TemporaryDirectory(prefix="g17-tile-") as directory:
        path = Path(directory) / (name + ".rs")
        path.write_text(rust)
        binary = Path(directory) / name
        subprocess.run(["rustc", "--edition=2021", "--test", "-Awarnings", str(path), "-o", str(binary)],
                       check=True)
        subprocess.run([str(binary), "--test-threads=1"], check=True)


# 1. The decision module and its own unit tests.
with tempfile.TemporaryDirectory(prefix="g17-tile-unit-") as directory:
    binary = Path(directory) / "unit"
    subprocess.run(["rustc", "--edition=2021", "--test", "-Awarnings", str(root / "g17_tile_backstop.rs"),
                    "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)

# The prefix rule is the submission module's, verbatim in meaning.
prefix = item(src["g17_submission.rs"], "pub(crate) fn g17p_render_prefix_contains(")
assert "sequence != 0 && prefix.is_some_and(|limit| sequence <= limit)" in prefix
ordered = item(src["g17_tile_backstop.rs"], "pub(crate) const fn ordered_after(")
assert "Some(limit) => sequence != 0 && sequence <= limit" in ordered

# 2. Same-owner affinity in the production pair selection, plus a model of
# two queues (same VM) and two processes (shared object) through `plan`.
drm, resources, limits = src["g17_drm.rs"], src["g17_resources.rs"], src["g17_queue_limits.rs"]
slot_count = re.search(r"pub\(crate\) const SLOT_COUNT: u8 = (\d+);", resources).group(1)
second_impl = drm.index("// ---- g17p_tile_backstop (B)")
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
mod g17_tile_backstop { @BACKSTOP@ }
const G17P_RENDER_SLOTS: usize = g17_resources::G17PRenderQueuePair::SLOT_COUNT as usize;
const G17P_ACTIVE_RENDER_SLOTS: usize = G17P_RENDER_SLOTS;
''' + "\n".join([item(drm, "struct G17PQueueLeaseState {"),
                 item(drm, "impl Default for G17PQueueLeaseState {"),
                 item(drm, "impl G17PQueueLeaseState {"),
                 item(drm, "impl G17PQueueLeaseState {", second_impl)]) + r'''
use g17_tile_backstop::{Footprint, Inflight, Plan, plan, DEFAULT_MASK};
const DEPTH: u8 = g17_queue_limits::RENDER_KICKS_PER_PAIR as u8;
#[test] fn affinity_takes_only_the_owners_pair_within_depth() {
    let mut s = G17PQueueLeaseState::default();
    let first = s.acquire_render_slot(7).unwrap();
    let other = s.acquire_render_slot(9).unwrap();
    // Stacks behind the owner's busy pair, up to the per-pair depth.
    for _ in 1..DEPTH { assert_eq!(s.acquire_render_slot_on(7, first), Some(first)); }
    assert_eq!(s.render_refs[first as usize], DEPTH);
    assert_eq!(s.acquire_render_slot_on(7, first), None, "depth bound");
    // Never another owner's pair, slot zero, or out of range.
    assert_eq!(s.acquire_render_slot_on(7, other), None);
    assert_eq!(s.acquire_render_slot_on(7, 0), None);
    assert_eq!(s.acquire_render_slot_on(7, 200), None);
    assert_eq!(s.acquire_render_slot_on(0, first), None);
    // An idle owned pair is taken like the ordinary path does.
    for _ in 0..DEPTH { s.release_render_slot(first); }
    assert_eq!(s.render_refs[first as usize], 0);
    assert_eq!(s.acquire_render_slot_on(7, first), Some(first));
    assert_eq!(s.render_refs[first as usize], 1);
    // Quarantined or kill-held: refused.
    s.release_render_slot(first);
    s.render_quarantined |= 1u64 << first;
    assert_eq!(s.acquire_render_slot_on(7, first), None);
    s.render_quarantined = 0;
    s.render_kill_hold |= 1u64 << first;
    assert_eq!(s.acquire_render_slot_on(7, first), None);
}

/// Two writers of one image: per step each tries to start one render with
/// the registry's rules (plan; a waiting render is recorded pending with a
/// ticket and converted on start), and running renders finish after a fixed
/// duration. Asserts no two cross-owner overlapping renders ever run at once,
/// and that both writers finish (no starvation).
fn two_writers(a: Footprint, b: Footprint) {
    struct Rec { owner: u64, sequence: u64, slot: u8, pending: bool, ticket: u64, ends: u32 }
    let prints = [a, b];
    let mut table: Vec<Rec> = Vec::new();
    let mut issued = [0u64; 2];
    let mut ticket = 1u64;
    let mut max_wait = 0u32;
    let mut waiting_since = [None::<u32>; 2];
    for now in 0..20_000u32 {
        table.retain(|r| r.pending || r.ends > now);
        for writer in 0..2usize {
            if issued[writer] == 200 { continue; }
            let owner = writer as u64 + 1;
            let mine = table.iter().find(|r| r.pending && r.owner == owner).map(|r| r.ticket);
            let p = plan(DEFAULT_MASK, owner, None, mine, &prints[writer], table.iter().map(|r| Inflight {
                owner: r.owner, sequence: r.sequence, slot: r.slot, footprint: &prints[(r.owner - 1) as usize],
                quarantined: false, pending: r.pending, ticket: r.ticket }));
            match p {
                Plan::Wait => {
                    if mine.is_none() {
                        table.push(Rec { owner, sequence: issued[writer] + 1, slot: u8::MAX, pending: true, ticket, ends: 0 });
                        ticket += 1;
                        waiting_since[writer] = Some(now);
                    }
                }
                _ => {
                    table.retain(|r| !(r.pending && r.owner == owner));
                    if let Some(since) = waiting_since[writer].take() { max_wait = max_wait.max(now - since); }
                    // Same-owner overlap with an unordered predecessor stacks,
                    // so one writer's renders end in order on one pair.
                    issued[writer] += 1;
                    let ends = now + 3 + writer as u32;
                    table.push(Rec { owner, sequence: issued[writer], slot: writer as u8 + 1, pending: false, ticket: 0, ends });
                }
            }
            let running: Vec<&Rec> = table.iter().filter(|r| !r.pending).collect();
            for x in &running { for y in &running {
                if x.owner != y.owner {
                    assert!(!prints[(x.owner - 1) as usize].overlaps(&prints[(y.owner - 1) as usize]),
                        "overlapping renders of two writers ran together at {}", now);
                }
            }}
        }
        if issued == [200, 200] && table.iter().all(|r| r.pending == false) { break; }
    }
    assert_eq!(issued, [200, 200], "a writer starved");
    assert!(max_wait < 64, "waits stay short in this model: {}", max_wait);
}
#[test] fn two_queues_of_one_vm_alternate_on_one_image() {
    let mut a = Footprint::new(3); a.push_vm_span(0x1_0000_0000, 0x1_0004_0000);
    two_writers(a.clone(), a);
}
#[test] fn two_processes_alternate_on_one_shared_image() {
    let mut a = Footprint::new(3); a.push_vm_span(0x1_0000_0000, 0x1_0004_0000);
    a.push_object_span(0xffff_0000_0000_4000, 0, 0x4_0000);
    let mut b = Footprint::new(4); b.push_vm_span(0x7_0000_0000, 0x7_0004_0000);
    b.push_object_span(0xffff_0000_0000_4000, 0, 0x4_0000);
    two_writers(a, b);
}
#[test] fn disjoint_writers_never_wait() {
    let mut a = Footprint::new(3); a.push_vm_span(0x1_0000_0000, 0x1_0004_0000);
    let mut b = Footprint::new(3); b.push_vm_span(0x2_0000_0000, 0x2_0004_0000);
    let running = [Inflight { owner: 2, sequence: 1, slot: 4, footprint: &b, quarantined: false, pending: false, ticket: 0 }];
    assert_eq!(plan(DEFAULT_MASK, 1, None, None, &a, running), Plan::Free);
}
'''
backstop = src["g17_tile_backstop.rs"]
backstop_items = backstop[backstop.index("pub(crate) const MAX_SPANS"):backstop.index("#[cfg(test)]")]
backstop_items = backstop_items.replace("pub(crate)", "pub").replace(
    "#[cfg(not(test))]\n#[inline(always)]\npub fn mask() -> u32 {\n    *crate::module_parameters::g17p_tile_backstop.value()\n}\n", "")
lease = (lease.replace("@SLOTS@", slot_count)
         .replace("@MAX@", re.search(r"pub\(crate\) const MAX_IN_FLIGHT_PER_QUEUE: [^=]+= ([^;]+);", limits).group(1))
         .replace("@KICKS@", re.search(r"pub\(crate\) const G17P_RENDER_MAX_IN_FLIGHT: [^=]+= ([^;]+);", resources).group(1))
         .replace("@BACKSTOP@", "use core::sync::atomic::{AtomicU64, Ordering};\n" + backstop_items))
run(lease, "lease")

# 3. Wiring.
acquire = item(drm, "    fn acquire_lease(self: &Arc<Self>, owner: u64, tile:")
assert "self.tile_plan(&held, owner, &tile, ticket)" in acquire
assert "Plan::Stack(slot) => held.acquire_render_slot_on(owner, slot)" in acquire
# Waits are bounded and re-plan on every lease release (lease_free).
assert "msecs_to_jiffies(backstop::WAIT_CAP_MS)" in acquire
assert "self.lease_free.wait_interruptible_timeout(&mut held, tile_budget)" in acquire
assert "tile_expired = true;" in acquire
# A waiting render is recorded pending (dropped on any early exit, converted
# when it starts), and recorded in flight once it holds its pair.
assert acquire.index("let mut held = self.lease.lock();") < acquire.index("let mut tile_pending")
assert "tile_pending = self.tile_wait(owner, &tile);" in acquire
assert acquire.index("drop(tile_pending.take());") < acquire.index("self.tile_record(owner, slot, &tile)")
early = item(drm, "    fn try_acquire_lease(self: &Arc<Self>, owner: u64, tile:")
assert "wait_interruptible" not in early and "lease_free" not in early and "tile_wait" not in early
assert "Plan::Wait => None," in early and "else { return Ok(None); };" in early
drop = item(drm, "impl Drop for G17PLeaseGuard {")
assert drop.index("let mut held = self.registry.lease.lock();") < drop.index("self.registry.tile_forget(") \
    < drop.index("drop(held);") < drop.index("self.registry.lease_free.notify_all();")
pending = item(drm, "impl Drop for G17PTilePending<'_> {")
assert "entry.slot == TILE_PENDING" in pending
# The record lives exactly as long as the lease, which a render's completion
# drops before its fence signals (so a semaphore-synchronized consumer never
# sees its producer in flight).
assert drm.index("drop(self.lease.lock().take());\n        let e2e_lease") < drm.index("self.fence.signal();\n            let e2e_hw")
# Footprint: validated fragment attachments, before the command copy, out of line.
submit = item(drm, "    fn submit_render_async(")
assert submit.index("self.tile_footprint(&translated.fragment_attachments)?") < submit.index("let mut translated = *translated;")
assert "G17PRenderJobPacket::new(" in submit and "footprint," in submit
footprint = item(drm, "    fn tile_footprint(&self, attachments: &g17_uapi::UapiAttachmentList)")
assert "#[inline(never)]\n    fn tile_footprint" in drm
assert "self.vm.shared_footprint_spans(attachment.address, attachment.size, &mut footprint);" in footprint
# (D) Shareable-object bindings: recorded after the commit, removed on unbind
# and GEM close; only external (non VM-private) multi-page bindings.
bind = item(src["file.rs"], "    pub(crate) fn vm_bind(")
assert bind.index("vm.commit_prepared_user_bind_batch(&mut batch)?;") < bind.index("vm.track_shared_binding(")
assert "vm.untrack_shared_range(op.addr..op.addr + op.range);" in bind
assert "vm.untrack_shared_object(bo);" in item(src["file.rs"], "    pub(crate) fn unbind_gem_object(")
track = item(src["mmu.rs"], "    pub(crate) fn track_shared_binding(")
assert "let shareable = !single_page && self.is_extobj(gem);" in track
assert "list.retain(|binding| binding.end <= address || end <= binding.start);" in track
spans = item(src["mmu.rs"], "    pub(crate) fn shared_footprint_spans(")
assert spans.index("count.load(Ordering::Relaxed) == 0 { return; }") < spans.index(".list.lock()")
# Default on; 0 restores the previous selection exactly.
param = item(src["asahi.rs"], "        g17p_tile_backstop: u32 {")
assert "default: 3," in param
assert "if mask & DEFAULT_MASK == 0 || footprint.is_empty() {\n        return Plan::Free;" in src["g17_tile_backstop.rs"]

print("PASS G17P tile backstop: footprints, same-owner affinity within the pair depth, "
      "bounded cross-queue/process waits with arrival order, records for exactly the lease, default on")
