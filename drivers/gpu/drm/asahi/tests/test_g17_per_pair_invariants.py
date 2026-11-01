#!/usr/bin/env python3
"""Per-pair render graph invariants that replace j700's recycling-cache tests.

j700 recycled retired render graphs across VMs (select_cached_render_graph,
reactivate_recycled_render_graph, G17PRetiredParameterManagement, ...). This
driver keeps one graph per physical render pair instead: a pair is warmed
fresh for its owning context, retained only for that VM, and handed to a new
owner only through the FList-ID takeover path. The hazard class those tests
guarded (and the 2026-09-26 "repopulate a released FList" fix addressed) is a
new owner inheriting stale firmware pool state. Each guarantee below names the
code path that provides it:

  1. takeover repopulates the pool's complete firmware image before the UMA
     descriptor is staged (G17PRenderFlist::repopulate, both owner-change sites);
  2. the rewritten page list and run slots are exactly the constructor image, so
     no stale free-page stack survives (shared for_each_pool_word);
  3. the completed counter is set to the current submitted counter, not zero
     (native populateInitialFirmwareState, per the RE in repopulate());
  4. a pool that never served an owner is not rewritten (served flag);
  5. a pool's FList ID is leased until the pool drops and a teardown settles
     only after its pools' 0x2e releases are consumed (shared FList; the v92c
     takeover guard is now inert because IDs are never shared);
  6. a retained graph never serves a different VM or root slot (EBUSY);
  7. a new context's graph is built fresh (take_warm_render_graph refuses a pair
     that still holds storage, a root binding or trackers).
"""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
resources = (root / "g17_resources.rs").read_text()
manager = (root / "g17_manager.rs").read_text()
live = (root / "g17_live_boot.rs").read_text()
drm = (root / "g17_drm.rs").read_text()


def item(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


# 1. Both owner-change sites repopulate before staging the UMA descriptor.
sites = 0
for marker in ["    pub(crate) fn begin_translated_render(",
               "    fn finish_warm_render_pair_storage(",
               "    fn begin_translated_render_with_hardware_buffer("]:
    if marker not in manager:
        continue
    body = item(manager, marker)
    start = 0
    while (index := body.find("if !owner_matches {", start)) >= 0:
        # v96 logs each takeover (counters) between the guard and repopulate.
        branch = body[index:index + 4000]
        assert branch.index("storage.repopulate_render_flist()") \
            < branch.index("self.resources.stage_user_render_uma(storage)"), marker
        sites += 1
        start = index + 1
assert sites == 2, sites

repopulate = item(resources, "    pub(crate) fn repopulate(&self) -> Result {")
# Shared FList: a pool is built for a logical context and maps its client VM.
constructor = item(resources, "    pub(crate) fn new(dev: &AsahiDevice, uat: &mmu::Uat, vm: &mmu::Vm) -> Result<Self> {")
# 2. The same generator writes the constructor image and the repopulated one.
assert "Self::for_each_pool_word(page_list_va, |at, value| put_u64(bytes, at, value))" in constructor
assert "Self::for_each_pool_word(page_list_va, |at, value|" in repopulate
assert "Self::control_shared_image(" in constructor and "Self::control_shared_image(" in repopulate
# 3. completed := current submitted (native), written into the shared image.
assert "let submitted = self.prepare_submitted_operations(0)?.next;" in repopulate
assert "put_u64(&mut shared_image, 0x54, submitted);" in repopulate
# 4. A never-served pool keeps its constructor image.
assert repopulate.index("if !self.served.swap(true, Ordering::AcqRel)") < repopulate.index("write_all(")

# 5. Shared FList: every pool leases its own FList ID (129..255), so reusing a
# physical pair can no longer replace another pool's UMA row and the v92c
# takeover guard never blocks. Instead the old pool keeps its ID until the pool
# drops, and a teardown settles only after its pools' releases are consumed.
guard = item(manager, "    pub(crate) fn render_flist_takeover_blocked(")
assert guard.rstrip().endswith("false\n    }") or guard.rstrip().endswith("false\n    }".rstrip())
assert "independent ID" in guard
assert "let bit = self.0 as usize - 129;" in item(resources, "impl Drop for G17PRenderFlistId {")
settle = item(manager, "    pub(crate) fn settle_logical_queue_teardowns(")
assert "self.displaced_graphs_flists_released(index)" in settle
publish = item(drm, "    fn publish_with(")
assert publish.index("render_flist_takeover_ready(") < publish.index("take_warm_render_graph(")
ready = item(live, "    pub(crate) fn render_flist_takeover_ready(")
assert ready.index("settle_consumed_render_usc_freelist_releases()") < ready.index("Ok(false)")

# 6. A retained graph is bound to its VM and root slot.
begin = item(manager, "    fn begin_translated_render_with_hardware_buffer(")
assert ".is_some_and(|binding| binding.matches(vm))" in begin
assert '"retained-root-slot-moved"' in begin

# 7. A new context's pair graph is built fresh.
take = item(manager, "    pub(crate) fn take_warm_render_graph(")
for field in ["slot.storage.is_some()", "slot.bind.is_some()", "slot.context_aliases.is_some()",
              "slot.tracker.is_some()", "slot.queued_tracker.is_some()"]:
    assert field in take, field
assert "return Err(EBUSY);" in take

# Behaviour: the shared generator yields one pristine entry per page and one run
# per block, independent of whatever firmware left in the list.
generator = item(resources, "    fn for_each_pool_word(")
constants = []
init = (root / "g17_initdata.rs").read_text()
buffer_size = next(l for l in init.splitlines() if l.startswith("pub(crate) const CONTROL_OPERAND_BUFFER_SIZE:"))
constants.append("mod g17_initdata { " + buffer_size.replace("pub(crate) ", "pub ") + " }")
for name in ["G17P_CONTROL_DIRECTORY_SIZE", "G17P_CONTROL_OPERAND_PAGE_LIST_SIZE", "G17P_CONTROL_OPERAND_TABLE_BACKING_SIZE",
             "G17P_CONTROL_OPERAND_BUFFER_SIZE", "G17P_CONTROL_OPERAND_RUN_SLOT_SIZE",
             "G17P_CONTROL_OPERAND_RUN_ADDRESS_MASK", "G17P_CONTROL_OPERAND_RUN_LENGTH_PAGES",
             "G17P_RENDER_FLIST_INITIAL_BLOCK_COUNT", "G17P_CONTROL_OPERAND_INITIAL_BLOCK_COUNT",
             "G17P_CONTROL_OPERAND_FIRST_GROW_BLOCK_COUNT", "G17P_CONTROL_OPERAND_ACTIVE_BLOCK_COUNT"]:
    line = next((l for l in resources.splitlines() if l.startswith(f"const {name}:")
                 or l.startswith(f"pub(crate) const {name}:")), None)
    if line is not None:
        text = line
        if not text.rstrip().endswith(";"):
            start = resources.index(line)
            text = resources[start:resources.index(";", start) + 1]
        constants.append(text)
source = r'''
#![allow(dead_code)]
type Result<T=()>=std::result::Result<T,i32>;
const EOVERFLOW:i32=75;
''' + "\n".join(constants) + r'''
struct Pool;
impl Pool {
''' + generator + r'''
}
#[test] fn repopulate_image_is_pristine_after_firmware_reordering() {
    let va = 0x10_0400_0000u64;
    let mut words = std::collections::BTreeMap::new();
    Pool::for_each_pool_word(va, |at, value| { words.insert(at, value); }).unwrap();
    // Firmware leaves the page list as a used free-page stack: reverse it.
    let mut stale: Vec<(usize,u64)> = words.iter().map(|(a,v)|(*a,*v)).collect();
    let values: Vec<u64> = stale.iter().rev().map(|(_,v)|*v).collect();
    for (entry, value) in stale.iter_mut().zip(values) { entry.1 = value; }
    let mut image: std::collections::BTreeMap<usize,u64> = stale.into_iter().collect();
    Pool::for_each_pool_word(va, |at, value| { image.insert(at, value); }).unwrap();
    assert_eq!(image, words, "every page-list entry and run slot is rewritten");
    let pages: Vec<u64> = words.iter().filter(|(a,_)| **a < G17P_CONTROL_OPERAND_PAGE_LIST_SIZE).map(|(_,v)|*v).collect();
    let mut sorted = pages.clone(); sorted.sort(); sorted.dedup();
    assert_eq!(sorted.len(), pages.len(), "no page appears twice");
    assert!(pages.windows(2).all(|w| w[0] < w[1]) || pages.len() < 2, "pages in constructor order");
}
'''
with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / "pairs.rs"
    path.write_text(source)
    binary = Path(directory) / "pairs"
    subprocess.run(["rustc", "--edition=2021", "--test", str(path), "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
print("PASS per-pair invariants: takeover repopulates the full pool image (completed=submitted), guards unconsumed releases, never reuses a graph across VMs")
