#!/usr/bin/env python3
"""Compile the production VM mutation journal with bounded-history races."""

from pathlib import Path
import subprocess
import tempfile


source = (Path(__file__).resolve().parents[1] / "mmu.rs").read_text()


def item(prefix):
    start = source.index(prefix)
    opening = source.index("{", start)
    depth = 1
    end = opening + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


program = "\n".join([
    "use core::ops::Range;",
    "use std::sync::Arc; use core::sync::atomic::{fence,AtomicU64,AtomicU32,Ordering};",
    # v94: g17p_mem_path bit3 (REACH_LOCKLESS) and MEM_STATS change counters.
    "mod g17_mem_path { use core::sync::atomic::{AtomicU32,AtomicU64,Ordering};"
    " pub const MEM_STATS: u32 = 1 << 1; pub const REACH_LOCKLESS: u32 = 1 << 3;"
    " pub static MASK: AtomicU32 = AtomicU32::new(0);"
    " pub static CHANGES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];"
    " pub fn enabled(bit: u32) -> bool { MASK.load(Ordering::Relaxed) & bit != 0 }"
    " pub fn note_vm_change(kernel: bool, _start: u64) { CHANGES[kernel as usize].fetch_add(1, Ordering::Relaxed); } }",
    "mod g17_timing_buffer { pub fn arch_counter() -> u64 { 7 } }",
    item("struct MappingValidationState {"),
    item("struct MappingHistoryEntry {"),
    item("impl MappingHistoryEntry {"),
    "#[derive(Copy, Clone, Debug, PartialEq, Eq)]",
    item("pub(crate) struct MappingChangeWalk {"),
    item("impl MappingValidationState {"),
    next(line for line in source.splitlines() if line.startswith("struct MappingValidationMutation(")),
    item("impl MappingValidationMutation {"),
    item("impl Drop for MappingValidationMutation {"),
    "const UAT_PGSZ: usize = 16384;",
    next(line for line in source.splitlines()
         if line.startswith("const MAPPING_VALIDATION_HISTORY:")),
    "#[derive(Clone, Copy)]",
    item("struct MappingValidationChange"),
    "type KBox<T> = Box<T>;",
    "struct VmInner { is_kernel: bool, mapping_validation_epoch: Option<u64>, "
    "mapping_validation: Option<Arc<MappingValidationState>>, "
    "mapping_validation_changes: Option<KBox<[Option<MappingValidationChange>; MAPPING_VALIDATION_HISTORY]>> }",
    "impl VmInner {",
    item("fn note_mapping_validation_change("),
    "}",
    "struct Lock(std::cell::RefCell<VmInner>);",
    "impl Lock { fn exec_lock(&self, _: Option<()>, _: bool) "
    "-> Result<std::cell::Ref<'_, VmInner>, ()> { Ok(self.0.borrow()) } }",
    "struct Vm { inner: Lock, mapping_validation: Option<Arc<MappingValidationState>> }",
    "impl Vm {",
    item("pub(crate) fn mapping_validation_epoch("),
    item("pub(crate) fn mapping_changes_since_counted("),
    item("pub(crate) fn mapping_changes_since_lockless("),
    "fn mapping_changes_since(&self, previous: u64, f: impl FnMut(Range<u64>) -> bool)"
    " -> Option<(u64, bool)> { self.mapping_changes_since_counted(previous, f, false)"
    ".map(|walk| (walk.epoch, walk.touched)) }",
    "}",
    r'''
fn fixture() -> Vm {
    let state = Arc::new(MappingValidationState::new());
    Vm { mapping_validation: Some(state.clone()), inner: Lock(std::cell::RefCell::new(VmInner {
        is_kernel: false,
        mapping_validation_epoch: Some(0),
        mapping_validation: Some(state),
        mapping_validation_changes: Some(Box::new([None; MAPPING_VALIDATION_HISTORY])),
    })) }
}
#[test]
fn empty_and_disjoint_changes_keep_the_proof() {
    let vm = fixture();
    assert_eq!(vm.mapping_changes_since(0, |_| true), Some((0, false)));
    vm.inner.0.borrow_mut().note_mapping_validation_change(0x10001..0x10002);
    assert_eq!(vm.mapping_changes_since(0, |range| range.start < 0x2000 && range.end > 0x1000),
        Some((1, false)));
    assert_eq!(vm.mapping_changes_since(0, |range| range.start < 0x10001 && range.end > 0x10000),
        Some((1, true)));
}
#[test]
fn failed_attempt_and_guard_mapping_both_invalidate() {
    let vm = fixture();
    // A mutation is recorded before a page-table operation that may fail.
    vm.inner.0.borrow_mut().note_mapping_validation_change(0x10001..0x10002);
    assert_eq!(vm.mapping_changes_since(0, |range| range.start < 0x10002 && range.end > 0x10001),
        Some((1, true)));
    // A TVB guard is a negative predicate; a leaf reaching its adjacent tail
    // invalidates even when the caller's byte range ended before the tail.
    assert_eq!(vm.mapping_changes_since(0, |range| range.start < 0x14000 && range.end > 0x10002),
        Some((1, true)));
}
#[test]
fn stale_or_corrupt_history_never_returns_clean() {
    let vm = fixture();
    for epoch in 0..=MAPPING_VALIDATION_HISTORY {
        let address = 1000 + epoch as u64;
        vm.inner.0.borrow_mut().note_mapping_validation_change(address..address + 1);
    }
    assert_eq!(vm.mapping_changes_since(0, |_| false), None);
    assert_eq!(vm.mapping_changes_since(1, |_| false), Some((129, false)));
    vm.inner.0.borrow_mut().mapping_validation_changes.as_mut().unwrap()[17] = None;
    assert_eq!(vm.mapping_changes_since(1, |_| false), None);
}
#[test]
fn invalid_range_or_epoch_overflow_disables_reuse() {
    let vm = fixture();
    vm.inner.0.borrow_mut().note_mapping_validation_change(5..5);
    assert_eq!(vm.mapping_changes_since(0, |_| false), None);
    let vm = fixture();
    vm.inner.0.borrow_mut().mapping_validation_epoch = Some(u64::MAX);
    assert_eq!(vm.mapping_changes_since(u64::MAX, |_| true), Some((u64::MAX, false)));
    vm.inner.0.borrow_mut().note_mapping_validation_change(1..2);
    assert_eq!(vm.mapping_changes_since(u64::MAX, |_| false), None);
}
#[test]
fn snapshot_never_takes_the_exec_lock_and_refuses_in_progress_mutations() {
    let vm=fixture();
    let mut writer=vm.inner.0.borrow_mut();
    assert_eq!(vm.mapping_validation_epoch(),Some(0));
    let change=writer.note_mapping_validation_change(0x10000..0x14000);
    // The old method panics on this second RefCell borrow. The new snapshot
    // neither waits for the writer nor claims its partial mapping is valid.
    assert_eq!(vm.mapping_validation_epoch(),None);
    drop(change);
    assert_eq!(vm.mapping_validation_epoch(),Some(1));
}
#[test]
fn nested_leaf_mutations_publish_only_after_the_outer_scope() {
    let vm=fixture();let mut writer=vm.inner.0.borrow_mut();
    let outer=writer.note_mapping_validation_change(0x10000..0x14000);
    let inner=writer.note_mapping_validation_change(0x18000..0x1c000);
    drop(inner);assert_eq!(vm.mapping_validation_epoch(),None);
    drop(outer);assert_eq!(vm.mapping_validation_epoch(),Some(2));
}
#[test]
fn partial_failure_invalidates_old_proof_and_releases_writer_scope() {
    fn attempt(writer:&mut VmInner)->Result<(),()> {
        let _change=writer.note_mapping_validation_change(0x10000..0x14000);
        Err(())
    }
    let vm=fixture();let old=vm.mapping_validation_epoch();
    assert!(attempt(&mut vm.inner.0.borrow_mut()).is_err());
    assert_ne!(old,vm.mapping_validation_epoch());
    assert_eq!(vm.mapping_validation_epoch(),Some(1));
    assert_eq!(vm.mapping_changes_since(0, |_|true),Some((1,true)));
}
#[test]
fn acquiring_a_published_epoch_observes_prior_leaf_writes() {
    let state=Arc::new(MappingValidationState::new());
    let leaf=Arc::new(AtomicU64::new(0));
    let writer_state=state.clone();let writer_leaf=leaf.clone();
    let writer=std::thread::spawn(move||{
        for epoch in 1..10000 {
            let _change=MappingValidationMutation::new(Some(writer_state.clone()));
            writer_state.record(Some(epoch));
            writer_leaf.store(epoch,Ordering::Relaxed);
        }
    });
    while !writer.is_finished() {
        if let Some(epoch)=state.snapshot() {
            assert!(leaf.load(Ordering::Relaxed)>=epoch);
        }
    }
    writer.join().unwrap();assert_eq!(state.snapshot(),Some(9999));
}
// ---- v94 REACH_LOCKLESS -------------------------------------------------
// The mask is process-global; every test below that needs it holds this lock.
static MASK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn with_mask<R>(mask: u32, f: impl FnOnce() -> R) -> R {
    let _guard = MASK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    g17_mem_path::MASK.store(mask, Ordering::SeqCst);
    let result = f();
    g17_mem_path::MASK.store(0, Ordering::SeqCst);
    result
}
fn change(vm: &Vm, start: u64) { drop(vm.inner.0.borrow_mut().note_mapping_validation_change(start..start + 1)); }
#[test]
fn lockless_walk_equals_locked_walk() {
    with_mask(g17_mem_path::REACH_LOCKLESS, || {
        let vm = fixture();
        for n in 0..40u64 { change(&vm, 0x100000 + n * 0x10000); }
        let current = vm.mapping_validation_epoch().unwrap();
        assert_eq!(current, 40);
        for previous in [0u64, 1, 17, 39, 40] {
            for hit in [None, Some(0x100000 + 25 * 0x10000u64)] {
                let f = |range: Range<u64>| hit.is_some_and(|h| range.start <= h && h < range.end);
                let locked = vm.mapping_changes_since_counted(previous, f, false).unwrap();
                let free = vm.mapping_changes_since_lockless(previous, current, f).unwrap();
                assert_eq!(locked, free, "previous {previous} hit {hit:?}");
            }
        }
    });
}
#[test]
fn lockless_is_off_by_default_and_refuses_unrecorded_history() {
    let vm = fixture();
    with_mask(0, || {
        change(&vm, 0x10000);
        assert_eq!(vm.mapping_changes_since_lockless(0, 1, |_| false), None);
    });
    with_mask(g17_mem_path::REACH_LOCKLESS, || {
        // Epoch 1 was recorded while the bit was clear: refuse, never clean.
        assert_eq!(vm.mapping_changes_since_lockless(0, 1, |_| false), None);
        change(&vm, 0x20000);
        assert_eq!(vm.mapping_changes_since_lockless(0, 2, |_| false), None);
        let walk = vm.mapping_changes_since_lockless(1, 2, |_| false).unwrap();
        assert_eq!((walk.epoch, walk.touched, walk.walked, walk.locked_at), (2, false, 1, 0));
        // The locked walk is unchanged by the bit.
        assert_eq!(vm.mapping_changes_since(0, |_| false), Some((2, false)));
    });
}
#[test]
fn lockless_refuses_writers_lost_history_and_wrong_current() {
    with_mask(g17_mem_path::REACH_LOCKLESS, || {
        let vm = fixture();
        change(&vm, 0x10000);
        assert!(vm.mapping_changes_since_lockless(0, 1, |_| false).is_some());
        // A caller-supplied epoch that is not the visible one is refused.
        assert_eq!(vm.mapping_changes_since_lockless(0, 2, |_| false), None);
        assert_eq!(vm.mapping_changes_since_lockless(0, 0, |_| false), None);
        {
            let mut writer = vm.inner.0.borrow_mut();
            let guard = writer.note_mapping_validation_change(0x20000..0x24000);
            assert_eq!(vm.mapping_changes_since_lockless(0, 1, |_| false), None);
            drop(guard);
        }
        assert!(vm.mapping_changes_since_lockless(0, 2, |_| false).is_some());
        for n in 0..MAPPING_VALIDATION_HISTORY as u64 { change(&vm, 0x40000 + n * 0x4000); }
        let current = vm.mapping_validation_epoch().unwrap();
        assert_eq!(vm.mapping_changes_since_lockless(current - 129, current, |_| false), None);
        assert!(vm.mapping_changes_since_lockless(current - 128, current, |_| false).is_some());
        // A torn/stale entry (epoch mismatch) is refused.
        let state = vm.mapping_validation.as_ref().unwrap();
        state.history[(current as usize - 3) % MAPPING_VALIDATION_HISTORY].epoch.store(u64::MAX, Ordering::SeqCst);
        assert_eq!(vm.mapping_changes_since_lockless(current - 5, current, |_| false), None);
        assert!(vm.mapping_changes_since_lockless(current - 2, current, |_| false).is_some());
        // Overflowed epochs refuse permanently.
        vm.inner.0.borrow_mut().mapping_validation_epoch = Some(u64::MAX);
        change(&vm, 0x10000);
        assert_eq!(vm.mapping_validation_epoch(), None);
        assert_eq!(vm.mapping_changes_since_lockless(current, current, |_| false), None);
    });
}
#[test]
fn lockless_touch_stops_the_walk_like_the_locked_walk() {
    with_mask(g17_mem_path::REACH_LOCKLESS, || {
        let vm = fixture();
        for n in 0..10u64 { change(&vm, 0x100000 + n * 0x10000); }
        let f = |range: Range<u64>| range.start == 0x100000 + 3 * 0x10000;
        let walk = vm.mapping_changes_since_lockless(0, 10, f).unwrap();
        assert_eq!((walk.touched, walk.walked), (true, 4));
        assert_eq!(vm.mapping_changes_since_counted(0, f, false).unwrap().walked, 4);
    });
}
#[test]
fn mem_stats_counts_changes_per_vm_class() {
    with_mask(g17_mem_path::MEM_STATS, || {
        let before = g17_mem_path::CHANGES[0].load(Ordering::SeqCst);
        let vm = fixture();
        change(&vm, 0x10000);
        change(&vm, 0x20000);
        assert_eq!(g17_mem_path::CHANGES[0].load(Ordering::SeqCst), before + 2);
    });
}
#[test]
fn lockless_reader_never_accepts_a_torn_or_foreign_range() {
    // Seqlock stress: one writer (the exec-lock holder) records changes whose
    // range encodes their epoch; readers walk concurrently without any lock.
    // Every accepted walk must have seen exactly epoch-matching ranges.
    with_mask(g17_mem_path::REACH_LOCKLESS, || {
        let state = Arc::new(MappingValidationState::new());
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer_state = state.clone();
        let writer = std::thread::spawn(move || {
            let mut inner = VmInner { is_kernel: false, mapping_validation_epoch: Some(0),
                mapping_validation: Some(writer_state),
                mapping_validation_changes: Some(Box::new([None; MAPPING_VALIDATION_HISTORY])) };
            for epoch in 1..200_000u64 {
                let start = epoch * UAT_PGSZ as u64;
                drop(inner.note_mapping_validation_change(start..start + 1));
            }
        });
        let readers: Vec<_> = (0..3).map(|n| {
            let state = state.clone(); let done = done.clone();
            std::thread::spawn(move || {
                let (mut accepted, mut refused) = (0u64, 0u64);
                while !done.load(Ordering::Relaxed) {
                    let Some(current) = state.snapshot() else { refused += 1; continue; };
                    let previous = current.saturating_sub(1 + (current + n) % 128);
                    let mut expected = previous + 1;
                    let walk = state.changes_since(previous, current, |range| {
                        let ok = range.start == expected * UAT_PGSZ as u64 && range.end == range.start + UAT_PGSZ as u64;
                        expected += 1;
                        !ok // a foreign range reports "touched"; an accepted clean walk had none
                    });
                    match walk {
                        Some(walk) => { assert!(!walk.touched, "accepted a torn range"); accepted += 1; }
                        None => refused += 1,
                    }
                }
                (accepted, refused)
            })
        }).collect();
        writer.join().unwrap();
        done.store(true, Ordering::Relaxed);
        let mut total = 0;
        for reader in readers { let (accepted, _) = reader.join().unwrap(); total += accepted; }
        assert!(total > 0);
    });
}
''',
])

# All seven mutation entry points retain the invalidation across actual leaf
# work. map_io explicitly unlocks on two exits, so both release the
# version guard first; ordinary lexical exits drop guards before exec guards.
assert source.count('let _validation_change = ') == 6
assert source.count('let validation_change = ') == 1
assert source.count('drop(validation_change);') == 2
assert 'drop(validation_change);\n            core::mem::drop(inner);' in source
assert 'drop(validation_change);\n        core::mem::drop(inner);' in source

with tempfile.TemporaryDirectory(prefix="g17p-mapping-journal-") as directory:
    path = Path(directory)
    (path / "journal.rs").write_text(program)
    subprocess.run(["rustc", "--edition=2021", "--test", str(path / "journal.rs"),
                    "-o", str(path / "journal-tests")], check=True)
    subprocess.run([str(path / "journal-tests"), "--test-threads=1"], check=True)  # shared MEM_STATS counters
