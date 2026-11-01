#!/usr/bin/env python3
from pathlib import Path


ROOT = Path(__file__).resolve().parents[5]
MANAGER = (ROOT / "drivers/gpu/drm/asahi/g17_manager.rs").read_text()
LIVE = (ROOT / "drivers/gpu/drm/asahi/g17_live_boot.rs").read_text()
REGS = (ROOT / "drivers/gpu/drm/asahi/regs.rs").read_text()
MODULE = (ROOT / "drivers/gpu/drm/asahi/asahi.rs").read_text()


def block(start: str, end: str) -> str:
    begin = MANAGER.index(start)
    return MANAGER[begin:MANAGER.index(end, begin)]


validator = block(
    "fn validate_render_queue_consistency(",
    "fn commit_render_firmware_install(",
)
for field in (
    "header.queue_id",
    "header.data_master",
    "header.priority",
    "header.entry_low",
    "header.entry_high",
    "header.field_56",
    "qos_record[0]",
    "producer.queue_id",
    "producer.completion_selector",
    "producer.qos.queue_byte_32",
    "descriptor_context_ids",
    "header.context_id",
    "parameter_buffer_binding",
    "header.parameter_buffer_object",
    "header.parameter_buffer_token",
    "header.parameter_buffer_id",
    "header.parameter_buffer_bit",
    "config != expected",
):
    assert field in validator, field

begin = block(
    "fn begin_translated_render_with_hardware_buffer(",
    "pub(crate) fn publish_default_render_pair_after_freelist(",
)
outer = begin.index(".publish_partial_opening_pair(")
for call in (
    "validate-fragment-qid-consistency",
    "validate-tiling-qid-consistency",
):
    assert begin.index(call) < outer, call

assert "g17p_sksm_configure_queue" not in validator

direct = block(
    "fn configure_fresh_nonbootstrap_render_pair(",
    "fn publish_render_sksm_group(",
)
for queue in ("fragment", "tiling"):
    assert f"queues.{queue}.config.write_pair()" in direct
    assert f"queues.{queue}.config.enable_pair" in direct
assert "g17p_sksm_configure_powered_render_pair" in direct
assert "if !issued" in direct
assert "return Err(EAGAIN);" in direct

pair_transaction = REGS[
    REGS.index("pub(crate) fn g17p_sksm_configure_powered_render_pair("):
]
pair_transaction = pair_transaction[:pair_transaction.index("fn sgx_read32")]
ordered = [
    "self.sksm_fifo.lock()",
    "g17p_sksm_port_powered()",
    "g17p_sksm_publish_locked(&sgx, fragment.0)",
    "g17p_sksm_publish_locked(&sgx, fragment.1)",
    "g17p_sksm_publish_locked(&sgx, tiling.0)",
    "g17p_sksm_publish_locked(&sgx, tiling.1)",
]
positions = [pair_transaction.index(term) for term in ordered]
assert positions == sorted(positions)

submit = LIVE[
    LIVE.index("pub(crate) fn submit_translated_render("):
]
submit = submit[:submit.index("fn ensure_g17p_render_registration_power(")]
begin_call = submit.index(".begin_translated_render(")
preflight = submit.index("self.ensure_g17p_render_registration_power(manager, &registers)?")
assert begin_call < preflight

# Power acquisition is invoked by the manager immediately before the direct
# four-write transaction, after CPU-only graph/descriptor staging.
assert begin.index("registration_power(self)") < begin.index(
    "self.configure_fresh_nonbootstrap_render_pair("
)
assert "Err(error) if error == EAGAIN => {" in begin

power = LIVE[
    LIVE.index("fn ensure_g17p_render_registration_power("):
]
power = power[:power.index("/// Publish the host lever")]
for prerequisite in (
    "self.cpus.is_some()",
    "self.gfx_rtkit.is_some()",
    "self.gfx1_rtkit.is_some()",
    "state.acked.load(Ordering::Acquire)",
    "g17p_sksm_configuration_port_powered()",
    "DEVICE_CONTROL_OPCODE_IDLE_POWER_OFF",
    "encode_primary_device_control()",
    "[target; 3]",
    "DEVICE_CONTROL_IDLE_POWER_OFF_ARG_DISABLE_IDLE_AND_WAKE",
    "release_g17p_render_registration_power",
    "Err(ETIMEDOUT)",
):
    assert prerequisite in power, prerequisite

# Production registration is automatic for every fresh non-bootstrap pair;
# it is not hidden behind the old diagnostic qid2/3 switch.
assert "g17p_render_direct_config" not in MODULE

direct_call = begin.index("self.configure_fresh_nonbootstrap_render_pair(")
assert direct_call < begin.index("let (pm_submission, flist_submission) =") < outer
assert "!execution_context.matches(vm) || !execution_context.is_current()" in MANAGER
print("g17 render QID consistency validation: PASS")
