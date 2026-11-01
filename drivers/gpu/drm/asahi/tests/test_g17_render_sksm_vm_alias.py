#!/usr/bin/env python3
"""A returning VM reuses the installed render QID's exact context aliases."""

from pathlib import Path


root = Path(__file__).resolve().parents[1]
resources = (root / "g17_resources.rs").read_text()
manager = (root / "g17_manager.rs").read_text()


def function(marker):
    start = resources.index(marker)
    pos = resources.index("{", start) + 1
    depth = 1
    while depth:
        depth += (resources[pos] == "{") - (resources[pos] == "}")
        pos += 1
    return resources[start:pos]


def function_at(start):
    pos = resources.index("{", start) + 1
    depth = 1
    while depth:
        depth += (resources[pos] == "{") - (resources[pos] == "}")
        pos += 1
    return resources[start:pos]


storage = resources[
    resources.index("pub(crate) struct G17PUserRenderStorage {") :
    resources.index("enum G17PRenderCommandContext")
]
prepare = function("fn prepare_render_sksm_vm_alias(")
new = function_at(resources.index("    fn new(", resources.index("impl G17PUserRenderStorage")))
finish = function("pub(crate) fn finish_user_render_storage(")
handoff_start = manager.index("pub(crate) fn handoff_retained_render_vm(")
handoff_pos = manager.index("{", handoff_start) + 1
handoff_depth = 1
while handoff_depth:
    handoff_depth += (manager[handoff_pos] == "{") - (manager[handoff_pos] == "}")
    handoff_pos += 1
handoff = manager[handoff_start:handoff_pos]

assert "ta_context_low: G17PRenderClientAlias" in storage
assert "fragment_context_low: G17PRenderClientAlias" in storage
assert "sksm_vm_aliases: KVec<mmu::KernelMapping>" in storage
assert "existing_pages != expected_pages" in prepare
assert "let client_pa = vm.translate_iova(low)?;" in prepare
assert "let backing_pa = uat.kernel_vm().translate_iova(high)?;" in prepare
assert "if client_pa != backing_pa" in prepare
assert "G17PRenderClientAlias::from_range(address, size), None" in prepare
assert "object.map_alias_at(vm, address, prot)?" in prepare
assert new.count("prepare_render_sksm_vm_alias(") == 2
assert "skSM_vm_aliases" not in resources  # typo guard
assert finish.index("stage_render_parameter_buffer_descriptor") < finish.index(
    "vm.append_driver_mappings(core::mem::take(&mut prepared.driver_aliases))?"
)
# The old graph cache was replaced by acknowledged whole-graph teardown.
# Installed QID backing remains parked on the pair, allowing a later graph
# in this VM to reuse only aliases of that exact backing.
assert "prepare_cached_reactivation" not in handoff
reserve = handoff.index("self.reserve_logical_queue_teardown_graph(")
park = handoff.index("self.park_render_sksm_backing_for_graph_rebuild(pair_slot)")
detach = handoff.index("storage: self.render_slots.get_mut(pair_slot).storage.take()")
retain = handoff.index(".push_within_capacity(displaced)")
assert reserve < park < detach < retain
assert "incoming vm={} builds a new graph" in handoff



class Vm:
    def __init__(self):
        self.pages = {}

    def prepare(self, va, physical_pages):
        present = [self.pages.get(va + page * 0x4000) for page in range(len(physical_pages))]
        if any(page is not None for page in present):
            assert all(page is not None for page in present), "partial alias must fail"
            assert present == physical_pages, "foreign backing must fail"
            return False
        for page, physical in enumerate(physical_pages):
            self.pages[va + page * 0x4000] = physical
        return True


ta = [0x100000 + page * 0x4000 for page in range(8)]
frg = [0x200000 + page * 0x4000 for page in range(8)]
vm_a, vm_b = Vm(), Vm()
assert vm_a.prepare(0x7001EF8000, ta)
assert vm_a.prepare(0x7001F20000, frg)
assert vm_b.prepare(0x7001EF8000, ta)
assert vm_b.prepare(0x7001F20000, frg)
# A -> B -> A returns non-owning views of the original exact mappings.
assert not vm_a.prepare(0x7001EF8000, ta)
assert not vm_a.prepare(0x7001F20000, frg)
try:
    vm_a.prepare(0x7001EF8000, [0x900000 + page * 0x4000 for page in range(8)])
except AssertionError as error:
    assert "foreign backing" in str(error)
else:
    raise AssertionError("foreign queue backing was accepted")

print("PASS returning VM reuses exact VM-lifetime SKSM aliases; partial/foreign mappings fail")
