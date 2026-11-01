#!/usr/bin/env python3
"""A KernelMapping unmap is invalidated in every context publishing its root.

Hardware repro: a second render queue in one VM builds a fresh graph whose
dual-root descriptor alias reuses the kernel_lower_vm VA of a graph reclaimed
moments earlier.  kernel_lower_vm has no bind slot, so the generic unmap path
invalidated nothing; firmware fetched the new descriptor through context 0's
stale entry, read the freed graph's last descriptor and executed it in that
graph's released context (halted GMMU reason 3, "no retained render bind").
"""

from pathlib import Path


root = Path(__file__).resolve().parents[1]
mmu = (root / "mmu.rs").read_text()
resources = (root / "g17_resources.rs").read_text()


def block(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


drop = block(mmu, "impl Drop for KernelMapping {")
unmap = drop.index("owner.page_table.unmap_pages(self.iova_range())")
slot_flush = drop.index("if let Some(asid) = owner.slot() {")
live_flush = drop.index("owner.tlbi_live_contexts(self.iova(), self.size() as u64)")
gate = drop.index("if owner.uat_inner.t8140 {")
# Leaves go first, then every publishing context is invalidated, all inside
# drop() so the VA node is released only afterwards.
assert unmap < slot_flush < gate < live_flush
# The AGX2 bind-slot flush is unchanged and still unconditional on its slot.
assert "mem::tlbi_range(asid as u8, self.iova() as usize, self.size());" in drop

live = block(mmu, "fn tlbi_live_contexts(")
assert "context_mask_for_root(self.ttb(), shared.ttbs())" in live
assert live.index("fence(Ordering::SeqCst)") < live.index("mem::tlbi_range(")
assert live.index("mem::tlbi_range(") < live.index("mem::sync()")

inner = block(mmu, "struct UatInner {")
assert "    t8140: bool," in inner
make_inner = block(mmu, "    fn make_inner(")
assert "t8140: cfg == UatConfig::T8140," in make_inner

# The descriptor aliases that exposed this are KernelMappings in
# kernel_lower_vm, owned by the render storage and dropped with it.
dual_root = block(resources, "fn map_shared_render_descriptor_range(")
dual_root_compact = " ".join(dual_root.split())
assert (
    "map_shared_render_range( object, client_vm, global_vm, object_range, "
    "g17p_cl_client_low_va_range(), mmu::PROT_GPU_SHARED_RO, )"
    in dual_root_compact
)
shared_mapper = block(resources, "fn map_shared_render_range(")
assert "global_vm," in shared_mapper and "object.map_range_into_range(" in shared_mapper
storage = block(resources, "pub(crate) struct G17PUserRenderStorage {")
assert "_descriptor_ta_global: mmu::KernelMapping," in storage
assert "_descriptor_3d_global: mmu::KernelMapping," in storage


def tagged(root_pa, context):
    return root_pa | 1 | (context << 48)


def context_mask(root_pa, ttb0):
    return sum(1 << i for i, value in enumerate(ttb0) if value == tagged(root_pa, i))


class Model:
    """Firmware reads a low VA through a context with a caching TLB."""

    def __init__(self, old_drop_invalidates_live_contexts):
        self.invalidate_live = old_drop_invalidates_live_contexts
        self.tlb = {}
        self.lower = {}  # kernel_lower_vm leaves: va -> pa
        self.ttb0 = [0] * 64
        self.lower_root = 0x800000
        self.ttb0[0] = tagged(self.lower_root, 0)  # split layout context 0
        self.free_vas = []

    def read(self, context, va):
        key = (context, va)
        if key not in self.tlb:
            self.tlb[key] = self.lower.get(va)
        return self.tlb[key]

    def map(self, va, pa):
        self.lower[va] = pa

    def drop(self, va):
        del self.lower[va]
        # kernel_lower_vm: owner.slot() is None, so only the live-context
        # rule can invalidate anything.
        if self.invalidate_live:
            mask = context_mask(self.lower_root, self.ttb0)
            self.tlb = {k: v for k, v in self.tlb.items()
                        if not (mask >> k[0] & 1 and k[1] == va)}
        self.free_vas.append(va)


for fixed in (False, True):
    m = Model(fixed)
    desc = 0x704023C000
    m.map(desc, "graph-A descriptor (context 9)")
    assert m.read(0, desc) == "graph-A descriptor (context 9)"
    m.drop(desc)  # quiescence reclaims graph A
    reused = m.free_vas.pop()  # lowest free VA goes to the second queue
    assert reused == desc
    m.map(reused, "graph-B descriptor (context 10)")
    got = m.read(0, reused)
    if fixed:
        assert got == "graph-B descriptor (context 10)", got
    else:
        # Reproduces the hardware failure: the freed graph's descriptor.
        assert got == "graph-A descriptor (context 9)", got

print("ok")
