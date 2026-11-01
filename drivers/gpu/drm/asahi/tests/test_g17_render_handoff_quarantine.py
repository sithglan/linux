#!/usr/bin/env python3
"""Firmware-referenced cross-VM graphs stay alive through later handoffs."""

from pathlib import Path


source = (Path(__file__).resolve().parents[1] / "g17_manager.rs").read_text()


def function(marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


graph = source[source.index("struct G17PRetiredRenderGraph {"):]
graph = graph[:graph.index("}\n")]
assert graph.index("bind: G17PRenderRootOwner") < graph.index(
    "context_aliases: Option<mmu::T8140NativeContextAliases>"
)
assert graph.index("context_aliases: Option<mmu::T8140NativeContextAliases>") < graph.index(
    "storage: g17_resources::G17PUserRenderStorage"
)

handoff = function("pub(crate) fn handoff_retained_render_vm(")
preserve = function("fn preserve_immediate_retired_render_graph(")
assert handoff.index("self.require_retired_render_owner()") < handoff.index(
    "G17PRetiredRenderGraph {"
)
assert '"handoff-require-retired-owner"' in handoff
for owner in (
    "self.render_slots.active_mut().bind.take().ok_or(EIO)?",
    "self.render_slots.active_mut().context_aliases.take()",
    "self.render_slots.active_mut().storage.take().ok_or(ENODEV)?",
):
    assert owner in handoff
assert "release_client_context" not in handoff
assert "unreserve_render_storage_hardware_buffer_id" not in handoff
assert ".push_within_capacity(previous)" in preserve
assert "completion_references_descriptor_range" not in preserve
assert "self.retired_render_handoff_graph = Some(retired)" in handoff
# Queue destruction and context supersession permit reuse of the complete
# graph. They must not split it into separately recycled PM/support objects.
assert "release_superseded_render_graph" not in preserve
select = function("fn select_cached_render_graph(")
assert "Self::retired_render_graph_reclaimable(" in select
assert "client_rebind_addresses_available(vm)" in select

release = function("fn release_retired_handoff_graph(")
assert "storage.into_retired_parameter_management()" in release
assert release.index("if !retained_firmware_state") < release.index("ids.unreserve(id)")

stop = function("pub(crate) fn release_runtime_objects_after_processor_stop(")
assert "state.retired_handoff_graph.take()" in stop
assert "self.retired_render_handoff_graph.take()" in stop
assert "self.retired_render_handoff_graphs.pop()" in stop
assert "retired_firmware_render_graphs" not in source

reclaimable = function("fn retired_render_graph_reclaimable(")
assert "*epoch >= graph.retire_after_context_epoch" in reclaimable
superseded = function("fn release_superseded_render_graph(")
assert superseded.index("into_retired_parameter_management") < superseded.index(
    "ids.unreserve(id)"
)
assert superseded.index("if !retained_firmware_state") < superseded.index("ids.unreserve(id)")
assert superseded.index("ids.unreserve(id)") < superseded.index("drop(bind)")


print("PASS firmware-visible graphs remain cached through queue close and context supersession")
