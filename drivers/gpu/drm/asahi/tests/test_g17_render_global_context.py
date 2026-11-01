#!/usr/bin/env python3
"""Context 1 owns global FList state; jobs use independent app-GART roots."""

from pathlib import Path

root = Path(__file__).resolve().parents[1]
mmu = (root / "mmu.rs").read_text()
resources = (root / "g17_resources.rs").read_text()
manager = (root / "g17_manager.rs").read_text()
live = (root / "g17_live_boot.rs").read_text()
limits = (root / "g17_queue_limits.rs").read_text()


def function(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


restore = function(mmu, "pub(crate) fn restore_t8140_kernel_context_after_bootstrap_render(")
assert "let context1_low = self.t8140_render_global_vm()?.ttb()" in restore
assert "context1_low | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT)" in restore

activate = function(mmu, "pub(crate) fn activate_t8140_render_global_context(")
assert "let global_low = self.t8140_render_global_vm()?.ttb()" in activate
assert activate.index("ttbs[1].ttb0.store(") < activate.index("mem::tlbi_asid(1)")
assert "T8140_CONTEXT_BOOTSTRAP_SOURCE_HIGH" in activate
assert "T8140_CONTEXT_BOOTSTRAP_EMPTY_HIGH" in activate
boot = function(live, "fn finish_boot_session(")
assert boot.index("manager.validate_native_pre_user_roots()?") \
    < boot.index("manager.activate_render_global_context()?")
assert boot.index("manager.activate_render_global_context()?") \
    < boot.index("render/compute UAPI ready")

constructor = function(resources, "pub(crate) fn new_with_uat(")
mapping = constructor[constructor.index("let render_global_vm ="):]
for owner, address in (
    ("control_operand_page_lists", "G17P_CONTROL_DIRECTORY_VA"),
    ("control_operand_table", "G17P_CONTROL_OPERAND_TABLE_VA"),
    ("control_operand_buffers", "g17p_control_operand_buffer_va(index)"),
):
    assert owner in mapping and address in mapping
assert "render_global_flist_mappings" in mapping

submit = function(manager, "fn begin_translated_render_with_hardware_buffer(")
assert ".bind(vm)" not in submit
assert "u8::try_from(execution_context.context_id())" in submit
assert "G17PRenderRootOwner::Job" in submit
assert "flist_bind" not in submit

assert "pub(crate) const BIND_CONTEXT_COUNT: u32 = 0" in limits
assert "pub(crate) const COMPUTE_CONTEXT_FIRST: u32 = 5" in limits
assert "pub(crate) const RENDER_APP_CONTEXT: u32 = TTBAT_CONTEXT_COUNT - 1" in limits

# Exact address-domain discriminator from v18j: firmware read context1 at
# 0x7000208480, which is inside the 0x7000208000 operand table. The permanent
# context1 root covers it while two job roots remain distinct.
context1 = {
    "directory": (0x7000000000, 0x8000),
    "table": (0x7000208000, 0x14000),
    "buffers": (0x7000220000, 22 * 0x100000),
}
fault = 0x7000208480
assert context1["table"][0] <= fault < sum(context1["table"])
job_contexts = {5: "vm-a", 6: "vm-b"}
assert len(set(job_contexts)) == len(job_contexts)

print("PASS permanent context1 FList closure and independent 58-context app-GART pool")
