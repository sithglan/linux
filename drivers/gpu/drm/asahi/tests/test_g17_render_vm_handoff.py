#!/usr/bin/env python3
"""Source boundaries for job-private render roots and bounded slot switching."""
from pathlib import Path

root = Path(__file__).resolve().parents[1]
manager = (root / "g17_manager.rs").read_text()
runtime = (root / "g17_live_boot.rs").read_text()
drm = (root / "g17_drm.rs").read_text()
resources = (root / "g17_resources.rs").read_text()
mmu = (root / "mmu.rs").read_text()
pgtable = (root / "pgtable.rs").read_text()


def function(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


handoff = function(manager, "pub(crate) fn handoff_retained_render_vm(")
preserve = function(manager, "fn preserve_immediate_retired_render_graph(")
assert handoff.index("self.require_retired_render_owner()") < handoff.index("G17PRetiredRenderGraph {")
assert '"handoff-require-retired-owner"' in handoff
assert "self.render_slots.active_mut().bind.take().ok_or(EIO)?" in handoff
assert "self.render_slots.active_mut().context_aliases.take()" in handoff
assert "self.render_slots.active_mut().storage.take().ok_or(ENODEV)?" in handoff
assert "release_client_context" not in handoff
assert ".push_within_capacity(previous)" in preserve
assert "completion_references_descriptor_range" not in preserve
assert "self.retired_render_handoff_graph = Some(retired)" in handoff
assert "install_t8140_native_context_aliases" not in handoff
assert "incoming VM builds a new root" in handoff

retired_graph = manager[manager.index("struct G17PRetiredRenderGraph {"):]
retired_graph = retired_graph[:retired_graph.index("}\n")]
assert retired_graph.index("bind: G17PRenderRootOwner") < retired_graph.index(
    "context_aliases: Option<mmu::T8140NativeContextAliases>"
)
assert retired_graph.index("context_aliases: Option<mmu::T8140NativeContextAliases>") < retired_graph.index(
    "storage: g17_resources::G17PUserRenderStorage"
)
release_retired = function(manager, "fn release_retired_handoff_graph(")
assert "storage.into_retired_parameter_management()" in release_retired
assert release_retired.index("if !retained_firmware_state") < release_retired.index(
    "ids.unreserve(id)"
)

retired = function(manager, "fn require_retired_render_owner(")
assert "snapshot.job_list_empty" in retired
assert "snapshot.tiling.idle()" in retired
assert "snapshot.fragment.idle()" in retired
assert "partial_opening_outer_counters" not in retired

# Every per-pair owner lives in the slot state; selecting a pair only
# moves the cursor, allocating a slot on its first selection and never
# copying state.
slot_state = function(manager, "struct G17PRenderSlotState {")
for owner in (
    "storage", "tracker", "bind", "context_aliases",
    "retired_handoff_graph",
    "submission_ordinal", "auxiliary_timestamp",
    "hardware_buffer_id", "hardware_buffer_published", "sksm",
):
    assert owner in slot_state

select = function(manager, "    fn select(&mut self, slot: u8) -> Result {")
assert "G17PRenderQueuePair::SLOT_COUNT" in select
assert "G17PRenderSlotState::new()?" in select
assert select.index("G17PRenderSlotState::new()?") < select.index("self.active_slot = slot")
assert "core::mem::swap" not in select
assert function(manager, "pub(crate) fn select_render_slot(").count("self.render_slots.select(slot)") == 1

# The manager contains the active render owner and two complete parked state
# images. It must be initialized directly in its KBox: building `Self` by
# value grows the probe frame beyond the arm64 kernel stack once nested under
# boot_initial_session.
constructor = function(manager, "pub(crate) fn new_with_uat(")
assert "KBox::init(" in constructor
assert "try_init!(Self {" in constructor
assert "KBox::new(\n            Self {" not in constructor

outer = function(resources, "pub(crate) fn publish_partial_opening_pair<")
assert "queue_pair.fragment as u8" in outer
assert "queue_pair.tiling as u8" in outer
assert "g17p_render_queue_id(" not in outer

late_tiling = function(resources, "fn build_g17p_late_tiling_bytes(")
assert "queue_pair.tiling" in late_tiling
assert "queue_pair.fragment" in late_tiling
assert "hardware_buffer_id," in late_tiling
assert "queue_pair.qos_hardware_buffer_id()" not in late_tiling
assert "g17p_render_queue_id(" not in late_tiling

stage_fragment = function(resources, "pub(crate) fn stage_fragment(")
assert "self.qos_hardware_buffer_id()?" in stage_fragment
optional_addresses = function(resources, "fn optional_addresses(")
assert "self.ta_hardware_buffer_id," in optional_addresses
assert "addresses.channel_control = control.gpu_va()" in optional_addresses
new_sksm = function(manager, "fn new_render_sksm_queues(")
assert "_ => hardware_buffer_id" in new_sksm
assert "pair.qos_hardware_buffer_id()" not in new_sksm
assert "G17P_NATIVE_RENDER_QOS_HARDWARE_BUFFER_ID" not in new_sksm

stage_sksm = function(manager, "fn stage_render_groups_with_sksm(")
assert "qos_hardware_buffer_id" in stage_sksm
assert "retained_hardware_buffer_id" not in stage_sksm
assert stage_sksm.count("producer.qos.queue_byte_32 = hardware_buffer_id") == 2

submit = function(runtime, "pub(crate) fn submit_translated_render(")
assert "select_render_slot(render_slot)?" in submit
settle_at = submit.index("self.settle_open_recovery_before_publication()?")
restore_at = submit.index("select_render_slot(render_slot)?", settle_at)
handoff_at = submit.index(".handoff_retained_render_vm(", settle_at)
assert settle_at < restore_at < handoff_at
assert "if defer_completion" in submit
watch = function(drm, "impl g17_job::G17PRenderQueueBackend for G17PRenderQueueBackend")
assert watch.count("packet.completion().render_slot()") >= 2
assert watch.count("timeout_deferred_render(") == 3
assert "quarantine_render_slot(" not in watch
assert "poll_deferred_render_completion" not in watch
# Ordinary completion is event-driven with the runtime mutex released. Only
# scheduler timeout/cancel enters the bounded final-observation path above.
events = function(runtime, "fn service_async_render_completions(")
settle = function(runtime, "fn settle_deferred_render_completion(")
assert "self.poll_deferred_render_completion(slot)?" in events
assert "self.settle_deferred_render_completion(slot, observed)?" in events
assert "signal.complete(Ok(observed))" in settle

# Job-root SecureGart setup must retain Mesa's six existing GPUVM low views.
# Only the VM-lifetime-reserved null view is a driver-owned KernelMapping.
job_aliases = function(mmu, "pub(crate) fn install_t8140_job_context_aliases(")
assert ".enumerate().skip(1)" in job_aliases
assert "T8140_CLIENT_NULL_VIEW" in job_aliases
assert job_aliases.count("source.map_in_range(") == 1
assert "source.translate_iova(high)? != source.translate_iova(low)?" in job_aliases

bind_object = function(mmu, "pub(crate) fn bind_object(")
assert ".alloc_pages(" not in bind_object

# The authoritative PTE layer rejects cross-allocator collisions instead of
# logging and overwriting one owner with another.
prepare = function(pgtable, "pub(crate) fn prepare_map(")
map_pages = function(pgtable, "pub(crate) fn map_pages(")
assert "return Err(EBUSY)" in prepare
assert "return Err(EBUSY)" in map_pages
assert "pte.store(phys" in map_pages
assert map_pages.index("return Err(EBUSY)") < map_pages.index("pte.store(phys")
unmap_pages = function(pgtable, "pub(crate) fn unmap_pages(")
assert "if iova_range.is_empty()" in unmap_pages
assert unmap_pages.index("if iova_range.is_empty()") < unmap_pages.index("let expected =")
assert "complete_coverage(expected, visited, fully_mapped)" in unmap_pages

# Process-root queue peers occupy the bounded gap between the render and
# relocated-compute FLists, while context-0 keeps the native queue addresses.
low_va = function(resources, "const fn low_va(")
assert "G17P_RENDER_JOB_QUEUE_LOW_BASE" in low_va
assert "G17P_RENDER_QUEUE_LOW_BASE" not in low_va
assert "G17P_RENDER_JOB_QUEUE_LOW_BASE == 0x70_01ef_8000" in resources

storage_impl = resources[resources.index("impl G17PUserRenderStorage {"):]
storage_new = function(storage_impl, "fn new(")
assert storage_new.count("g17p_cl_client_low_va_range()") >= 3
assert "G17P_BOOTSTRAP_TA_DESCRIPTOR_LOW_VA\n                    .." not in storage_new
reachability = function(resources, "pub(crate) fn first_unreachable(")
assert '"primary-index-page"' not in reachability
assert '"pm-page-list"' in reachability
assert "pm.page_list_low_va()" in reachability
assert "g17p_render_page_list_va(self.queue_pair)" in reachability
ta_status = reachability[reachability.index('"ta-status-page"'):]
ta_status = ta_status[:ta_status.index('"3d-status-page"')]
assert "vm.covers_range(" in ta_status
assert "uat.kernel_vm().covers_range(" not in ta_status

print("PASS job-private VM handoff and complete two-slot owner parking; ordinary completion is event-driven")
