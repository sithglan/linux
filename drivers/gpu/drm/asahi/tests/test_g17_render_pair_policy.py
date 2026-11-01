#!/usr/bin/env python3
"""Execute the production render-slot lease policy in both modes: by default a
render queue claims its two physical TA/3D pairs (the sibling at its first
acquisition, warmed before its first kick); with the opt-in
g17p_render_single_pair=1 it stays on one pair and stacks its kicks there."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
drm = (root / "g17_drm.rs").read_text()
limits = (root / "g17_queue_limits.rs").read_text()
resources = (root / "g17_resources.rs").read_text()
asahi = (root / "asahi.rs").read_text()


def item(source, marker):
    start = source.index(marker)
    end = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


def const(source, name):
    return re.search(r"pub\(crate\) const " + name + r": [^=]+= ([^;]+);", source).group(1).strip()


# Two pairs by default; one pair only when the opt-in parameter is set.
param = item(asahi, "        g17p_render_single_pair: u32 {")
assert "default: 0," in param
claim = item(drm, "    fn claim_sibling_render_slot(")
assert "g17p_render_single_pair.value() != 0 { return; }" in claim

state = "\n".join([
    item(drm, "struct G17PQueueLeaseState {"),
    item(drm, "impl Default for G17PQueueLeaseState {"),
    item(drm, "impl G17PQueueLeaseState {"),
])
slot_count = re.search(r"pub\(crate\) const SLOT_COUNT: u8 = (\d+);", resources).group(1)
rust = r'''
#![allow(dead_code)]
use std::cell::Cell;
mod g17_resources { pub struct G17PRenderQueuePair; impl G17PRenderQueuePair { pub const SLOT_COUNT: u8 = @SLOTS@; } }
mod g17_queue_limits {
 pub const MAX_IN_FLIGHT_PER_QUEUE: u32 = @MAX@;
 pub const RENDER_KICKS_PER_PAIR: u32 = @KICKS@;
}
thread_local! { static SINGLE: Cell<u32> = const { Cell::new(0) }; }
mod module_parameters {
 pub struct P;
 impl P { pub fn value(&self) -> &u32 { if super::SINGLE.with(|s| s.get()) != 0 { &1 } else { &0 } } }
 #[allow(non_upper_case_globals)] pub static g17p_render_single_pair: P = P;
}
const G17P_RENDER_SLOTS: usize = g17_resources::G17PRenderQueuePair::SLOT_COUNT as usize;
const G17P_ACTIVE_RENDER_SLOTS: usize = G17P_RENDER_SLOTS;
''' + state + r'''
fn owned(s: &G17PQueueLeaseState, owner: u64) -> Vec<usize> {
 (0..G17P_RENDER_SLOTS).filter(|slot| s.render_owner[*slot] == owner).collect()
}
const PAIRS: usize = g17_queue_limits::MAX_IN_FLIGHT_PER_QUEUE as usize;
const KICKS: u32 = g17_queue_limits::RENDER_KICKS_PER_PAIR;
#[test] fn default_queue_claims_its_sibling_at_first_acquisition() {
 let mut s = G17PQueueLeaseState::default();
 let first = s.acquire_render_slot(7).unwrap();
 assert_eq!(owned(&s, 7).len(), PAIRS, "both pairs from the first acquisition");
 let sibling = s.warmable_sibling_render_slot(7, first).unwrap();
 assert_ne!(sibling, first);
 // The idle, already claimed sibling takes the next kick.
 assert_eq!(s.acquire_render_slot(7), Some(sibling));
 // With both busy, kicks stack on the owner's pairs up to the per-pair
 // depth and never claim a third pair.
 for _ in 2..(PAIRS as u32 * KICKS) {
  let slot = s.acquire_render_slot(7).unwrap();
  assert!(slot == first || slot == sibling);
 }
 assert_eq!(s.acquire_render_slot(7), None);
 assert_eq!(owned(&s, 7).len(), PAIRS);
}
#[test] fn default_concurrent_first_submissions_each_claim_two_pairs() {
 let mut s = G17PQueueLeaseState::default();
 for owner in 1..=16u64 { s.acquire_render_slot(owner).unwrap(); }
 let mut all = Vec::new();
 for owner in 1..=16u64 { let pairs = owned(&s, owner); assert_eq!(pairs.len(), PAIRS); all.extend(pairs); }
 let n = all.len(); all.sort(); all.dedup(); assert_eq!(all.len(), n, "no pair is shared");
 assert!(!all.contains(&0), "slot zero stays the boot channel owner");
}
#[test] fn opt_in_single_pair_stacks_on_one_pair() {
 SINGLE.with(|v| v.set(1));
 let mut s = G17PQueueLeaseState::default();
 let first = s.acquire_render_slot(7).unwrap();
 assert_eq!(owned(&s, 7), vec![first as usize]);
 assert_eq!(s.warmable_sibling_render_slot(7, first), None);
 for _ in 1..KICKS { assert_eq!(s.acquire_render_slot(7), Some(first)); }
 assert_eq!(s.acquire_render_slot(7), None, "bounded by RENDER_KICKS_PER_PAIR");
 assert_eq!(owned(&s, 7), vec![first as usize], "never a second pair under load");
 for _ in 0..KICKS { s.release_render_slot(first); }
 assert_eq!(s.acquire_render_slot(7), Some(first), "idle owner gets its retained pair back");
 let others: Vec<u8> = (1..=16u64).map(|owner| s.acquire_render_slot(100 + owner).unwrap()).collect();
 for owner in 1..=16u64 { assert_eq!(owned(&s, 100 + owner).len(), 1); }
 let mut unique = others.clone(); unique.sort(); unique.dedup(); assert_eq!(unique.len(), 16);
 SINGLE.with(|v| v.set(0));
}
'''
rust = (rust.replace("@SLOTS@", slot_count)
        .replace("@MAX@", const(limits, "MAX_IN_FLIGHT_PER_QUEUE"))
        .replace("@KICKS@", const(resources, "G17P_RENDER_MAX_IN_FLIGHT")))
with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / "pair_policy.rs"
    path.write_text(rust)
    binary = Path(directory) / "pair_policy"
    subprocess.run(["rustc", "--edition=2021", "--test", str(path), "-o", str(binary)], check=True)
    subprocess.run([str(binary), "--test-threads=1"], check=True)
print("PASS render lease policy: two pairs per queue by default; one pair only with g17p_render_single_pair=1")
