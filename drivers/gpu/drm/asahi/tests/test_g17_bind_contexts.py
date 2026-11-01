#!/usr/bin/env python3
"""Compile the production T8140 UAT bind-context pool and exercise it.

Nothing here is a second model of the allocator: the slot-selection loop, the
slot->hardware-context mapping, the owner bookkeeping and the guard's release
path are all lifted verbatim out of `slotalloc.rs` and `mmu.rs`. Only the
kernel synchronisation primitives are mocked, and the condvar mock always
times out so an exhausted pool is deterministic instead of blocking.
"""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parent.parent
slotalloc = (root / "slotalloc.rs").read_text()
mmu = (root / "mmu.rs").read_text()
from g17_queue_limits_test_support import queue_limits_source
limits = queue_limits_source()


def block(source, marker):
    start = source.index(marker)
    open_brace = source.index("{", start)
    depth = 1
    end = open_brace + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


def item(source, name):
    match = re.search(r"^(?:pub\(crate\) )?const " + name + r":.*$", source, re.M)
    assert match, name
    return match[0]


mock = r"""
#![allow(dead_code, unused_variables, unused_imports)]
use core::num::NonZeroUsize;
use core::ops::{Deref, DerefMut};
use std::sync::Arc;

type Result<T = ()> = std::result::Result<T, i32>;
const EBUSY: i32 = 16;
const ERESTARTSYS: i32 = 512;
const EIO: i32 = 5;
const GFP_KERNEL: u32 = 0;

macro_rules! pr_warn { ($($t:tt)*) => {} }
macro_rules! pr_crit { ($($t:tt)*) => {} }

fn msecs_to_jiffies(ms: u32) -> u64 { ms as u64 }

enum CondVarTimeoutResult { Timeout, Woken { jiffies: u64 }, Signal { jiffies: u64 } }

struct Mutex<T>(std::sync::Mutex<T>);
impl<T> Mutex<T> {
    fn new(value: T) -> Self { Self(std::sync::Mutex::new(value)) }
    fn lock(&self) -> std::sync::MutexGuard<'_, T> { self.0.lock().unwrap() }
}

/// The pool is never contended across threads in this harness, so a wait can
/// only ever mean "exhausted". Timing out is what the production
/// CONSTRAINED_SLOT_WAIT_MS path does in that case.
struct CondVar;
impl CondVar {
    fn wait_interruptible_timeout<T>(
        &self, _guard: &mut std::sync::MutexGuard<'_, T>, _jiffies: u64,
    ) -> CondVarTimeoutResult { CondVarTimeoutResult::Timeout }
    fn wait_interruptible<T>(&self, _guard: &mut std::sync::MutexGuard<'_, T>) -> bool { true }
    fn notify_one(&self) {}
}

type KVec<T> = Vec<T>;
"""

harness = r"""
fn allocator(slots: u32) -> SlotAllocator<SlotInner> {
    let mut entries = Vec::new();
    for _ in 0..slots {
        entries.push(Some(Entry { item: SlotInner(), get_time: 0, drop_time: 0 }));
    }
    SlotAllocator(Arc::new(SlotAllocatorOuter {
        inner: Mutex::new(SlotAllocatorInner {
            data: SlotOwners::new(),
            slots: entries,
            get_count: 0,
            drop_count: 0,
            slot_limit: usize::MAX,
            lowest_first: false,
        }),
        cond: CondVar,
    }))
}

/// The production `Uat::bind` preamble: same limit, same policy, same owner
/// record, same slot -> hardware context mapping.
fn bind(
    alloc: &SlotAllocator<SlotInner>, token: Option<SlotToken>, vm_id: u64, robust: bool,
) -> Result<(Guard<SlotInner>, usize)> {
    alloc.set_limit(if robust { NonZeroUsize::new(1) } else { NonZeroUsize::new(T8140_BIND_SLOTS) });
    alloc.set_lowest_first(true);
    let guard = alloc.get_inner(token, |data, guard| {
        data.record_owner(guard.slot(), vm_id);
        Ok(())
    })?;
    let context = t8140_bind_context(guard.slot()).ok_or(EBUSY)?;
    Ok((guard, context))
}
"""

tests = r"""
#[test]
fn slot_zero_is_the_opening_context_and_the_band_is_disjoint() {
    assert_eq!(T8140_BIND_SLOTS, 1 + g17_queue_limits::BIND_CONTEXT_COUNT as usize);
    assert_eq!(t8140_bind_context(0), Some(1));
    for slot in 1..T8140_BIND_SLOTS as u32 {
        let context = t8140_bind_context(slot).unwrap();
        assert_eq!(context, g17_queue_limits::BIND_CONTEXT_FIRST as usize + slot as usize - 1);
    }
    assert_eq!(t8140_bind_context(T8140_BIND_SLOTS as u32), None);
    assert_eq!(t8140_bind_context(63), None);

    // No bind context may collide with the kernel context, the two contexts a
    // compute kick declares, the accelerator reservation, a compute execution
    // context, or the render app context.
    let mut seen = 0u64;
    for slot in 0..T8140_BIND_SLOTS as u32 {
        let context = t8140_bind_context(slot).unwrap();
        assert!(context < UAT_NUM_CTX);
        assert!(context != 0 && context != 4);
        assert!(!T8140_COMPUTE_CTXS.contains(&context));
        assert_ne!(context as u32, g17_queue_limits::RENDER_APP_CONTEXT);
        assert_eq!(g17_queue_limits::COMPUTE_CONTEXT_MASK & (1u64 << context), 0);
        assert_eq!(seen & (1u64 << context), 0, "duplicate context");
        seen |= 1u64 << context;
        // A selector is six bits wide in the fragment MCache record.
        assert!(context <= 0x3f);
    }
    // Everything above context 1 must be reserved in the band mask, so the
    // application-GART allocators can never hand the same ID out.
    assert_eq!(seen & !2, g17_queue_limits::BIND_CONTEXT_MASK);
}

#[test]
fn concurrent_clients_get_distinct_contexts_lowest_first() {
    let alloc = allocator(UAT_USER_CTX as u32);
    let mut held = Vec::new();
    for vm in 0..T8140_BIND_SLOTS as u64 {
        let (guard, context) = bind(&alloc, None, vm + 1, false).unwrap();
        assert_eq!(guard.slot() as u64, vm);
        held.push((guard, context));
    }
    // The first client is on the measured opening context, later ones are not.
    assert_eq!(held[0].1, 1);
    for entry in &held[1..] {
        assert!(entry.1 >= g17_queue_limits::BIND_CONTEXT_FIRST as usize);
    }
    assert_eq!(alloc.busy_mask(), (1u64 << T8140_BIND_SLOTS) - 1);
    assert_eq!(
        alloc.with_inner(|data| data.snapshot()),
        core::array::from_fn::<u64, T8140_BIND_SLOTS, _>(|slot| slot as u64 + 1),
    );

    // The pool is full: the next client fails cleanly instead of blocking.
    assert!(matches!(bind(&alloc, None, 99, false), Err(EBUSY)));

    // A client going away hands its hardware context straight back, and the
    // owner record is cleared with it.
    let (guard, context) = held.remove(0);
    assert_eq!(context, 1);
    drop(guard);
    assert_eq!(alloc.busy_mask() & 1, 0);
    assert_eq!(alloc.with_inner(|data| data.snapshot())[0], 0);
    let (next, next_context) = bind(&alloc, None, 99, false).unwrap();
    assert_eq!(next.slot(), 0);
    assert_eq!(next_context, 1);
    assert_eq!(alloc.with_inner(|data| data.snapshot())[0], 99);
    drop(next);
    drop(held);
    assert_eq!(alloc.busy_mask(), 0);
    assert_eq!(alloc.with_inner(|data| data.snapshot()), [0; T8140_BIND_SLOTS]);

    // And the pool restarts from the lowest context afterwards.
    let (again, again_context) = bind(&alloc, None, 7, false).unwrap();
    assert_eq!((again.slot(), again_context), (0, 1));
}

#[test]
fn a_returning_client_keeps_its_own_context() {
    let alloc = allocator(UAT_USER_CTX as u32);
    let (first, first_context) = bind(&alloc, None, 1, false).unwrap();
    if T8140_BIND_SLOTS == 1 {
        let token = first.token();
        drop(first);
        let (again, again_context) = bind(&alloc, Some(token), 1, false).unwrap();
        assert_eq!((again.slot(), again_context), (0, first_context));
        assert!(!again.changed());
        return;
    }
    let (second, second_context) = bind(&alloc, None, 2, false).unwrap();
    let token = second.token();
    assert_ne!(first_context, second_context);
    drop(second);
    // Slot 0 is still busy, so lowest-first alone would be no evidence. Free
    // it too: the token must still win, because moving a live client's root
    // to another hardware context would invalidate the MCache selector its
    // retained descriptors already encode.
    drop(first);
    let (again, again_context) = bind(&alloc, Some(token), 2, false).unwrap();
    assert_eq!(again.slot(), token.last_slot());
    assert_eq!(again_context, second_context);
    assert!(!again.changed());
}

#[test]
fn robust_isolation_restores_the_single_bindable_context() {
    let alloc = allocator(UAT_USER_CTX as u32);
    let (first, context) = bind(&alloc, None, 1, true).unwrap();
    assert_eq!(context, 1);
    assert!(matches!(bind(&alloc, None, 2, true), Err(EBUSY)));
    drop(first);
    let (second, context) = bind(&alloc, None, 2, true).unwrap();
    assert_eq!(context, 1);
    drop(second);
}

#[test]
fn an_unlimited_allocator_still_uses_lru() {
    let alloc = allocator(2);
    alloc.set_limit(None);
    alloc.set_lowest_first(false);
    let a = alloc.get_inner(None, |_, _| Ok(())).unwrap();
    let b = alloc.get_inner(None, |_, _| Ok(())).unwrap();
    assert_eq!((a.slot(), b.slot()), (0, 1));
    drop(b);
    drop(a);
    // LRU: slot 1 was released first, so it is the oldest and comes back
    // first. Lowest-first would have answered 0.
    let c = alloc.get_inner(None, |_, _| Ok(())).unwrap();
    assert_eq!(c.slot(), 1);
}
"""

pieces = [
    mock,
    "mod g17_queue_limits{" + limits + "}",
    "use g17_queue_limits::*;",
    item(mmu, "UAT_NUM_CTX"),
    item(mmu, "UAT_USER_CTX_START"),
    item(mmu, "UAT_USER_CTX"),
    item(mmu, "T8140_COMPUTE_CTXS"),
    item(slotalloc, "CONSTRAINED_SLOT_WAIT_MS"),
    re.search(r"^const T8140_BIND_SLOTS: usize =[\s\S]*?;", mmu, re.M)[0],
    block(mmu, "const fn t8140_bind_context(slot: u32) -> Option<usize> {"),
    "pub(crate) struct SlotInner();",
    block(mmu, "pub(crate) struct SlotOwners {"),
    block(mmu, "impl SlotOwners {"),
    block(mmu, "impl slotalloc::SlotItem for SlotInner {").replace("slotalloc::", ""),
    block(slotalloc, "pub(crate) trait SlotItem {"),
    "impl SlotItem for () { type Data = (); }",
    "#[derive(Copy, Clone, Debug)]",
    block(slotalloc, "pub(crate) struct SlotToken {"),
    block(slotalloc, "impl SlotToken {"),
    block(slotalloc, "pub(crate) struct Guard<T: SlotItem> {"),
    block(slotalloc, "impl<T: SlotItem> Guard<T> {"),
    block(slotalloc, "struct Entry<T: SlotItem> {"),
    block(slotalloc, "struct SlotAllocatorInner<T: SlotItem> {"),
    block(slotalloc, "struct SlotAllocatorOuter<T: SlotItem> {").replace("#[pin]", ""),
    "pub(crate) struct SlotAllocator<T: SlotItem>(Arc<SlotAllocatorOuter<T>>);",
    "\n".join([
        "impl<T: SlotItem> SlotAllocator<T> {",
        block(slotalloc, "    pub(crate) fn with_inner<RetVal>("),
        block(slotalloc, "    pub(crate) fn set_limit("),
        block(slotalloc, "    pub(crate) fn set_lowest_first("),
        block(slotalloc, "    pub(crate) fn busy_mask("),
        block(slotalloc, "    pub(crate) fn get_inner("),
        "}",
    ]),
    block(slotalloc, "impl<T: SlotItem> Drop for Guard<T> {"),
    harness,
    tests,
]

with tempfile.TemporaryDirectory(prefix="g17-bind-contexts-") as temp:
    source = Path(temp) / "test.rs"
    source.write_text("\n".join(pieces))
    binary = Path(temp) / "test"
    subprocess.run(
        ["rustc", "--edition=2021", "--test", "-Awarnings", str(source), "-o", str(binary)],
        check=True,
    )
    subprocess.run([str(binary), "--test-threads=1"], check=True)

print(
    "PASS production T8140 bootstrap bind: slot 0 is opening context 1, ordinary "
    "app-GART contexts are disjoint, release/reuse and token stability hold, "
    "robust_isolation preserves the single-context "
    "behaviour, and non-T8140 allocators keep LRU"
)
