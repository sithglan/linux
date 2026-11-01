#!/usr/bin/env python3
"""A replaced render pool is visible in every context rooted in its VM."""

from pathlib import Path


root = Path(__file__).resolve().parents[1]
mmu = (root / "mmu.rs").read_text()
resources = (root / "g17_resources.rs").read_text()


def function(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


context_mask = function(mmu, "fn context_mask_for_root(")
vm_context_mask = function(mmu, "pub(crate) fn t8140_vm_context_mask(")
flush = function(mmu, "pub(crate) fn flush_t8140_vm_contexts(")
live_range_flush = function(mmu, "fn tlbi_live_contexts(")
step_map = function(mmu, "    fn step_map(")
private_map = function(mmu, "    fn map_node(")
assert "owner.page_table.ttb()" in vm_context_mask
assert "context_mask_for_root(root, inner.ttbs())" in vm_context_mask
assert "ttb.ttb0.load(Ordering::Acquire)" in context_mask
assert "== t8140_tagged_root(root, context_id)" in context_mask
assert "self.t8140_vm_context_mask(vm)?" in flush
assert flush.index("fence(Ordering::SeqCst)") < flush.index("mem::tlbi_asid(")
assert flush.index("mem::tlbi_asid(") < flush.index("mem::sync()")
assert "T8140_COMPUTE_CTXS" not in flush
assert "RENDER_APP_CONTEXT" not in flush

# User VM_BIND maps use the same root-identity rule, but invalidate only the
# changed range. This is required for negative translations cached after an
# independent execution context is installed and before its buffers are bound.
assert "context_mask_for_root(self.ttb(), shared.ttbs())" in live_range_flush
assert live_range_flush.index("fence(Ordering::SeqCst)") < live_range_flush.index(
    "mem::tlbi_range("
)
assert live_range_flush.index("mem::tlbi_range(") < live_range_flush.index("mem::sync()")
assert step_map.index("map_and_link_va(") < step_map.index(
    "self.tlbi_live_contexts(op.addr(), op.range())"
)
assert private_map.index("self.page_table.map_pages(") < private_map.index(
    "self.tlbi_live_contexts(node.start(), node.mapped_size as u64)"
)

new_storage = function(resources, "pub(crate) fn finish_user_render_storage(")
assert new_storage.index("vm.install_render_pool_mappings(") < new_storage.index(
    "self.address_space.uat.flush_t8140_vm_contexts(vm)?"
)
assert new_storage.index("self.address_space.uat.flush_t8140_vm_contexts(vm)?") < new_storage.index(
    "self.stage_render_parameter_buffer_page_list(storage)?"
)

class ContextTlb:
    def __init__(self):
        self.cached = {}

    def translate(self, context, page_table, va):
        key = (context, va)
        if key not in self.cached:
            self.cached[key] = page_table.get(va)
        return self.cached[key]

    def flush(self, contexts):
        self.cached = {
            key: value for key, value in self.cached.items() if key[0] not in contexts
        }


# A cold fresh-storage image owns parity zero, then the first real odd ordinal
# removes it.  The following even ordinal reuses exactly that VA.  Invalidating
# only the bind context leaves the independently allocated render context with
# its cached absent translation; matching-root invalidation exposes generation
# two before its descriptor can be published.
parity_even_tpc = 0x1002050000
page_table = {parity_even_tpc: "cold"}
tlb = ContextTlb()
assert tlb.translate(9, page_table, parity_even_tpc) == "cold"
del page_table[parity_even_tpc]
tlb.flush({1})
page_table[parity_even_tpc] = "ordinal-14"
assert tlb.translate(9, page_table, parity_even_tpc) == "cold"
tlb.flush({1, 9})
assert tlb.translate(9, page_table, parity_even_tpc) == "ordinal-14"

# Dynamic TVB blocks use the private fixed-map path rather than GPUVM
# step_map.  A published fragment context can cache the reserved VA as absent
# before firmware asks the host to grow.  Invalidating only the bind context
# leaves that negative entry live; matching-root invalidation exposes the new
# block before the opcode-8 acknowledgement publishes it to firmware.
tvb_growth_block = 0x1004000000
page_table = {}
tlb = ContextTlb()
assert tlb.translate(10, page_table, tvb_growth_block) is None
page_table[tvb_growth_block] = "grown-block"
tlb.flush({1})
assert tlb.translate(10, page_table, tvb_growth_block) is None
tlb.flush({1, 10})
assert tlb.translate(10, page_table, tvb_growth_block) == "grown-block"


def contexts_for_root(entries, root):
    return [index for index, entry in enumerate(entries) if entry == (root, index)]


# Bind, descriptor and app-GART contexts may all carry one VM. A different
# live client's entries are left untouched; no fixed context identity is used.
ttbat = [None] * 64
ttbat[1] = (0x111000, 1)
ttbat[7] = (0x111000, 7)
ttbat[63] = (0x111000, 63)
ttbat[8] = (0x222000, 8)
assert contexts_for_root(ttbat, 0x111000) == [1, 7, 63]
assert contexts_for_root(ttbat, 0x222000) == [8]

print("PASS live-VM TTBAT root matching flushes every alias context")
