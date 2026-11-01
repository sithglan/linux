#!/usr/bin/env python3
"""Compile the production cache and its unit tests without kernel-only resources."""

from pathlib import Path
import subprocess
import tempfile


source = (Path(__file__).resolve().parents[1] / "g17_resources.rs").read_text()


def item(prefix):
    start = source.index(prefix)
    opening = source.index("{", start)
    depth = 1
    end = opening + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


tests = [
    "retained_reachability_keeps_all_command_generations",
    "retained_reachability_rejects_each_changed_vm_input",
    "retained_reachability_does_not_cache_overlapping_or_unavailable_epochs",
    "retained_reachability_topology_invalidation_clears_all_generations",
]
constant = next(line for line in source.splitlines()
                if line.startswith("pub(crate) const G17P_RENDER_MAX_IN_FLIGHT:"))
program = "\n".join([
    "use core::ops::Range;",
    # v94 diagnostics and the opt-in REACH_LOCKLESS path (g17p_mem_path bit3).
    """mod g17_mem_path {
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    pub const MEM_STATS: u32 = 1 << 1; pub const REACH_LOCKLESS: u32 = 1 << 3;
    pub static MASK: AtomicU32 = AtomicU32::new(0);
    pub fn enabled(bit: u32) -> bool { MASK.load(Ordering::Relaxed) & bit != 0 }
    pub fn stamp() -> u64 { if enabled(MEM_STATS) { 5 } else { 0 } }
    pub struct Stat { pub count: AtomicU64 }
    impl Stat { pub const fn new() -> Self { Self { count: AtomicU64::new(0) } }
        pub fn add(&self, _: u64) { self.count.fetch_add(1, Ordering::Relaxed); } }
    pub fn count(counter: &AtomicU64, value: u64) {
        if enabled(MEM_STATS) { counter.fetch_add(value, Ordering::Relaxed); } }
    pub static REACH_EXACT: Stat = Stat::new(); pub static REACH_LOCKLESS_WALK: Stat = Stat::new();
    pub static REACH_LOCKED_WALK: Stat = Stat::new(); pub static REACH_FULL: Stat = Stat::new();
    pub static REACH_WALKED: AtomicU64 = AtomicU64::new(0);
    pub static REACH_FULL_NO_PROOF: AtomicU64 = AtomicU64::new(0);
    pub static REACH_FULL_NO_KEY: AtomicU64 = AtomicU64::new(0);
    pub static REACH_FULL_REFUSED: AtomicU64 = AtomicU64::new(0);
    pub static REACH_FULL_TOUCHED: AtomicU64 = AtomicU64::new(0);
}
mod g17_e2e {
    use std::sync::Mutex;
    pub const OPT_FINE: u32 = 1 << 4; pub const K_FINE_REACH: u8 = 32;
    pub static ARMED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    pub static RECORDS: Mutex<Vec<(u8, u32, u64, [u64; 5])>> = Mutex::new(Vec::new());
    pub fn option(bit: u32) -> bool { ARMED.load(core::sync::atomic::Ordering::Relaxed) & bit != 0 }
    pub fn fine() -> u64 { if option(OPT_FINE) { 3 } else { 0 } }
    pub fn record_at(_: u64, kind: u8, aux: u32, id: u64, payload: [u64; 5]) {
        RECORDS.lock().unwrap().push((kind, aux, id, payload)); }
}
mod g17_timing_buffer { pub fn arch_counter() -> u64 { 9 } }""",
    constant,
    "#[derive(Debug, Copy, Clone, PartialEq, Eq)]",
    item("struct G17PRenderReachabilityKey"),
    item("struct G17PRenderReachabilityCache"),
    item("impl G17PRenderReachabilityCache"),
    item("fn reachability_test_key"),
    "#[derive(Default)]",
    item("struct G17PReachTrace"),
    item("impl G17PReachTrace"),
    *("#[test]\n" + item("fn " + name) for name in tests),
])

# Exercise the actual wrapper with page-table epochs and full-walk work replaced
# by deterministic mocks. The wrapper itself is extracted unchanged above.
program += r'''
mod mmu {
    use core::cell::Cell;
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct MappingChangeWalk { pub epoch: u64, pub touched: bool, pub walked: u32, pub locked_at: u64 }
    pub struct Vm {
        pub id: u64,
        pub epoch: Cell<Option<u64>>,
        pub reads: Cell<u64>,
        pub mutate_on_read: Cell<Option<u64>>,
        pub mutation_intersects: Cell<bool>,
        /// v94: reads of the lock-free epoch that still see a writer (None).
        pub hidden_reads: Cell<u32>,
        /// v94: the lock-free history refuses (e.g. not yet recorded).
        pub lockless_refuses: Cell<bool>,
        pub locked_walks: Cell<u32>,
        pub lockless_walks: Cell<u32>,
        /// v94: a writer commits right after the locked walk returns.
        pub bump_after_locked_walk: Cell<bool>,
    }
    impl Vm {
        pub fn new(id: u64) -> Self {
            Self { id, epoch: Cell::new(Some(1)), reads: Cell::new(0),
                   mutate_on_read: Cell::new(None), mutation_intersects: Cell::new(true),
                   hidden_reads: Cell::new(0), lockless_refuses: Cell::new(false),
                   locked_walks: Cell::new(0), lockless_walks: Cell::new(0),
                   bump_after_locked_walk: Cell::new(false) }
        }
        pub fn mapping_changes_since_counted(
            &self, previous: u64, intersects: impl FnMut(core::ops::Range<u64>) -> bool, timed: bool,
        ) -> Option<MappingChangeWalk> {
            self.locked_walks.set(self.locked_walks.get() + 1);
            // The exec lock waits out a writer: the history is complete here.
            self.hidden_reads.set(0);
            let walk = self.mapping_changes_since(previous, intersects).map(|(epoch, touched)| MappingChangeWalk {
                epoch, touched, walked: (epoch - previous) as u32, locked_at: if timed { 11 } else { 0 } });
            if self.bump_after_locked_walk.get() { self.bump_unrelated(); }
            walk
        }
        pub fn mapping_changes_since_lockless(
            &self, previous: u64, current: u64, intersects: impl FnMut(core::ops::Range<u64>) -> bool,
        ) -> Option<MappingChangeWalk> {
            if !crate::g17_mem_path::enabled(crate::g17_mem_path::REACH_LOCKLESS) || self.lockless_refuses.get() {
                return None;
            }
            self.lockless_walks.set(self.lockless_walks.get() + 1);
            if self.epoch.get() != Some(current) { return None; }
            self.mapping_changes_since(previous, intersects).map(|(epoch, touched)| MappingChangeWalk {
                epoch, touched, walked: (epoch - previous) as u32, locked_at: 0 })
        }
        pub fn id(&self) -> u64 { self.id }
        pub fn mapping_validation_epoch(&self) -> Option<u64> {
            self.reads.set(self.reads.get() + 1);
            if self.mutate_on_read.get() == Some(self.reads.get()) { self.bump(); }
            if self.hidden_reads.get() > 0 {
                self.hidden_reads.set(self.hidden_reads.get() - 1);
                return None;
            }
            self.epoch.get()
        }
        pub fn bump(&self) { self.epoch.set(self.epoch.get().and_then(|v| v.checked_add(1))); }
        pub fn bump_unrelated(&self) { self.mutation_intersects.set(false); self.bump(); }
        pub fn mapping_changes_since(
            &self, previous: u64, mut intersects: impl FnMut(core::ops::Range<u64>) -> bool,
        ) -> Option<(u64, bool)> {
            let now = self.epoch.get()?;
            Some((now, now != previous && intersects(if self.mutation_intersects.get() {
                1..2
            } else {
                100..101
            })))
        }
    }
    pub struct Uat { pub kernel: Vm }
    impl Uat { pub fn kernel_vm(&self) -> &Vm { &self.kernel } }
}
struct Descriptors;
impl Descriptors { fn iova(&self) -> u64 { 0x4000 } }
struct G17PUserRenderStorage {
    descriptors: Descriptors,
    render_command_generation: u8,
    reachability_cache: G17PRenderReachabilityCache,
    last_reachability_cache_hit: bool,
    walks: usize,
    fail: bool,
    mutate_during_walk: bool,
}
impl G17PUserRenderStorage {
    fn new() -> Self {
        Self { descriptors: Descriptors, render_command_generation: 0,
               reachability_cache: G17PRenderReachabilityCache::new(),
               last_reachability_cache_hit: false,
               walks: 0, fail: false, mutate_during_walk: false }
    }
    fn first_unreachable(&mut self, _: &mmu::Uat, vm: &mmu::Vm) -> Option<&'static str> {
        self.walks += 1;
        if self.mutate_during_walk { vm.bump(); }
        if self.fail { Some("missing-block") } else { None }
    }
    fn mapping_change_touches_graph(&self, range: Range<u64>, _: bool) -> bool {
        range.start == 1
    }
'''
program += item("fn reachability_key(") + "\n"
program += item("pub(crate) fn first_unreachable_cached(") + "\n"
program += item("fn finish_reach_trace(") + "\n}\n"
program += r'''
fn fixture() -> (G17PUserRenderStorage, mmu::Uat, mmu::Vm) {
    (G17PUserRenderStorage::new(), mmu::Uat { kernel: mmu::Vm::new(1) }, mmu::Vm::new(2))
}
#[test]
fn wrapper_skips_walk_only_for_matching_vm_epochs_and_generation() {
    let (mut storage, uat, vm) = fixture();
    assert_eq!(storage.first_unreachable_cached(&uat, &vm), None);
    assert_eq!(storage.walks, 1);
    assert_eq!(storage.first_unreachable_cached(&uat, &vm), None);
    assert_eq!(storage.walks, 1);
    assert!(storage.last_reachability_cache_hit);
    storage.render_command_generation = 1;
    storage.first_unreachable_cached(&uat, &vm);
    assert_eq!(storage.walks, 2);
    assert!(!storage.last_reachability_cache_hit);
    storage.render_command_generation = 0;
    storage.first_unreachable_cached(&uat, &vm);
    assert_eq!(storage.walks, 2);
    vm.bump();
    storage.first_unreachable_cached(&uat, &vm);
    assert_eq!(storage.walks, 3);
    uat.kernel.bump();
    storage.first_unreachable_cached(&uat, &vm);
    assert_eq!(storage.walks, 4);
    let other_vm = mmu::Vm::new(3);
    other_vm.epoch.set(vm.epoch.get());
    storage.first_unreachable_cached(&uat, &other_vm);
    assert_eq!(storage.walks, 5);
}
#[test]
fn wrapper_keeps_proof_across_unrelated_app_and_kernel_maps() {
    let (mut storage, uat, vm) = fixture();
    storage.first_unreachable_cached(&uat, &vm);
    vm.bump_unrelated();
    storage.first_unreachable_cached(&uat, &vm);
    assert_eq!(storage.walks, 1);
    assert!(storage.last_reachability_cache_hit);
    uat.kernel.bump_unrelated();
    storage.first_unreachable_cached(&uat, &vm);
    assert_eq!(storage.walks, 1);
    vm.mutation_intersects.set(true);
    vm.bump();
    storage.first_unreachable_cached(&uat, &vm);
    assert_eq!(storage.walks, 2);
}
#[test]
fn wrapper_never_caches_failed_or_overlapping_walks() {
    let (mut storage, uat, vm) = fixture();
    storage.fail = true;
    for walks in 1..=2 {
        assert_eq!(storage.first_unreachable_cached(&uat, &vm), Some("missing-block"));
        assert_eq!(storage.walks, walks);
    }
    storage.fail = false;
    storage.mutate_during_walk = true;
    assert_eq!(storage.first_unreachable_cached(&uat, &vm), None);
    assert_eq!(storage.walks, 3);
    storage.mutate_during_walk = false;
    storage.first_unreachable_cached(&uat, &vm);
    assert_eq!(storage.walks, 4);
    storage.first_unreachable_cached(&uat, &vm);
    assert_eq!(storage.walks, 4);
}
#[test]
fn wrapper_double_collect_rejects_hit_race_and_unavailable_epochs() {
    let (mut storage, uat, vm) = fixture();
    storage.first_unreachable_cached(&uat, &vm);
    vm.mutate_on_read.set(Some(vm.reads.get() + 2));
    storage.first_unreachable_cached(&uat, &vm);
    assert_eq!(storage.walks, 2);
    // The miss overlapped the epoch change, so its success was not cached.
    storage.first_unreachable_cached(&uat, &vm);
    assert_eq!(storage.walks, 3);
    storage.first_unreachable_cached(&uat, &vm);
    assert_eq!(storage.walks, 3);
    vm.epoch.set(None);
    for walks in 4..=5 {
        storage.first_unreachable_cached(&uat, &vm);
        assert_eq!(storage.walks, walks);
    }
}
// ---- v94 REACH_LOCKLESS (bit3) and diagnostics --------------------------
static MASK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn with_mask<R>(mask: u32, armed: u32, f: impl FnOnce() -> R) -> R {
    use core::sync::atomic::Ordering;
    let _guard = MASK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    g17_mem_path::MASK.store(mask, Ordering::SeqCst);
    g17_e2e::ARMED.store(armed, Ordering::SeqCst);
    g17_e2e::RECORDS.lock().unwrap().clear();
    let result = f();
    g17_mem_path::MASK.store(0, Ordering::SeqCst);
    g17_e2e::ARMED.store(0, Ordering::SeqCst);
    result
}
#[test]
fn bit_clear_writer_in_progress_still_forces_the_full_walk() {
    with_mask(0, 0, || {
        let (mut storage, uat, vm) = fixture();
        storage.first_unreachable_cached(&uat, &vm);
        vm.bump_unrelated();
        vm.hidden_reads.set(1);
        storage.first_unreachable_cached(&uat, &vm);
        assert_eq!(storage.walks, 2);
        assert_eq!(vm.lockless_walks.get(), 0);
    });
}
#[test]
fn lockless_serves_unrelated_changes_without_the_locked_walk() {
    with_mask(g17_mem_path::REACH_LOCKLESS, 0, || {
        let (mut storage, uat, vm) = fixture();
        storage.first_unreachable_cached(&uat, &vm);
        vm.bump_unrelated();
        uat.kernel.bump_unrelated();
        assert_eq!(storage.first_unreachable_cached(&uat, &vm), None);
        assert!(storage.last_reachability_cache_hit);
        assert_eq!((storage.walks, vm.locked_walks.get(), uat.kernel.locked_walks.get()), (1, 0, 0));
        assert_eq!((vm.lockless_walks.get(), uat.kernel.lockless_walks.get()), (1, 1));
        // The new proof is cached: the next publication is an exact hit.
        storage.first_unreachable_cached(&uat, &vm);
        assert_eq!(vm.lockless_walks.get(), 1);
        // A touching change still forces the full walk, with no locked walk.
        vm.mutation_intersects.set(true);
        vm.bump();
        storage.first_unreachable_cached(&uat, &vm);
        assert_eq!((storage.walks, vm.locked_walks.get()), (2, 0));
    });
}
#[test]
fn lockless_refusal_falls_back_to_the_locked_walk() {
    with_mask(g17_mem_path::REACH_LOCKLESS, 0, || {
        let (mut storage, uat, vm) = fixture();
        storage.first_unreachable_cached(&uat, &vm);
        vm.lockless_refuses.set(true);
        vm.bump_unrelated();
        assert_eq!(storage.first_unreachable_cached(&uat, &vm), None);
        assert!(storage.last_reachability_cache_hit);
        assert_eq!((storage.walks, vm.locked_walks.get()), (1, 1));
    });
}
#[test]
fn lockless_serves_a_writer_in_progress_with_the_locked_walk() {
    with_mask(g17_mem_path::REACH_LOCKLESS, 0, || {
        let (mut storage, uat, vm) = fixture();
        storage.first_unreachable_cached(&uat, &vm);
        vm.bump_unrelated();
        vm.hidden_reads.set(1);
        assert_eq!(storage.first_unreachable_cached(&uat, &vm), None);
        assert_eq!((storage.walks, vm.locked_walks.get()), (1, 1));
        // The reached epochs were cached.
        storage.first_unreachable_cached(&uat, &vm);
        assert_eq!((storage.walks, vm.locked_walks.get()), (1, 1));
    });
}
#[test]
fn lockless_locked_walk_rejects_a_key_that_moved_after_the_walk() {
    with_mask(g17_mem_path::REACH_LOCKLESS, 0, || {
        let (mut storage, uat, vm) = fixture();
        storage.first_unreachable_cached(&uat, &vm);
        vm.bump_unrelated();
        vm.hidden_reads.set(1);
        vm.bump_after_locked_walk.set(true);
        assert_eq!(storage.first_unreachable_cached(&uat, &vm), None);
        assert_eq!(storage.walks, 2);
        assert!(!storage.last_reachability_cache_hit);
    });
}
#[test]
fn lockless_never_caches_a_failed_walk() {
    with_mask(g17_mem_path::REACH_LOCKLESS, 0, || {
        let (mut storage, uat, vm) = fixture();
        storage.fail = true;
        assert_eq!(storage.first_unreachable_cached(&uat, &vm), Some("missing-block"));
        vm.bump_unrelated();
        assert_eq!(storage.first_unreachable_cached(&uat, &vm), Some("missing-block"));
        assert_eq!((storage.walks, vm.lockless_walks.get(), vm.locked_walks.get()), (2, 0, 0));
    });
}
#[test]
fn fine_trace_records_path_and_reason_without_changing_decisions() {
    use core::sync::atomic::Ordering;
    let records = with_mask(g17_mem_path::MEM_STATS, g17_e2e::OPT_FINE | 1, || {
        let (mut storage, uat, vm) = fixture();
        storage.first_unreachable_cached(&uat, &vm);      // full, no proof
        storage.first_unreachable_cached(&uat, &vm);      // exact
        vm.bump_unrelated();
        storage.first_unreachable_cached(&uat, &vm);      // locked walk (bit3 clear)
        vm.mutation_intersects.set(true);
        vm.bump();
        storage.first_unreachable_cached(&uat, &vm);      // full, touched
        assert_eq!(storage.walks, 2);
        assert_eq!(g17_mem_path::REACH_FULL_TOUCHED.load(Ordering::SeqCst), 1);
        g17_e2e::RECORDS.lock().unwrap().clone()
    });
    let paths: Vec<_> = records.iter().map(|r| (r.0, r.1 & 0xff, r.2)).collect();
    assert_eq!(paths, vec![(32, 0x13, 0x4000), (32, 0x00, 0x4000), (32, 0x02, 0x4000), (32, 0x43, 0x4000)]);
    // The locked walk reports one change per VM and its lock timestamps.
    assert_eq!(records[2].3[1], 1 | 0 << 32);
    assert_eq!(records[2].3[2], 11);
}
'''

with tempfile.TemporaryDirectory(prefix="g17p-reachability-") as directory:
    path = Path(directory)
    (path / "cache.rs").write_text(program)
    subprocess.run(["rustc", "--edition=2021", "--test", str(path / "cache.rs"),
                    "-o", str(path / "cache-tests")], check=True)
    subprocess.run([str(path / "cache-tests")], check=True)
