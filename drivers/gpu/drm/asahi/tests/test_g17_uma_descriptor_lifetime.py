#!/usr/bin/env python3
"""Source guards for per-owner UMA descriptor publication and retention."""

from pathlib import Path


src = Path(__file__).resolve().parent.parent
resources = (src / "g17_resources.rs").read_text()
manager = (src / "g17_manager.rs").read_text()


def block(source: str, marker: str) -> str:
    start = source.index(marker)
    end = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


owner = block(resources, "pub(crate) struct G17PRenderFlist")
owner_impl = block(resources, "impl G17PRenderFlist {")
constructor = block(resources, "impl G17PUninstalledRenderGraph {")
alias = block(resources, "    fn stage_user_render_flist(")
stage = block(resources, "    fn stage_render_uma_owner(")
slot = block(manager, "struct G17PRenderSlotState")
manager_struct = block(manager, "pub(crate) struct G17PManagerConstruction {")

assert "control_shared: MappedObject" in owner
assert "backing: MappedObject" in owner
# Shared FList: each pool leases its own ID from the 129..255 bitmap and
# returns it when the pool drops, instead of deriving it from a pair.
id_lease = block(resources, "impl G17PRenderFlistId {")
assert "Ok(Self(129 + bank as u16 * 64 + bit as u16))" in id_lease
assert "let bit = self.0 as usize - 129;" in block(resources, "impl Drop for G17PRenderFlistId {")
image = block(owner_impl, "    fn control_shared_image(")
assert "put_u64(&mut bytes, 0x14, page_list_va)" in image
assert "put_u64(&mut bytes, 0x30, run_table_va)" in image
assert "put_u32(&mut bytes, 0x08, u32::from(id))" in image
assert "Self::control_shared_image(id, page_list_va, run_table_va, inner_va)?" in owner_impl

assert "G17PRenderFlist::new" in constructor
assert "self.stage_render_uma_owner(&storage.render_flist)?" not in constructor
assert "self.stage_render_flist_state()?" not in alias
# Shared FList: the pool maps its one client alias when it is created and
# keeps it until the pool drops; graph staging only checks the client VM.
assert "if render_flist.client_vm.id() != vm.id() { return Err(EFAULT); }" in alias
assert "_client_backing: client_backing," in owner_impl

assert "g17p_umadesc_offset(hardware_buffer_id)" in stage
assert "let state = owner.uma_state()?" in stage
assert "encode_g17p_umadesc(previous, state)" in stage

# Shared FList: the per-pool lifecycle is indexed by FList ID in the manager;
# a pair keeps only which pool it uses, its leases and its prepared acquisition.
for field in ["flist_index", "flist_leases", "prepared_render_flist"]:
    assert field in slot
    assert f"    {field}:" not in manager_struct
assert "    render_flist_owners: KVec<Option<G17PBootstrapUmaLifecycle>>," in manager_struct

begin = block(manager, "    fn begin_translated_render_with_hardware_buffer(")
assert "stage_user_render_uma" in begin
# Validation must precede every firmware-visible owner write. In particular,
# WrongPhase/WrongOwner must not replace a descriptor before returning EBUSY.
assert begin.index("G17PBootstrapUmaLifecycle::acquire_owner") < begin.index("repopulate_render_flist")
assert begin.index("G17PBootstrapUmaLifecycle::acquire_owner") < begin.index("stage_user_render_uma")
assert begin.index("stage_user_render_uma") < begin.index("prepared_render_flist = Some(prepared)")

warm = block(manager, "    fn finish_warm_render_pair_storage(")
assert warm.index("G17PBootstrapUmaLifecycle::acquire_owner") < warm.index("repopulate_render_flist")
assert warm.index("G17PBootstrapUmaLifecycle::acquire_owner") < warm.index("stage_user_render_uma")
print("per-owner UMA descriptor lifetime/source guards passed")
