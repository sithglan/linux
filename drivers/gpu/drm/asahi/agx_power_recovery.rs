// SPDX-License-Identifier: GPL-2.0-only OR MIT

#![cfg_attr(not(test), allow(dead_code))]

//! Evidence-backed G15-G17 recovery and G17 internal-power contracts.
//!
//! This module contains only pure validation and host-side recovery writes.
//! It performs no MMIO, mailbox traffic, ASC reset, or firmware-state write.
//! The power-transition grammar and idle-poll behavior are independently
//! byte-confirmed in pinned G17G/G17P/G17S GFX1 images. Only G17S establishes
//! the ordered direct PIO prefix and following firmware operations. The
//! recovery word, host writes, and no-op reset hook are byte-confirmed in
//! pinned G15G/G15S/G15C, G16G/G16X, and G17G/G17S/G17P host drivers. Their
//! shared values are modeled here without reusing target-specific structure
//! offsets (`0x5190` on G15 and G16G, but `0x4ae0` on G16X and G17).

/// The grounded, pure validation model is available.
pub(crate) const GROUNDED_POWER_RECOVERY_MODEL_AVAILABLE: bool = true;

/// Exact targets whose host binaries establish the shared recovery grammar.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryEvidenceTarget {
    G15G,
    G15S,
    G15C,
    G15D,
    G16G,
    G16S,
    G16C,
    G17P,
    G17G,
    G17S,
}

pub(crate) const RECOVERY_EVIDENCE_TARGETS: [RecoveryEvidenceTarget; 10] = [
    RecoveryEvidenceTarget::G15G,
    RecoveryEvidenceTarget::G15S,
    RecoveryEvidenceTarget::G15C,
    RecoveryEvidenceTarget::G15D,
    RecoveryEvidenceTarget::G16G,
    RecoveryEvidenceTarget::G16S,
    RecoveryEvidenceTarget::G16C,
    RecoveryEvidenceTarget::G17P,
    RecoveryEvidenceTarget::G17G,
    RecoveryEvidenceTarget::G17S,
];

/// G17 GFX1 images establish the internal transition grammar and one idle
/// poll. Only G17S establishes the direct PIO prefix and intervening
/// firmware-operation order. All three targets establish physical type-0
/// mappings. G17G/G17P leave type 0x0b unbound, while G17S installs it in a
/// target override; none of those facts proves Linux MMIO ownership.
pub(crate) const EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE: bool = false;

/// The firmware writes and cross-role 0x22/0x23 protocol remain unproved, so
/// Linux must not execute G17 recovery from this host-write subset.
pub(crate) const EXECUTABLE_G17_RECOVERY_HANDSHAKE_AVAILABLE: bool = false;

/// Pinned G17 host targets whose `halGetPMConfig()` return block is exact.
///
/// The six fields remain semantically opaque. In particular, the G17P block
/// follows its HAL200 implementation rather than the dual-role G17 topology,
/// while the pinned G17G and G17S hosts share the HAL300 block.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17PmConfigEvidenceTarget {
    G17P,
    G17G,
    G17S,
}

pub(crate) const G17_PM_CONFIG_EVIDENCE_TARGETS: [G17PmConfigEvidenceTarget; 3] = [
    G17PmConfigEvidenceTarget::G17P,
    G17PmConfigEvidenceTarget::G17G,
    G17PmConfigEvidenceTarget::G17S,
];

/// Exact opaque six-u64 blocks returned by the pinned host implementations.
///
/// These constants are diagnostic evidence only. Unknown field semantics and
/// the absence of pinned G17A/G17C host witnesses keep them out of executable
/// power or init-data paths.
pub(crate) const G17_HAL200_PM_CONFIG_OPAQUE: [u64; 6] = [0x4, 0x4, 0x1800, 0x8, 0x8, 0x10];
pub(crate) const G17_HAL300_PM_CONFIG_OPAQUE: [u64; 6] = [0x10, 0x40, 0x1800, 0x8, 0x8, 0x40];

pub(crate) const fn g17_pm_config_for_evidence_target(
    target: G17PmConfigEvidenceTarget,
) -> &'static [u64; 6] {
    match target {
        G17PmConfigEvidenceTarget::G17P => &G17_HAL200_PM_CONFIG_OPAQUE,
        G17PmConfigEvidenceTarget::G17G | G17PmConfigEvidenceTarget::G17S => {
            &G17_HAL300_PM_CONFIG_OPAQUE
        }
    }
}

/// One raw `{ frequency, voltage }` record from the captured T6050 ADT.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct T6050PerfState {
    pub(crate) freq_hz: u32,
    pub(crate) volt_mv: u32,
}

/// Exact shape of the captured T6050/G17S main performance-state properties.
pub(crate) const T6050_G17S_PERF_STATE_COUNT: u32 = 14;
pub(crate) const T6050_G17S_PERF_STATE_TABLE_COUNT: u32 = 2;
const T6050_G17S_MAIN_PERF_STATE_RECORDS: usize =
    T6050_G17S_PERF_STATE_COUNT as usize * T6050_G17S_PERF_STATE_TABLE_COUNT as usize;

/// Fail-closed validation errors for the captured T6050 `*1` mirror contract.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum T6050PerfStateMirrorError {
    UnexpectedPerfStateCount,
    UnexpectedPerfStateTableCount,
    UnexpectedMainCoreShape,
    UnexpectedMainSramShape,
    UnexpectedCoreMirrorShape,
    UnexpectedSramMirrorShape,
    CoreMirrorMismatch,
    SramMirrorMismatch,
}

/// Validate the exact captured T6050/G17S `perf-states1` mirror relationship.
///
/// `perf-states1` and `perf-states-sram1` duplicate table 1, the second half
/// of their corresponding two-table main properties. This proves neither a
/// consumer nor a third power domain, so this function only validates input.
pub(crate) fn validate_t6050_g17s_perf_state_mirrors(
    perf_state_count: u32,
    perf_state_table_count: u32,
    perf_states: &[T6050PerfState],
    perf_states_sram: &[T6050PerfState],
    perf_states1: &[T6050PerfState],
    perf_states_sram1: &[T6050PerfState],
) -> Result<(), T6050PerfStateMirrorError> {
    if perf_state_count != T6050_G17S_PERF_STATE_COUNT {
        return Err(T6050PerfStateMirrorError::UnexpectedPerfStateCount);
    }
    if perf_state_table_count != T6050_G17S_PERF_STATE_TABLE_COUNT {
        return Err(T6050PerfStateMirrorError::UnexpectedPerfStateTableCount);
    }
    if perf_states.len() != T6050_G17S_MAIN_PERF_STATE_RECORDS {
        return Err(T6050PerfStateMirrorError::UnexpectedMainCoreShape);
    }
    if perf_states_sram.len() != T6050_G17S_MAIN_PERF_STATE_RECORDS {
        return Err(T6050PerfStateMirrorError::UnexpectedMainSramShape);
    }
    if perf_states1.len() != T6050_G17S_PERF_STATE_COUNT as usize {
        return Err(T6050PerfStateMirrorError::UnexpectedCoreMirrorShape);
    }
    if perf_states_sram1.len() != T6050_G17S_PERF_STATE_COUNT as usize {
        return Err(T6050PerfStateMirrorError::UnexpectedSramMirrorShape);
    }

    let table1_start = T6050_G17S_PERF_STATE_COUNT as usize;
    if perf_states1 != &perf_states[table1_start..] {
        return Err(T6050PerfStateMirrorError::CoreMirrorMismatch);
    }
    if perf_states_sram1 != &perf_states_sram[table1_start..] {
        return Err(T6050PerfStateMirrorError::SramMirrorMismatch);
    }

    Ok(())
}

/// G17 GFX1 internal power states whose numeric names are string-confirmed.
pub(crate) const GFX1_POWER_SLEEP: u8 = 0x08;
pub(crate) const GFX1_POWER_NAP: u8 = 0x10;

/// Mandatory intermediate state between SLEEP and NAP.
///
/// Its private Apple enum name is not known, so the numeric value is kept
/// without assigning a semantic name.
pub(crate) const GFX1_POWER_INTERMEDIATE_21: u8 = 0x21;

/// G17 targets with independently pinned GFX1 transition and idle-poll bytes.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17Gfx1PowerEvidenceTarget {
    G17P,
    G17G,
    G17S,
}

pub(crate) const G17_GFX1_POWER_EVIDENCE_TARGETS: [G17Gfx1PowerEvidenceTarget; 3] = [
    G17Gfx1PowerEvidenceTarget::G17P,
    G17Gfx1PowerEvidenceTarget::G17G,
    G17Gfx1PowerEvidenceTarget::G17S,
];

/// Exact target-specific call site for the shared pre-idle CAS helper.
///
/// The three helper bodies have the same 53-instruction normalized dataflow:
/// their only body-local atomic access compares `1` and replaces it with `0`
/// at the passed firmware-global object. G17S executes its direct PIO prefix
/// before the same helper, while G17G/G17P reach the helper without that
/// prefix. Thus the helper cannot account for the missing target PIO writes.
/// Its private name and the ownership of its scheduler slow path stay opaque.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17Gfx1PreIdleEntryEvidence {
    pub(crate) helper_text_offset: u32,
    pub(crate) helper_object_vm_offset: u32,
    pub(crate) direct_pio_prefix_before_helper: bool,
}

pub(crate) const G17_GFX1_PRE_IDLE_HELPER_WINDOW_SIZE: u32 = 0xd4;
pub(crate) const G17_GFX1_PRE_IDLE_HELPER_INSTRUCTION_COUNT: u32 = 53;
pub(crate) const G17_GFX1_PRE_IDLE_HELPER_CAS_COMPARE: u64 = 1;
pub(crate) const G17_GFX1_PRE_IDLE_HELPER_CAS_REPLACEMENT: u64 = 0;
/// No helper body directly resolves or accesses a PIO descriptor.
pub(crate) const G17_GFX1_PRE_IDLE_HELPER_CONTAINS_DIRECT_PIO_ACCESS: bool = false;

pub(crate) const fn g17_gfx1_pre_idle_entry_evidence(
    target: G17Gfx1PowerEvidenceTarget,
) -> G17Gfx1PreIdleEntryEvidence {
    match target {
        G17Gfx1PowerEvidenceTarget::G17P => G17Gfx1PreIdleEntryEvidence {
            helper_text_offset: 0x0003_5f5c,
            helper_object_vm_offset: 0x000f_90f0,
            direct_pio_prefix_before_helper: false,
        },
        G17Gfx1PowerEvidenceTarget::G17G => G17Gfx1PreIdleEntryEvidence {
            helper_text_offset: 0x0003_5db4,
            helper_object_vm_offset: 0x000f_3c30,
            direct_pio_prefix_before_helper: false,
        },
        G17Gfx1PowerEvidenceTarget::G17S => G17Gfx1PreIdleEntryEvidence {
            helper_text_offset: 0x0003_b1d8,
            helper_object_vm_offset: 0x0010_0df0,
            direct_pio_prefix_before_helper: true,
        },
    }
}

/// Semantic G17 PIO types used by the pinned G17S GFX1 transition callback.
///
/// The firmware's 52-entry name/type table and type-indexed 0x28-byte PIO
/// descriptor copier directly pair these names and numeric values. Independently
/// pinned G17G/G17P/G17S host mappings are modeled below.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum G17PioSlot {
    GpcAicSwInt = 0x00,
    PushTelemetryDashboardConfigSpace = 0x0b,
    FenderRegsMcw = 0x24,
}

/// Exact PIO descriptor layout used by the pinned firmware's type copier.
pub(crate) const G17S_PIO_DESCRIPTOR_SIZE: u32 = 0x28;
pub(crate) const G17S_PIO_DESCRIPTOR_POINTER_OFFSET: u32 = 0x08;

/// Offset of one type's descriptor from the firmware PIO descriptor mirror.
pub(crate) const fn g17s_pio_descriptor_offset(slot: G17PioSlot) -> u32 {
    slot as u32 * G17S_PIO_DESCRIPTOR_SIZE
}

/// Offset of the callback's mapped-pointer field from the descriptor mirror.
pub(crate) const fn g17s_pio_mapped_pointer_offset(slot: G17PioSlot) -> u32 {
    g17s_pio_descriptor_offset(slot) + G17S_PIO_DESCRIPTOR_POINTER_OFFSET
}

/// One exact 32-bit PIO operation in firmware execution order.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17PioOp {
    Write {
        slot: G17PioSlot,
        offset: u32,
        value: u32,
    },
    SetBits {
        slot: G17PioSlot,
        offset: u32,
        mask: u32,
    },
}

/// Ordered PIO prefix for G17S GFX1 `0x21 -> SLEEP`.
///
/// Firmware-internal synchronization and state checks occur after this prefix
/// and before the idle poll. Consequently this is not an executable sequence.
pub(crate) const G17S_GFX1_21_TO_SLEEP_PIO_PREFIX: [G17PioOp; 2] = [
    G17PioOp::Write {
        slot: G17PioSlot::GpcAicSwInt,
        offset: 0x430,
        value: 0x0800_0000,
    },
    G17PioOp::Write {
        slot: G17PioSlot::PushTelemetryDashboardConfigSpace,
        offset: 0x354,
        value: 0x0020_0000,
    },
];

/// Ordered PIO prefix for G17S GFX1 `SLEEP/NAP -> 0x21`.
pub(crate) const G17S_GFX1_LOW_POWER_TO_21_PIO_PREFIX: [G17PioOp; 3] = [
    G17PioOp::Write {
        slot: G17PioSlot::GpcAicSwInt,
        offset: 0x630,
        value: 0x0800_0000,
    },
    G17PioOp::SetBits {
        slot: G17PioSlot::PushTelemetryDashboardConfigSpace,
        offset: 0x758,
        mask: 0x0000_0b00,
    },
    G17PioOp::Write {
        slot: G17PioSlot::PushTelemetryDashboardConfigSpace,
        offset: 0x254,
        value: 0x0020_0000,
    },
];

/// Compatibility alias retained for the original, reversed direction name.
pub(crate) const G17S_GFX1_SLEEP_TO_21_PIO_PREFIX: [G17PioOp; 2] =
    G17S_GFX1_21_TO_SLEEP_PIO_PREFIX;
/// Compatibility alias retained for the original, reversed direction name.
pub(crate) const G17S_GFX1_21_TO_LOW_POWER_PIO_PREFIX: [G17PioOp; 3] =
    G17S_GFX1_LOW_POWER_TO_21_PIO_PREFIX;

/// Validate only the exact observed `0x21 -> SLEEP` PIO prefix.
pub(crate) fn validate_g17s_gfx1_21_to_sleep_pio_prefix(ops: &[G17PioOp]) -> bool {
    ops == G17S_GFX1_21_TO_SLEEP_PIO_PREFIX
}

/// Validate only the exact observed `SLEEP/NAP -> 0x21` PIO prefix.
pub(crate) fn validate_g17s_gfx1_low_power_to_21_pio_prefix(ops: &[G17PioOp]) -> bool {
    ops == G17S_GFX1_LOW_POWER_TO_21_PIO_PREFIX
}

/// Compatibility wrapper retained for the original, reversed direction name.
pub(crate) fn validate_g17s_gfx1_sleep_to_21_pio_prefix(ops: &[G17PioOp]) -> bool {
    validate_g17s_gfx1_21_to_sleep_pio_prefix(ops)
}

/// Compatibility wrapper retained for the original, reversed direction name.
pub(crate) fn validate_g17s_gfx1_21_to_low_power_pio_prefix(ops: &[G17PioOp]) -> bool {
    validate_g17s_gfx1_low_power_to_21_pio_prefix(ops)
}

/// One exact firmware-internal operation between the G17S PIO writes and poll.
///
/// Addresses are offsets from the firmware text or global-data virtual base.
/// Helper calls deliberately remain opaque: their machine-code targets and
/// object addresses are proven, but Linux ownership and equivalent semantics
/// are not.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17SFirmwareOp {
    OpaqueHelperCall {
        helper_text_offset: u32,
        object_global_offset: u32,
    },
    AtomicAdd32 {
        global_offset: u32,
        addend: i32,
    },
    RejectBothNonZero32 {
        root_global_offset: u32,
        pointer_offset: u32,
        field_offset: u32,
        global_offset: u32,
    },
    AtomicSetBits64 {
        global_offset: u32,
        mask: u64,
    },
}

/// Ordered G17S operations after the `0x21 -> SLEEP` PIO writes and before the
/// type-0x24 idle poll.
pub(crate) const G17S_GFX1_PRE_IDLE_FIRMWARE_OPS: [G17SFirmwareOp; 6] = [
    G17SFirmwareOp::OpaqueHelperCall {
        helper_text_offset: 0x0003_b1d8,
        object_global_offset: 0x0000_0df0,
    },
    G17SFirmwareOp::AtomicAdd32 {
        global_offset: 0x0000_21e0,
        addend: -1,
    },
    G17SFirmwareOp::RejectBothNonZero32 {
        root_global_offset: 0x0000_0e08,
        pointer_offset: 0x481,
        field_offset: 0x38,
        global_offset: 0x0000_2d10,
    },
    G17SFirmwareOp::AtomicSetBits64 {
        global_offset: 0x0000_11a0,
        mask: 0x0800_0000,
    },
    G17SFirmwareOp::OpaqueHelperCall {
        helper_text_offset: 0x0003_a8e8,
        object_global_offset: 0x0000_11a0,
    },
    G17SFirmwareOp::OpaqueHelperCall {
        helper_text_offset: 0x0003_b1d8,
        object_global_offset: 0x0000_0dd8,
    },
];

/// Validate only the byte-confirmed G17S pre-idle firmware-operation order.
pub(crate) fn validate_g17s_gfx1_pre_idle_firmware_ops(ops: &[G17SFirmwareOp]) -> bool {
    ops == G17S_GFX1_PRE_IDLE_FIRMWARE_OPS
}

/// G17 host targets with the byte-identical slot-0x24 relative-map record.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17PioMapEvidenceTarget {
    G17P,
    G17G,
    G17S,
}

pub(crate) const G17_PIO_SLOT_24_MAP_EVIDENCE_TARGETS: [G17PioMapEvidenceTarget; 3] = [
    G17PioMapEvidenceTarget::G17P,
    G17PioMapEvidenceTarget::G17G,
    G17PioMapEvidenceTarget::G17S,
];

/// Exact common slot-0x24 relative mapping; the complete family tables differ.
pub(crate) const G17_PIO_SLOT_24_RELATIVE_OFFSET: u32 = 0x00d4_0000;
pub(crate) const G17_PIO_SLOT_24_SIZE: u32 = 0x4000;

/// Complete ordered type list of the pinned G17S 19-record relative table.
///
/// Type 0x0b is absent; G17S installs that descriptor later in its target
/// configure override instead of obtaining it from the SGX-relative table.
pub(crate) const G17S_HOST_PIO_RELATIVE_TABLE_TYPES: [u8; 19] = [
    0x11, 0x35, 0x1f, 0x22, 0x24, 0x26, 0x27, 0x21, 0x25, 0x28, 0x2a, 0x30, 0x14, 0x15,
    0x12, 0x13, 0x18, 0x17, 0x32,
];

/// G17 targets whose physical SGX and host-created PIO bindings are pinned.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17HostPioPhysicalEvidenceTarget {
    G17P,
    G17G,
    G17S,
}

pub(crate) const G17_HOST_PIO_PHYSICAL_EVIDENCE_TARGETS: [G17HostPioPhysicalEvidenceTarget; 3] =
    [
        G17HostPioPhysicalEvidenceTarget::G17P,
        G17HostPioPhysicalEvidenceTarget::G17G,
        G17HostPioPhysicalEvidenceTarget::G17S,
    ];

/// Exact opaque `meta-sw-interrupt` tuple copied by the pinned host.
///
/// Only the first u64 has a proven use here: the host aligns it down to the
/// host I/O page size and installs it as PIO type 0. The second u64 and
/// trailing u32 deliberately remain unnamed.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17MetaSwInterruptEvidence {
    pub(crate) first_u64: u64,
    pub(crate) second_u64: u64,
    pub(crate) trailing_u32: u32,
}

/// One address/size pair from the Apple DeviceTree `reg` property.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17AdtRegisterRange {
    pub(crate) phys_base: u64,
    pub(crate) size: u32,
}

/// Literal power-ownership property on one D93AP RTBuddy nub.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17AdtPowerManagementProperty {
    UserPowerManaged,
    PowerManaged,
}

/// Exact public D93AP/T8140 topology required before A18 Pro live bring-up.
///
/// This is parsed from `DeviceTree.d93ap.im4p` in iPhone17,1 23G83
/// (raw payload SHA-256 c7613024b817687fb46cecefb8086a77bfa5a3a98153218452ba5ae2309cbb15).
/// The zero-count/all-zero performance table is intentional: it is not a
/// legacy GPU OPP ladder and must not be passed to that initialization path.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct T8140G17PAdtTopology {
    pub(crate) sgx_regs: [G17AdtRegisterRange; 2],
    pub(crate) sgx_power_gates: [u32; 2],
    pub(crate) sgx_power_gate_parents: [Option<u32>; 2],
    pub(crate) gfx_power_domain: u32,
    pub(crate) gfx_power_domain_parents: [u32; 2],
    pub(crate) clock_id: u32,
    pub(crate) sgx_interrupts: [u32; 8],
    pub(crate) sgx_interrupts_valid: u32,
    pub(crate) gfx_asc_regs: [G17AdtRegisterRange; 2],
    pub(crate) gfx_asc_power_gate: u32,
    pub(crate) gfx_asc_power_gate_parent: u32,
    pub(crate) gfx_asc_interrupts: [u32; 4],
    pub(crate) gfx_power_management: G17AdtPowerManagementProperty,
    pub(crate) gfx1_asc_regs: [G17AdtRegisterRange; 2],
    pub(crate) gfx1_asc_power_gate: u32,
    pub(crate) gfx1_asc_power_gate_parent: u32,
    pub(crate) gfx1_asc_interrupts: [u32; 4],
    pub(crate) gfx1_power_management: G17AdtPowerManagementProperty,
    pub(crate) shared_iommu_mapper: bool,
    pub(crate) meta_sw_interrupt: G17MetaSwInterruptEvidence,
    pub(crate) perf_state_count: u32,
    pub(crate) gpu_num_perf_states: u32,
    pub(crate) perf_state_record_count: u32,
    pub(crate) perf_state_records_all_zero: bool,
    pub(crate) perf_states_sram_present: bool,
}

/// One role-local ASCWrap v6 provider decoded from exact D93AP topology.
///
/// `wrapper_regs` contains the mailbox block at
/// [`T8140_ASCWRAP_V6_MAILBOX_OFFSET`]. `iorvbar_regs` is the independent
/// eight-byte IORVBAR window and is deliberately not part of mailbox MMIO.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct T8140G17PAscProvider {
    pub(crate) role: T8140G17PAscRole,
    pub(crate) wrapper_regs: G17AdtRegisterRange,
    pub(crate) iorvbar_regs: G17AdtRegisterRange,
    pub(crate) send_empty_irq: u32,
    pub(crate) recv_not_empty_irq: u32,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum T8140G17PAscRole {
    Gfx,
    Gfx1,
}

/// Named PMGR device selected by one D93AP GPU power-gate ID.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum T8140G17PPmgrDeviceName {
    GfxSgx,
    GfxBusy,
    GfxAsc,
    GfxAsc1,
}

/// Exact PMGR membership for one D93AP GPU power-gate ID.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct T8140G17PPmgrGate {
    pub(crate) device_id: u32,
    pub(crate) name: T8140G17PPmgrDeviceName,
    pub(crate) flags: u32,
    pub(crate) index: u32,
    pub(crate) alias: Option<u32>,
}

/// D93AP's SGX and two ASC nubs select these named `pmgr1,t8140` devices.
/// This proves membership only; it does not prove a Linux enable sequence or
/// whether RTBuddy firmware retains ownership of any gate.
pub(crate) const D93AP_T8140_G17P_PMGR_GATES: [T8140G17PPmgrGate; 4] = [
    T8140G17PPmgrGate {
        device_id: 235,
        name: T8140G17PPmgrDeviceName::GfxSgx,
        flags: 16,
        index: 0,
        alias: Some(40),
    },
    T8140G17PPmgrGate {
        device_id: 236,
        name: T8140G17PPmgrDeviceName::GfxBusy,
        flags: 16,
        index: 0,
        alias: None,
    },
    T8140G17PPmgrGate {
        device_id: 237,
        name: T8140G17PPmgrDeviceName::GfxAsc,
        flags: 16,
        index: 0,
        alias: Some(40),
    },
    T8140G17PPmgrGate {
        device_id: 321,
        name: T8140G17PPmgrDeviceName::GfxAsc1,
        flags: 16,
        index: 0,
        alias: Some(40),
    },
];

pub(crate) const T8140_G17P_PMGR_GATE_TOPOLOGY_PROVEN: bool = true;
pub(crate) const T8140_G17P_LIVE_POWER_SEQUENCE_PROVEN: bool = false;

/// ASCWrap v6 retains the ASC mailbox-v4 register ABI at wrapper + 0x8000.
pub(crate) const T8140_ASCWRAP_V6_MAILBOX_OFFSET: u32 = 0x8000;

pub(crate) const D93AP_T8140_G17P_ASC_PROVIDERS: [T8140G17PAscProvider; 2] = [
    T8140G17PAscProvider {
        role: T8140G17PAscRole::Gfx,
        wrapper_regs: G17AdtRegisterRange {
            phys_base: 0x2_7260_0000,
            size: 0x0008_8000,
        },
        iorvbar_regs: G17AdtRegisterRange {
            phys_base: 0x2_7205_0000,
            size: 8,
        },
        send_empty_irq: 993,
        recv_not_empty_irq: 996,
    },
    T8140G17PAscProvider {
        role: T8140G17PAscRole::Gfx1,
        wrapper_regs: G17AdtRegisterRange {
            phys_base: 0x2_72e0_0000,
            size: 0x0008_8000,
        },
        iorvbar_regs: G17AdtRegisterRange {
            phys_base: 0x2_7285_0000,
            size: 8,
        },
        send_empty_irq: 997,
        recv_not_empty_irq: 1000,
    },
];

pub(crate) const D93AP_T8140_G17P_ADT_TOPOLOGY: T8140G17PAdtTopology = T8140G17PAdtTopology {
    sgx_regs: [
        G17AdtRegisterRange {
            phys_base: 0x2_7000_0000,
            size: 0x0400_0000,
        },
        G17AdtRegisterRange {
            phys_base: 0x2_70d0_0000,
            size: 0x0012_c000,
        },
    ],
    sgx_power_gates: [235, 236],
    sgx_power_gate_parents: [Some(40), None],
    gfx_power_domain: 40,
    gfx_power_domain_parents: [29, 31],
    clock_id: 351,
    sgx_interrupts: [975, 976, 977, 978, 1001, 1003, 990, 992],
    sgx_interrupts_valid: 0xdf,
    gfx_asc_regs: [
        G17AdtRegisterRange {
            phys_base: 0x2_7260_0000,
            size: 0x0008_8000,
        },
        G17AdtRegisterRange {
            phys_base: 0x2_7205_0000,
            size: 8,
        },
    ],
    gfx_asc_power_gate: 237,
    gfx_asc_power_gate_parent: 40,
    gfx_asc_interrupts: [994, 993, 996, 995],
    gfx_power_management: G17AdtPowerManagementProperty::UserPowerManaged,
    gfx1_asc_regs: [
        G17AdtRegisterRange {
            phys_base: 0x2_72e0_0000,
            size: 0x0008_8000,
        },
        G17AdtRegisterRange {
            phys_base: 0x2_7285_0000,
            size: 8,
        },
    ],
    gfx1_asc_power_gate: 321,
    gfx1_asc_power_gate_parent: 40,
    gfx1_asc_interrupts: [998, 997, 1000, 999],
    gfx1_power_management: G17AdtPowerManagementProperty::PowerManaged,
    shared_iommu_mapper: true,
    meta_sw_interrupt: G17MetaSwInterruptEvidence {
        first_u64: 0x3_0101_4048,
        second_u64: 0x3_0101_4248,
        trailing_u32: 0x4000,
    },
    perf_state_count: 0,
    gpu_num_perf_states: 2,
    perf_state_record_count: 16,
    perf_state_records_all_zero: true,
    perf_states_sram_present: false,
};

/// Do not begin A18 Pro live bring-up against an inferred or partial topology.
pub(crate) fn validate_t8140_g17p_adt_topology(observed: &T8140G17PAdtTopology) -> bool {
    observed == &D93AP_T8140_G17P_ADT_TOPOLOGY
}

/// Exact host-side action selected by one valid D93AP SGX event-source index.
///
/// These names describe only the dispatch proven in the pinned G17P host. The
/// drained event ring carries completion stamps, but no live run yet proves
/// that one Linux SKSM kick produces this ingress on real D93AP hardware.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum T8140G17PSgxHostAction {
    FirmwareEventZero,
    GpcHostInterrupts,
    DrainFirmwareRings,
    McwInterrupt,
    AcceleratorVirtualC60,
}

/// Physical D93AP interrupt selected by one host event-source index.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct T8140G17PSgxHostIrqRoute {
    pub(crate) source_index: u8,
    pub(crate) aic_irq: u32,
    pub(crate) action: T8140G17PSgxHostAction,
}

/// The complete valid SGX event-source map from the signed 23G83 D93AP tree
/// and `AGXFirmware::handleEvent` in pinned com.apple.AGXG17P.
pub(crate) const D93AP_T8140_G17P_SGX_HOST_IRQ_ROUTES: [T8140G17PSgxHostIrqRoute; 7] = [
    T8140G17PSgxHostIrqRoute {
        source_index: 0,
        aic_irq: 975,
        action: T8140G17PSgxHostAction::FirmwareEventZero,
    },
    T8140G17PSgxHostIrqRoute {
        source_index: 1,
        aic_irq: 976,
        action: T8140G17PSgxHostAction::GpcHostInterrupts,
    },
    T8140G17PSgxHostIrqRoute {
        source_index: 2,
        aic_irq: 977,
        action: T8140G17PSgxHostAction::GpcHostInterrupts,
    },
    T8140G17PSgxHostIrqRoute {
        source_index: 3,
        aic_irq: 978,
        action: T8140G17PSgxHostAction::GpcHostInterrupts,
    },
    T8140G17PSgxHostIrqRoute {
        source_index: 4,
        aic_irq: 1001,
        action: T8140G17PSgxHostAction::DrainFirmwareRings,
    },
    T8140G17PSgxHostIrqRoute {
        source_index: 6,
        aic_irq: 990,
        action: T8140G17PSgxHostAction::McwInterrupt,
    },
    T8140G17PSgxHostIrqRoute {
        source_index: 7,
        aic_irq: 992,
        action: T8140G17PSgxHostAction::AcceleratorVirtualC60,
    },
];

/// Both proven ingress mechanisms for the pinned host's main firmware event
/// rings converge on SGX source 4.
///
/// A physical IRQ invokes the filter event source directly. RTBuddy AKF
/// message type 2 calls `IOFilterInterruptEventSource::signalInterrupt()` on
/// the source selected from the interrupt-count configuration; D93AP has eight
/// interrupt words, selecting source 4. Both paths reach
/// `AGXFirmware::handleEvent(4)`, clear outstanding firmware interrupts, and
/// drain both role-local firmware event rings.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct T8140G17PFirmwareEventIngress {
    pub(crate) physical_aic_irq: u32,
    pub(crate) source_index: u8,
    pub(crate) rtbuddy_akf_message_type: u8,
}

pub(crate) const D93AP_T8140_G17P_FIRMWARE_EVENT_INGRESS: T8140G17PFirmwareEventIngress =
    T8140G17PFirmwareEventIngress {
        physical_aic_irq: 1001,
        source_index: 4,
        rtbuddy_akf_message_type: 2,
    };

/// The platform event-source and firmware-ring ingress mapping is exact; the
/// live SKSM completion handshake still must remain fail-closed.
pub(crate) const T8140_G17P_SGX_EVENT_SOURCE_MAP_PROVEN: bool = true;
pub(crate) const T8140_G17P_FIRMWARE_EVENT_INGRESS_PROVEN: bool = true;
pub(crate) const T8140_G17P_SKSM_COMPLETION_IRQ_PROVEN: bool = false;

/// Resolve only a valid interrupt from an exact D93AP topology.
///
/// Raw slot 5 contains IRQ 1003 but bit 5 is clear in `interrupts-valid`, so
/// it is deliberately rejected rather than exposed as a Linux resource.
pub(crate) fn decode_t8140_g17p_sgx_host_irq_route(
    observed: &T8140G17PAdtTopology,
    source_index: u8,
) -> Option<T8140G17PSgxHostIrqRoute> {
    if !validate_t8140_g17p_adt_topology(observed) || source_index >= 8 {
        return None;
    }

    let valid = (observed.sgx_interrupts_valid & (1u32 << u32::from(source_index))) != 0;
    if !valid {
        return None;
    }

    D93AP_T8140_G17P_SGX_HOST_IRQ_ROUTES
        .iter()
        .copied()
        .find(|route| route.source_index == source_index)
}

/// Decode the two independently owned mailbox providers from D93AP topology.
///
/// AppleA7IOP selects raw interrupt index 1 for send-empty and index 2 for
/// receive-not-empty. The other two wrapper interrupts remain unexposed until
/// a host consumer for them is proven. Any partial or inferred topology is
/// rejected before resources are returned.
pub(crate) fn decode_t8140_g17p_asc_providers(
    observed: &T8140G17PAdtTopology,
) -> Option<[T8140G17PAscProvider; 2]> {
    if !validate_t8140_g17p_adt_topology(observed) {
        return None;
    }

    Some([
        T8140G17PAscProvider {
            role: T8140G17PAscRole::Gfx,
            wrapper_regs: observed.gfx_asc_regs[0],
            iorvbar_regs: observed.gfx_asc_regs[1],
            send_empty_irq: observed.gfx_asc_interrupts[1],
            recv_not_empty_irq: observed.gfx_asc_interrupts[2],
        },
        T8140G17PAscProvider {
            role: T8140G17PAscRole::Gfx1,
            wrapper_regs: observed.gfx1_asc_regs[0],
            iorvbar_regs: observed.gfx1_asc_regs[1],
            send_empty_irq: observed.gfx1_asc_interrupts[1],
            recv_not_empty_irq: observed.gfx1_asc_interrupts[2],
        },
    ])
}

// -------------------------------------------------------------------------
// T8140 topology as a *Linux* device tree observes it.
//
// The pinned D93AP topology above records ADT `reg` values, which are
// arm-io child-bus addresses. The J700 Linux device tree (and the live J700
// bring-up) shows the same devices at CPU physical addresses shifted by the
// arm-io `ranges` translation: SGX child 0x2_7000_0000 <-> CPU 0x4_8000_0000,
// GFX ASC child 0x2_7260_0000 <-> CPU 0x4_8260_0000. A Linux platform
// decoder therefore observes CPU addresses and must translate them back
// before they can be compared against the pinned child-bus constants.
// -------------------------------------------------------------------------

/// The J700/T8140 arm-io `ranges` translation from ADT child-bus addresses to
/// CPU physical addresses (CPU = child + offset). Verified on the live J700
/// (Mac17,5) against both the SGX and both ASC wrapper windows.
pub(crate) const T8140_ARM_IO_CPU_TRANSLATION_OFFSET: u64 = 0x2_1000_0000;

/// One ASCWrap v6 provider as observed from a Linux device tree: the
/// mailbox node's two `reg` windows (CPU physical) and its two named
/// interrupts. `role_is_gfx1` comes from the node's `apple,firmware-role`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct T8140G17PLinuxDtAscObservation {
    pub(crate) role_is_gfx1: bool,
    pub(crate) wrapper_cpu_base: u64,
    pub(crate) wrapper_size: u64,
    pub(crate) iorvbar_cpu_base: u64,
    pub(crate) iorvbar_size: u64,
    pub(crate) send_empty_irq: u32,
    pub(crate) recv_not_empty_irq: u32,
}

/// The dual-provider observation a Linux platform decoder can actually make
/// from the T8140 GPU node's `mboxes` at probe time.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct T8140G17PLinuxDtTopology {
    pub(crate) gfx: T8140G17PLinuxDtAscObservation,
    pub(crate) gfx1: T8140G17PLinuxDtAscObservation,
}

fn linux_dt_asc_provider(
    role: T8140G17PAscRole,
    observed: &T8140G17PLinuxDtAscObservation,
) -> Option<T8140G17PAscProvider> {
    let expected_gfx1 = matches!(role, T8140G17PAscRole::Gfx1);
    if observed.role_is_gfx1 != expected_gfx1 {
        return None;
    }

    Some(T8140G17PAscProvider {
        role,
        wrapper_regs: G17AdtRegisterRange {
            phys_base: observed
                .wrapper_cpu_base
                .checked_sub(T8140_ARM_IO_CPU_TRANSLATION_OFFSET)?,
            size: observed.wrapper_size.try_into().ok()?,
        },
        iorvbar_regs: G17AdtRegisterRange {
            phys_base: observed
                .iorvbar_cpu_base
                .checked_sub(T8140_ARM_IO_CPU_TRANSLATION_OFFSET)?,
            size: observed.iorvbar_size.try_into().ok()?,
        },
        send_empty_irq: observed.send_empty_irq,
        recv_not_empty_irq: observed.recv_not_empty_irq,
    })
}

/// Decode the two mailbox providers from a Linux DT observation.
///
/// This only reshapes and bus-translates what the device tree actually says;
/// it deliberately performs no comparison against the pinned D93AP constants.
/// Admission stays with the caller (`g17_power_preflight`), which compares
/// the decoded providers against the pinned expectation, so an inferred or
/// partial observation can never substitute for the exact one.
pub(crate) fn decode_t8140_g17p_asc_providers_from_linux_dt(
    observed: &T8140G17PLinuxDtTopology,
) -> Option<[T8140G17PAscProvider; 2]> {
    Some([
        linux_dt_asc_provider(T8140G17PAscRole::Gfx, &observed.gfx)?,
        linux_dt_asc_provider(T8140G17PAscRole::Gfx1, &observed.gfx1)?,
    ])
}

/// Source from which the pinned host constructs one physical PIO binding.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17HostPioBindingSource {
    MetaSwInterrupt,
    SgxRelativeOffsetTable,
    TargetConfigureConstant,
}

/// One exact physical binding created by the pinned macOS host.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17HostPioPhysicalBinding {
    pub(crate) phys_base: u64,
    pub(crate) size: u32,
    pub(crate) source: G17HostPioBindingSource,
}

/// Exact absolute PIO assignments made by a target configure override.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17HostPioTargetConfigureEvidence {
    pub(crate) slot_0_phys_base: u64,
    pub(crate) slot_0b_phys_base: u64,
    pub(crate) slot_size: u32,
}

/// Exact target input used to derive the pinned host's physical PIO map.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17HostPioPhysicalEvidence {
    pub(crate) sgx_phys_base: u64,
    pub(crate) host_io_page_size: u32,
    pub(crate) meta_sw_interrupt: G17MetaSwInterruptEvidence,
    pub(crate) target_configure: Option<G17HostPioTargetConfigureEvidence>,
}

/// The macOS host constructs these mappings, but Linux ownership is unproved.
pub(crate) const G17_HOST_PIO_LINUX_MMIO_OWNERSHIP_PROVEN: bool = false;

pub(crate) const fn g17_host_pio_physical_evidence(
    target: G17HostPioPhysicalEvidenceTarget,
) -> G17HostPioPhysicalEvidence {
    match target {
        G17HostPioPhysicalEvidenceTarget::G17P => G17HostPioPhysicalEvidence {
            sgx_phys_base: D93AP_T8140_G17P_ADT_TOPOLOGY.sgx_regs[0].phys_base,
            host_io_page_size: 0x4000,
            meta_sw_interrupt: D93AP_T8140_G17P_ADT_TOPOLOGY.meta_sw_interrupt,
            target_configure: None,
        },
        G17HostPioPhysicalEvidenceTarget::G17G => G17HostPioPhysicalEvidence {
            sgx_phys_base: 0x3_7000_0000,
            host_io_page_size: 0x4000,
            meta_sw_interrupt: G17MetaSwInterruptEvidence {
                first_u64: 0x3_8101_4048,
                second_u64: 0x3_8101_4248,
                trailing_u32: 0x4000,
            },
            target_configure: None,
        },
        G17HostPioPhysicalEvidenceTarget::G17S => G17HostPioPhysicalEvidence {
            sgx_phys_base: G17S_CAPTURED_SGX_PHYS_BASE,
            host_io_page_size: 0x4000,
            meta_sw_interrupt: G17MetaSwInterruptEvidence {
                first_u64: 0x2_8041_4084,
                second_u64: 0x2_8041_4284,
                trailing_u32: 0x80,
            },
            target_configure: Some(G17HostPioTargetConfigureEvidence {
                slot_0_phys_base: 0x2_8041_4000,
                slot_0b_phys_base: 0x2_8425_8000,
                slot_size: 0x4000,
            }),
        },
    }
}

/// Derive only the physical bindings established by the pinned target host.
///
/// G17G/G17P type 0 comes from `meta-sw-interrupt`; the G17S generic path does
/// the same, then its target override reasserts the same aligned address as an
/// absolute constant. Type 0x24 comes from the target SGX base plus its exact
/// relative-table record. Type 0x0b remains `None` on G17G/G17P, but the G17S
/// target override installs its exact absolute address after the 19-record
/// relative table omits it.
pub(crate) const fn g17_host_pio_physical_binding(
    target: G17HostPioPhysicalEvidenceTarget,
    slot: G17PioSlot,
) -> Option<G17HostPioPhysicalBinding> {
    let evidence = g17_host_pio_physical_evidence(target);

    match slot {
        G17PioSlot::GpcAicSwInt => match evidence.target_configure {
            Some(target_configure) => Some(G17HostPioPhysicalBinding {
                phys_base: target_configure.slot_0_phys_base,
                size: target_configure.slot_size,
                source: G17HostPioBindingSource::TargetConfigureConstant,
            }),
            None => Some(G17HostPioPhysicalBinding {
                phys_base: evidence.meta_sw_interrupt.first_u64
                    & !((evidence.host_io_page_size as u64) - 1),
                size: evidence.host_io_page_size,
                source: G17HostPioBindingSource::MetaSwInterrupt,
            }),
        },
        G17PioSlot::PushTelemetryDashboardConfigSpace => match evidence.target_configure {
            Some(target_configure) => Some(G17HostPioPhysicalBinding {
                phys_base: target_configure.slot_0b_phys_base,
                size: target_configure.slot_size,
                source: G17HostPioBindingSource::TargetConfigureConstant,
            }),
            None => None,
        },
        G17PioSlot::FenderRegsMcw => Some(G17HostPioPhysicalBinding {
            phys_base: evidence.sgx_phys_base + G17_PIO_SLOT_24_RELATIVE_OFFSET as u64,
            size: G17_PIO_SLOT_24_SIZE,
            source: G17HostPioBindingSource::SgxRelativeOffsetTable,
        }),
    }
}

/// Validate an observed binding, including the proven absence of type 0x0b.
pub(crate) fn validate_g17_host_pio_physical_binding(
    target: G17HostPioPhysicalEvidenceTarget,
    slot: G17PioSlot,
    binding: Option<G17HostPioPhysicalBinding>,
) -> bool {
    binding == g17_host_pio_physical_binding(target, slot)
}

/// Captured T6050/J714s SGX base and the derived G17S poll address.
///
/// G17G/G17P physical bases are deliberately not inferred from this G17S ADT.
pub(crate) const G17S_CAPTURED_SGX_PHYS_BASE: u64 = 0x21_0000_0000;
pub(crate) const G17S_GFX1_IDLE_STATUS_PHYS: u64 =
    G17S_CAPTURED_SGX_PHYS_BASE + G17_PIO_SLOT_24_RELATIVE_OFFSET as u64;

/// Exact common G17 GFX1 idle-poll fields.
pub(crate) const G17_GFX1_IDLE_POLL_MASK: u32 = 1;
pub(crate) const G17_GFX1_IDLE_POLL_EXPECTED: u32 = 1;
pub(crate) const G17_GFX1_IDLE_POLL_TIMEOUT_TICKS: u32 = 0x1770;
/// Maximum printed by the pinned firmware for the `0x1770`-tick deadline.
pub(crate) const G17_GFX1_IDLE_POLL_REPORTED_MAX_US: u32 = 250;

/// G17S-compatible names retained for the captured physical-address witness.
pub(crate) const G17S_GFX1_IDLE_POLL_MASK: u32 = G17_GFX1_IDLE_POLL_MASK;
pub(crate) const G17S_GFX1_IDLE_POLL_EXPECTED: u32 = G17_GFX1_IDLE_POLL_EXPECTED;
pub(crate) const G17S_GFX1_IDLE_POLL_TIMEOUT_TICKS: u32 = G17_GFX1_IDLE_POLL_TIMEOUT_TICKS;
pub(crate) const G17S_GFX1_IDLE_POLL_REPORTED_MAX_US: u32 = G17_GFX1_IDLE_POLL_REPORTED_MAX_US;

/// Apply the pinned G17 firmware's timebase-delta reporting conversion.
///
/// This is evidence for its diagnostic output, not a Linux timeout source.
pub(crate) const fn g17_gfx1_report_timebase_delta_us(delta_ticks: u32) -> u32 {
    ((delta_ticks as u64 * 0xaaaa_aaab) >> 36) as u32
}

/// Apply the shared conversion to the original G17S witness.
pub(crate) const fn g17s_gfx1_report_timebase_delta_us(delta_ticks: u32) -> u32 {
    g17_gfx1_report_timebase_delta_us(delta_ticks)
}

/// Test the exact success predicate used by the common G17 GFX1 idle poll.
pub(crate) const fn g17_gfx1_idle_poll_satisfied(
    _target: G17Gfx1PowerEvidenceTarget,
    status: u32,
) -> bool {
    status & G17_GFX1_IDLE_POLL_MASK == G17_GFX1_IDLE_POLL_EXPECTED
}

/// Test the G17S witness through the common G17 idle-poll predicate.
pub(crate) const fn g17s_gfx1_idle_poll_satisfied(status: u32) -> bool {
    g17_gfx1_idle_poll_satisfied(G17Gfx1PowerEvidenceTarget::G17S, status)
}

/// Fail-closed result for the bounded G17 GFX1 transition grammar.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum PowerTransitionError {
    /// Direct SLEEP-to-NAP and NAP-to-SLEEP paths reach firmware aborts.
    DirectSleepNapIsFatal,
    /// No other edge was established by the pinned transition callback.
    UnprovenTransition,
}

/// Validate only the four independently proven G17 bridge edges.
///
/// Same-state requests and unknown values are deliberately rejected. This
/// proves the transition grammar, not target helper equivalence or ownership.
pub(crate) const fn validate_g17_gfx1_power_transition(
    _target: G17Gfx1PowerEvidenceTarget,
    old: u8,
    new: u8,
) -> Result<(), PowerTransitionError> {
    if (old == GFX1_POWER_SLEEP && new == GFX1_POWER_NAP)
        || (old == GFX1_POWER_NAP && new == GFX1_POWER_SLEEP)
    {
        return Err(PowerTransitionError::DirectSleepNapIsFatal);
    }

    if ((old == GFX1_POWER_SLEEP || old == GFX1_POWER_NAP) && new == GFX1_POWER_INTERMEDIATE_21)
        || (old == GFX1_POWER_INTERMEDIATE_21 && (new == GFX1_POWER_SLEEP || new == GFX1_POWER_NAP))
    {
        Ok(())
    } else {
        Err(PowerTransitionError::UnprovenTransition)
    }
}

/// Validate the G17S witness through the common G17 transition grammar.
pub(crate) const fn validate_g17s_gfx1_power_transition(
    old: u8,
    new: u8,
) -> Result<(), PowerTransitionError> {
    validate_g17_gfx1_power_transition(G17Gfx1PowerEvidenceTarget::G17S, old, new)
}

/// Target-specific locations independently establishing the callback-to-ACK
/// prefix in each pinned G17 GFX1 image.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17Gfx1CallbackAckEvidence {
    pub(crate) worker_text_offset: u32,
    pub(crate) release_helper_text_offset: u32,
    pub(crate) payload_mapper_text_offset: u32,
    pub(crate) sender_text_offset: u32,
}

pub(crate) const fn g17_gfx1_callback_ack_evidence(
    target: G17Gfx1PowerEvidenceTarget,
) -> G17Gfx1CallbackAckEvidence {
    match target {
        G17Gfx1PowerEvidenceTarget::G17P => G17Gfx1CallbackAckEvidence {
            worker_text_offset: 0x0003_b030,
            release_helper_text_offset: 0x0003_5484,
            payload_mapper_text_offset: 0x0003_8dc8,
            sender_text_offset: 0x0003_dc00,
        },
        G17Gfx1PowerEvidenceTarget::G17G => G17Gfx1CallbackAckEvidence {
            worker_text_offset: 0x0003_ae34,
            release_helper_text_offset: 0x0003_52e8,
            payload_mapper_text_offset: 0x0003_8bdc,
            sender_text_offset: 0x0003_da48,
        },
        G17Gfx1PowerEvidenceTarget::G17S => G17Gfx1CallbackAckEvidence {
            worker_text_offset: 0x0004_0258,
            release_helper_text_offset: 0x0003_a70c,
            payload_mapper_text_offset: 0x0003_e000,
            sender_text_offset: 0x0004_2f00,
        },
    }
}

/// Exact ordered prefix from one firmware power callback through its ACK.
///
/// Optional callbacks are skipped when their pointers are null. The release
/// helper's CASL body is exact, but its matching acquisition and ownership
/// contract are not. The worker can also run a target-state hook after the ACK;
/// neither that hook nor any later worker action is represented here.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17Gfx1CallbackAckStep {
    InvokeOptionalPreCallback,
    InvokeRegisteredCallbacks,
    InvokeOptionalPostCallback,
    ReleaseFirmwareLocalObjectCasl,
    MapCompletionPayload,
    SendType0bCompletionMessage,
}

pub(crate) const G17_GFX1_CALLBACK_ACK_PREFIX: [G17Gfx1CallbackAckStep; 6] = [
    G17Gfx1CallbackAckStep::InvokeOptionalPreCallback,
    G17Gfx1CallbackAckStep::InvokeRegisteredCallbacks,
    G17Gfx1CallbackAckStep::InvokeOptionalPostCallback,
    G17Gfx1CallbackAckStep::ReleaseFirmwareLocalObjectCasl,
    G17Gfx1CallbackAckStep::MapCompletionPayload,
    G17Gfx1CallbackAckStep::SendType0bCompletionMessage,
];

/// High message-type bits ORed with the mapped payload before the send.
pub(crate) const G17_GFX1_CALLBACK_ACK_MESSAGE_TYPE_BITS: u64 = 0x00b0_0000_0000_0000;
/// The only sender status for which the worker retries the ACK.
pub(crate) const G17_GFX1_CALLBACK_ACK_RETRY_STATUS: u32 = 0x8000_000e;

pub(crate) fn validate_g17_gfx1_callback_ack_prefix(
    _target: G17Gfx1PowerEvidenceTarget,
    steps: &[G17Gfx1CallbackAckStep],
) -> bool {
    steps == G17_GFX1_CALLBACK_ACK_PREFIX
}

/// Fail-closed results from the common internal-state-to-payload mapper.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17Gfx1CallbackAckError {
    /// Internal state 3 branches to a firmware abort instead of returning.
    FatalInternalState,
    /// The firmware's default is zero, but Linux must not accept an unknown.
    UnprovenInternalState,
}

/// Decode only independently established internal states and table entries.
///
/// The pinned mapper returns zero for other numeric inputs. Rejecting that
/// default prevents an unknown state from becoming an apparently valid ACK.
pub(crate) const fn g17_gfx1_callback_ack_payload(
    _target: G17Gfx1PowerEvidenceTarget,
    internal_state: u8,
) -> Result<u16, G17Gfx1CallbackAckError> {
    match internal_state {
        3 => Err(G17Gfx1CallbackAckError::FatalInternalState),
        4 => Ok(3),
        5 => Ok(0x203),
        6 | 7 => Ok(0),
        GFX1_POWER_SLEEP => Ok(0x201),
        GFX1_POWER_NAP => Ok(0),
        0x20 => Ok(0x10),
        GFX1_POWER_INTERMEDIATE_21 => Ok(0x20),
        _ => Err(G17Gfx1CallbackAckError::UnprovenInternalState),
    }
}

/// Host-side management-message routes established in all three pinned
/// generic RTBuddy images.
///
/// Despite the firmware-side callback-ACK terminology above, type 0x0b is
/// not the host's type-7 power ACK. The two messages wake different fields
/// belonging to different objects.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17HostManagementMessageRoute {
    /// `_handlePowerAck()` updates and wakes `RTBuddy + 0x158`.
    IopStatusPowerAck,
    /// The handler stores the low word and wakes management endpoint +0x108.
    Gfx1CallbackRemoteState,
}

/// Object containing the field used as the matching sleep/wakeup event.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17HostManagementWakeObject {
    RtBuddy,
    ManagementEndpoint,
}

/// Fail-closed result for an incoming management-message type not covered by
/// this bounded power-route model.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17HostManagementMessageError {
    UnprovenMessageType,
}

/// Target-pinned generic RTBuddy text locations proving the two host routes.
///
/// These offsets are relative to each image's `__TEXT_EXEC` VM address. The
/// independently extracted G17P, G17G, and G17S images normalize exactly.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17HostManagementRouteEvidence {
    pub(crate) management_handler_text_offset: u32,
    pub(crate) type0b_remote_state_branch_text_offset: u32,
    pub(crate) wait_remote_state_text_offset: u32,
    pub(crate) handle_power_ack_text_offset: u32,
    pub(crate) perform_power_change_text_offset: u32,
    pub(crate) set_iop_status_text_offset: u32,
}

pub(crate) const G17_HOST_MANAGEMENT_MESSAGE_TYPE_SHIFT: u32 = 52;
pub(crate) const G17_HOST_MANAGEMENT_MESSAGE_TYPE_MASK: u64 = 0xf;
pub(crate) const G17_HOST_POWER_ACK_MESSAGE_TYPE: u8 = 0x07;
pub(crate) const G17_HOST_GFX1_CALLBACK_MESSAGE_TYPE: u8 = 0x0b;
pub(crate) const G17_HOST_MANAGEMENT_REMOTE_STATE_WORD_OFFSET: u32 = 0x108;
pub(crate) const G17_HOST_RTBUDDY_IOP_STATUS_WORD_OFFSET: u32 = 0x158;

/// Target-specific GFX1 text offsets proving the callback endpoint dataflow.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17Gfx1CallbackEndpointEvidence {
    pub(crate) endpoint_request_store_text_offset: u32,
    pub(crate) endpoint_allocator_call_text_offset: u32,
    pub(crate) endpoint_allocator_sentinel_compare_text_offset: u32,
    pub(crate) endpoint_allocator_dynamic_store_text_offset: u32,
    pub(crate) endpoint_result_load_text_offset: u32,
    pub(crate) endpoint_handle_shift_text_offset: u32,
    pub(crate) callback_endpoint_store_text_offset: u32,
    pub(crate) callback_registration_load_text_offset: u32,
    pub(crate) callback_registration_context_text_offset: u32,
    pub(crate) callback_handler_text_offset: u32,
    pub(crate) callback_context_store_text_offset: u32,
    pub(crate) completion_context_load_text_offset: u32,
    pub(crate) completion_endpoint_load_text_offset: u32,
    pub(crate) completion_send_call_text_offset: u32,
}

pub(crate) const G17_GFX1_CALLBACK_REQUESTED_ENDPOINT_SLOT: u8 = 0;
pub(crate) const G17_GFX1_CALLBACK_DYNAMIC_ENDPOINT_SENTINEL: u8 = 0xff;
pub(crate) const G17_GFX1_CALLBACK_ENDPOINT_HANDLE_SHIFT: u32 = 16;
pub(crate) const G17_GFX1_CALLBACK_ENDPOINT_FIELD_OFFSET: u32 = 8;
pub(crate) const G17_GFX1_CALLBACK_CONTEXT_FIELD_OFFSET: u32 = 0x40;

/// The exact constructors request endpoint slot 0, not the allocator's 0xff
/// dynamic-slot sentinel. The successful path therefore retains zero, shifts
/// it by 16, stores it at callback-object +8, registers the same object as the
/// handler context, and later reloads that zero for the completion sender.
pub(crate) const G17_GFX1_CALLBACK_MANAGEMENT_ENDPOINT_LINK_PROVEN: bool = true;

pub(crate) const fn g17_gfx1_callback_endpoint_evidence(
    target: G17Gfx1PowerEvidenceTarget,
) -> G17Gfx1CallbackEndpointEvidence {
    match target {
        G17Gfx1PowerEvidenceTarget::G17P => G17Gfx1CallbackEndpointEvidence {
            endpoint_request_store_text_offset: 0x0003_2d7c,
            endpoint_allocator_call_text_offset: 0x0003_2d80,
            endpoint_allocator_sentinel_compare_text_offset: 0x0003_6c70,
            endpoint_allocator_dynamic_store_text_offset: 0x0003_6cc4,
            endpoint_result_load_text_offset: 0x0003_2da4,
            endpoint_handle_shift_text_offset: 0x0003_2e84,
            callback_endpoint_store_text_offset: 0x0003_2ea0,
            callback_registration_load_text_offset: 0x0003_2f20,
            callback_registration_context_text_offset: 0x0003_e088,
            callback_handler_text_offset: 0x0003_89e4,
            callback_context_store_text_offset: 0x0003_8af0,
            completion_context_load_text_offset: 0x0003_b184,
            completion_endpoint_load_text_offset: 0x0003_b198,
            completion_send_call_text_offset: 0x0003_b1a0,
        },
        G17Gfx1PowerEvidenceTarget::G17G => G17Gfx1CallbackEndpointEvidence {
            endpoint_request_store_text_offset: 0x0003_2be8,
            endpoint_allocator_call_text_offset: 0x0003_2bec,
            endpoint_allocator_sentinel_compare_text_offset: 0x0003_6a84,
            endpoint_allocator_dynamic_store_text_offset: 0x0003_6ad8,
            endpoint_result_load_text_offset: 0x0003_2c10,
            endpoint_handle_shift_text_offset: 0x0003_2cec,
            callback_endpoint_store_text_offset: 0x0003_2d08,
            callback_registration_load_text_offset: 0x0003_2d88,
            callback_registration_context_text_offset: 0x0003_df28,
            callback_handler_text_offset: 0x0003_87f8,
            callback_context_store_text_offset: 0x0003_8904,
            completion_context_load_text_offset: 0x0003_af88,
            completion_endpoint_load_text_offset: 0x0003_af9c,
            completion_send_call_text_offset: 0x0003_afa4,
        },
        G17Gfx1PowerEvidenceTarget::G17S => G17Gfx1CallbackEndpointEvidence {
            endpoint_request_store_text_offset: 0x0003_800c,
            endpoint_allocator_call_text_offset: 0x0003_8010,
            endpoint_allocator_sentinel_compare_text_offset: 0x0003_bea8,
            endpoint_allocator_dynamic_store_text_offset: 0x0003_befc,
            endpoint_result_load_text_offset: 0x0003_8034,
            endpoint_handle_shift_text_offset: 0x0003_8110,
            callback_endpoint_store_text_offset: 0x0003_812c,
            callback_registration_load_text_offset: 0x0003_81ac,
            callback_registration_context_text_offset: 0x0004_33c0,
            callback_handler_text_offset: 0x0003_dc1c,
            callback_context_store_text_offset: 0x0003_dd28,
            completion_context_load_text_offset: 0x0004_03ac,
            completion_endpoint_load_text_offset: 0x0004_03c0,
            completion_send_call_text_offset: 0x0004_03c8,
        },
    }
}

/// Validate the complete static value chain without registering an endpoint
/// or sending a management message.
pub(crate) const fn g17_gfx1_callback_endpoint_link_is_exact(
    _target: G17Gfx1PowerEvidenceTarget,
    requested_slot: u8,
    allocator_result_slot: u8,
    callback_endpoint_handle: u32,
    registration_endpoint_handle: u32,
    completion_endpoint_handle: u32,
) -> bool {
    requested_slot == G17_GFX1_CALLBACK_REQUESTED_ENDPOINT_SLOT
        && requested_slot != G17_GFX1_CALLBACK_DYNAMIC_ENDPOINT_SENTINEL
        && allocator_result_slot == requested_slot
        && callback_endpoint_handle
            == (allocator_result_slot as u32) << G17_GFX1_CALLBACK_ENDPOINT_HANDLE_SHIFT
        && registration_endpoint_handle == callback_endpoint_handle
        && completion_endpoint_handle == callback_endpoint_handle
}

pub(crate) const fn g17_host_management_route_evidence(
    target: G17Gfx1PowerEvidenceTarget,
) -> G17HostManagementRouteEvidence {
    match target {
        G17Gfx1PowerEvidenceTarget::G17P
        | G17Gfx1PowerEvidenceTarget::G17G
        | G17Gfx1PowerEvidenceTarget::G17S => G17HostManagementRouteEvidence {
            management_handler_text_offset: 0x0002_cda4,
            type0b_remote_state_branch_text_offset: 0x0002_ce90,
            wait_remote_state_text_offset: 0x0002_d908,
            handle_power_ack_text_offset: 0x0002_dea0,
            perform_power_change_text_offset: 0x0001_abf8,
            set_iop_status_text_offset: 0x0001_ca64,
        },
    }
}

/// Decode only the two incoming host power routes relevant to the GFX1
/// callback-completion linkage question.
pub(crate) const fn g17_host_management_message_route(
    _target: G17Gfx1PowerEvidenceTarget,
    message: u64,
) -> Result<G17HostManagementMessageRoute, G17HostManagementMessageError> {
    let message_type = ((message >> G17_HOST_MANAGEMENT_MESSAGE_TYPE_SHIFT)
        & G17_HOST_MANAGEMENT_MESSAGE_TYPE_MASK) as u8;

    match message_type {
        G17_HOST_POWER_ACK_MESSAGE_TYPE => Ok(G17HostManagementMessageRoute::IopStatusPowerAck),
        G17_HOST_GFX1_CALLBACK_MESSAGE_TYPE => {
            Ok(G17HostManagementMessageRoute::Gfx1CallbackRemoteState)
        }
        _ => Err(G17HostManagementMessageError::UnprovenMessageType),
    }
}

pub(crate) const fn g17_host_management_route_wake_field(
    route: G17HostManagementMessageRoute,
) -> (G17HostManagementWakeObject, u32) {
    match route {
        G17HostManagementMessageRoute::IopStatusPowerAck => (
            G17HostManagementWakeObject::RtBuddy,
            G17_HOST_RTBUDDY_IOP_STATUS_WORD_OFFSET,
        ),
        G17HostManagementMessageRoute::Gfx1CallbackRemoteState => (
            G17HostManagementWakeObject::ManagementEndpoint,
            G17_HOST_MANAGEMENT_REMOTE_STATE_WORD_OFFSET,
        ),
    }
}

// -------------------------------------------------------------------------
// GFX1_POWER_SEQUENCE — scheduler-role power-kick command acceptance.
//
// Grounded 2026-08-25 by disassembling `GFX.a010.macho` (`otool -tvV`). The
// remote-power-kick DECODE lives in the GFX/scheduler role, not GFX1: only
// `GFX.strings.txt` carries `agfaRemotePowerKick`,
// `AGFASchedulerProcessPowerKick`, `AGFAPowerMetricsServerTask`. The GFX1 role
// owns the transition grammar modeled above; this block adds the command
// acceptance the scheduler enforces before any power effect.
//
//   `agfaRemotePowerKick` decoder @ 0xfffffc00000237e4 — a pure exact-value
//   equality tree (no MMIO):
//     accept 0x0010_0000 @0xfffffc0000023840 (`cmp x0,#0x100,lsl#12`; OR-in
//            `ldsetal` @0xfffffc0000023854)
//     accept 0x0100_0000 @0xfffffc000002386c (`mov w8,#0x1000000; cmp; b.ne`)
//     accept 0x8000_0000 @0xfffffc000002381c-…3820 (`mov w8,#-0x80000000; cmp`)
//     abort  {0x40,0x80,0x100,0x200,0x2_0000_0000,0x20_0000_0000,
//            0x100_0000_0000} @0xfffffc0000023898-…38fc → `_RTK_abort`
//            "Unrecognized remote power kick value" @0xfffffc0000023970
//     else   `retab` silent no-op @0xfffffc0000023910
//   `AGFASchedulerProcessPowerKick` switch @ 0xfffffc0000026e34 — recognized
//     event bits {0x10_0000,0x400_0000,0x800_0000,0x1000_0000,0x2000_0000,
//     0x4000_0000}, default aborts "Unknown message received".
//   `AGFAPowerMetricsServerTask` @ 0xfffffc000002a800 — the pending-mask
//     highest set-bit index must be < 2 (`clz`/`eor #0x3f`; `cmp w8,#2;
//     b.hs` @0xfffffc000002a878), i.e. mask ⊆ {0x1,0x2}, else "Invalid Power
//     Server Kick" @0xfffffc000002a8ac.
//
// These are OFFLINE acceptance contracts: a driver can emit only accepted
// values and never a self-panicking bit, verified purely from these bytes.
// They do NOT make the sequence executable — the effects, the cross-ASC
// delivery, and the PMP doorbell physical bases (host-provided initdata
// `[x24+0x678]`, absent from the machos) are live-only. So
// `EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE` stays false.
// -------------------------------------------------------------------------

/// Firmware VA of the `agfaRemotePowerKick` exact-value decoder.
pub(crate) const G17_REMOTE_POWER_KICK_DECODER_VA: u64 = 0xffff_fc00_0002_37e4;
/// Firmware VA of the decoder's `_RTK_abort` default ("Unrecognized ...").
pub(crate) const G17_REMOTE_POWER_KICK_ABORT_VA: u64 = 0xffff_fc00_0002_3970;
/// The only three remote-power-kick bit values the decoder accepts.
pub(crate) const G17_REMOTE_POWER_KICK_ACCEPT: [u64; 3] =
    [0x0010_0000, 0x0100_0000, 0x8000_0000];
/// The seven bit values the decoder explicitly self-panics on.
pub(crate) const G17_REMOTE_POWER_KICK_FORBIDDEN: [u64; 7] = [
    0x40,
    0x80,
    0x100,
    0x200,
    0x2_0000_0000,
    0x20_0000_0000,
    0x100_0000_0000,
];
/// Recognized scheduler power-event bits (`AGFASchedulerProcessPowerKick`).
pub(crate) const G17_SCHEDULER_POWER_EVENT_ACCEPT: [u64; 6] = [
    0x0010_0000,
    0x0400_0000,
    0x0800_0000,
    0x1000_0000,
    0x2000_0000,
    0x4000_0000,
];
/// Allowed power-server pending-mask bits (highest set-bit index must be < 2).
pub(crate) const G17_POWER_SERVER_KICK_MASK_ALLOWED: u32 = 0b11;

/// The scheduler-role power-kick command acceptance contract is byte-grounded.
/// This is a necessary condition for a correct kick; it does not by itself
/// make the GFX1 power sequence executable (see the module-level note).
pub(crate) const GROUNDED_POWER_KICK_COMMAND_DECODE_AVAILABLE: bool = true;

/// Outcome of the `agfaRemotePowerKick` exact-value decoder for one value.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum RemotePowerKickClass {
    /// One of the three accepted bits: causes a real power effect.
    Accepted,
    /// One of the seven forbidden bits: the firmware calls `_RTK_abort`.
    ForbiddenPanic,
    /// Any other value: the decoder returns via `retab` with no effect.
    IgnoredNoOp,
}

/// Mirror the exact-value equality tree at 0xfffffc00000237e4.
pub(crate) const fn classify_remote_power_kick(value: u64) -> RemotePowerKickClass {
    let mut i = 0;
    while i < G17_REMOTE_POWER_KICK_ACCEPT.len() {
        if value == G17_REMOTE_POWER_KICK_ACCEPT[i] {
            return RemotePowerKickClass::Accepted;
        }
        i += 1;
    }
    let mut j = 0;
    while j < G17_REMOTE_POWER_KICK_FORBIDDEN.len() {
        if value == G17_REMOTE_POWER_KICK_FORBIDDEN[j] {
            return RemotePowerKickClass::ForbiddenPanic;
        }
        j += 1;
    }
    RemotePowerKickClass::IgnoredNoOp
}

/// Fail-closed reasons a remote power kick must not be emitted.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum PowerKickEmitError {
    /// The value self-panics the firmware (`_RTK_abort`).
    ForbiddenPanicBit,
    /// The value is silently ignored, so emitting it is a driver bug.
    UnrecognizedNoOp,
}

/// Fail-closed: only permit emitting a remote power kick the firmware accepts.
pub(crate) const fn validate_remote_power_kick_emit(
    value: u64,
) -> Result<(), PowerKickEmitError> {
    match classify_remote_power_kick(value) {
        RemotePowerKickClass::Accepted => Ok(()),
        RemotePowerKickClass::ForbiddenPanic => Err(PowerKickEmitError::ForbiddenPanicBit),
        RemotePowerKickClass::IgnoredNoOp => Err(PowerKickEmitError::UnrecognizedNoOp),
    }
}

/// Whether a scheduler power-event bit is one the firmware recognizes.
pub(crate) const fn scheduler_power_event_recognized(event: u64) -> bool {
    let mut i = 0;
    while i < G17_SCHEDULER_POWER_EVENT_ACCEPT.len() {
        if event == G17_SCHEDULER_POWER_EVENT_ACCEPT[i] {
            return true;
        }
        i += 1;
    }
    false
}

/// Whether a power-server pending mask stays within the firmware bound
/// (no set bit at index >= 2). A zero mask has nothing to dispatch.
pub(crate) const fn power_server_kick_mask_in_bounds(mask: u32) -> bool {
    mask & !G17_POWER_SERVER_KICK_MASK_ALLOWED == 0
}

/// Exact values observed in the shared G15-G17 host recovery word.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum RecoveryWordState {
    RunningOrPostRecoveryResumed = 0,
    FirmwareStoppedForPreRecovery = 1,
    HostResumedPreRecovery = 2,
    FirmwareStoppedForPostRecovery = 3,
}

/// G15/G16 host targets with an independently pinned `+0x5190` recovery word.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G15G16RecoveryLayoutEvidenceTarget {
    G15G,
    G15S,
    G15C,
    G15D,
    G16G,
    G16S,
    G16C,
}

pub(crate) const G15_G16_RECOVERY_LAYOUT_EVIDENCE_TARGETS:
    [G15G16RecoveryLayoutEvidenceTarget; 7] = [
        G15G16RecoveryLayoutEvidenceTarget::G15G,
        G15G16RecoveryLayoutEvidenceTarget::G15S,
        G15G16RecoveryLayoutEvidenceTarget::G15C,
        G15G16RecoveryLayoutEvidenceTarget::G15D,
        G15G16RecoveryLayoutEvidenceTarget::G16G,
        G15G16RecoveryLayoutEvidenceTarget::G16S,
        G15G16RecoveryLayoutEvidenceTarget::G16C,
    ];

/// Offset from the mapped firmware-state base, not from the host object.
pub(crate) const G15_AND_G16G_RECOVERY_WORD_OFFSET: u32 = 0x5190;

/// G16X uses the later mapped-state location for both proven target variants.
pub(crate) const G16X_RECOVERY_WORD_OFFSET: u32 = 0x4ae0;

pub(crate) const fn g15_g16_recovery_word_offset(
    target: G15G16RecoveryLayoutEvidenceTarget,
) -> u32 {
    match target {
        G15G16RecoveryLayoutEvidenceTarget::G15G
        | G15G16RecoveryLayoutEvidenceTarget::G15S
        | G15G16RecoveryLayoutEvidenceTarget::G15C
        | G15G16RecoveryLayoutEvidenceTarget::G15D
        | G15G16RecoveryLayoutEvidenceTarget::G16G => G15_AND_G16G_RECOVERY_WORD_OFFSET,
        G15G16RecoveryLayoutEvidenceTarget::G16S
        | G15G16RecoveryLayoutEvidenceTarget::G16C => G16X_RECOVERY_WORD_OFFSET,
    }
}

/// G17 host targets with an independently pinned `+0x4ae0` recovery word.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17RecoveryLayoutEvidenceTarget {
    G17P,
    G17G,
    G17S,
}

pub(crate) const G17_RECOVERY_LAYOUT_EVIDENCE_TARGETS: [G17RecoveryLayoutEvidenceTarget; 3] = [
    G17RecoveryLayoutEvidenceTarget::G17P,
    G17RecoveryLayoutEvidenceTarget::G17G,
    G17RecoveryLayoutEvidenceTarget::G17S,
];

/// Offset from the mapped firmware-state base, not from the host object.
pub(crate) const G17_RECOVERY_WORD_OFFSET: u32 = 0x4ae0;

pub(crate) const fn g17_recovery_word_offset(
    target: G17RecoveryLayoutEvidenceTarget,
) -> u32 {
    match target {
        G17RecoveryLayoutEvidenceTarget::G17P
        | G17RecoveryLayoutEvidenceTarget::G17G
        | G17RecoveryLayoutEvidenceTarget::G17S => G17_RECOVERY_WORD_OFFSET,
    }
}

/// Host-owned recovery phase whose write is directly established.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum HostResumePhase {
    PreRecovery,
    PostRecovery,
}

/// Fail-closed recovery-word validation errors.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryError {
    UnexpectedWordOffset,
    UnknownWordValue,
    FirmwareNotStoppedForPhase,
}

/// Decode only the four values checked by every pinned evidence target.
pub(crate) const fn decode_recovery_word(raw: u32) -> Result<RecoveryWordState, RecoveryError> {
    match raw {
        0 => Ok(RecoveryWordState::RunningOrPostRecoveryResumed),
        1 => Ok(RecoveryWordState::FirmwareStoppedForPreRecovery),
        2 => Ok(RecoveryWordState::HostResumedPreRecovery),
        3 => Ok(RecoveryWordState::FirmwareStoppedForPostRecovery),
        _ => Err(RecoveryError::UnknownWordValue),
    }
}

/// Decode a G15/G16 recovery word only at the exact target-backed location.
pub(crate) const fn decode_g15_g16_recovery_word_at_offset(
    target: G15G16RecoveryLayoutEvidenceTarget,
    observed_offset: u32,
    raw: u32,
) -> Result<RecoveryWordState, RecoveryError> {
    if observed_offset != g15_g16_recovery_word_offset(target) {
        return Err(RecoveryError::UnexpectedWordOffset);
    }

    decode_recovery_word(raw)
}

/// Decode a G17 recovery word only at the exact target-backed location.
pub(crate) const fn decode_g17_recovery_word_at_offset(
    target: G17RecoveryLayoutEvidenceTarget,
    observed_offset: u32,
    raw: u32,
) -> Result<RecoveryWordState, RecoveryError> {
    if observed_offset != g17_recovery_word_offset(target) {
        return Err(RecoveryError::UnexpectedWordOffset);
    }

    decode_recovery_word(raw)
}

/// Return the exact host value for one directly established resume edge.
///
/// This models only the two HOST-owned edges. The firmware-owned edges are
/// now byte-grounded separately (see [`firmware_recovery_word_write`]): the
/// firmware writes 1 at recovery start and 0 at recovery complete, and never
/// writes 2 or 3 — so the earlier `2 -> 3` inference is refuted. The caller
/// must observe the matching stopped value first.
pub(crate) const fn host_resume_recovery(
    phase: HostResumePhase,
    observed_raw: u32,
) -> Result<RecoveryWordState, RecoveryError> {
    let observed = match decode_recovery_word(observed_raw) {
        Ok(value) => value,
        Err(error) => return Err(error),
    };

    match (phase, observed) {
        (HostResumePhase::PreRecovery, RecoveryWordState::FirmwareStoppedForPreRecovery) => {
            Ok(RecoveryWordState::HostResumedPreRecovery)
        }
        (HostResumePhase::PostRecovery, RecoveryWordState::FirmwareStoppedForPostRecovery) => {
            Ok(RecoveryWordState::RunningOrPostRecoveryResumed)
        }
        _ => Err(RecoveryError::FirmwareNotStoppedForPhase),
    }
}

/// Apple's host reset hook is exactly `bti c; ret` in every evidence target.
pub(crate) const HOST_RESET_FOR_RECOVERY_IS_NOOP: bool = true;

/// Recovery inspection enters through role 0/GFX and drains both role logs.
/// These role facts are specific to the dual-role G17 implementation.
pub(crate) const G17_RECOVERY_INSPECTION_INGRESS_ROLE: u8 = 0;
pub(crate) const G17_RECOVERY_LOG_ROLE_MASK: u8 = 0b11;

/// Only these two values reach recognized recovery power-cycle paths in the
/// pinned G17S GFX1 image. Their private names and wire ordering are unknown.
pub(crate) const G17S_GFX1_RECOVERY_POWER_EVENTS: [u8; 2] = [0x22, 0x23];

pub(crate) const fn validate_g17s_gfx1_recovery_power_event(event: u8) -> bool {
    event == G17S_GFX1_RECOVERY_POWER_EVENTS[0] || event == G17S_GFX1_RECOVERY_POWER_EVENTS[1]
}

// -------------------------------------------------------------------------
// RECOVERY_HANDSHAKE — the GFX/scheduler recovery FSM firmware-write side.
//
// Grounded 2026-08-25 by disassembling `GFX.a010.macho` (`otool -tvV`). The
// recovery state machine is GFX-only (GFX1 carries only the
// `agfaHRBEnabledQueuesHalt` string, no recovery dispatch).
//
//   `AGFASchedulerProcessRecovery` outer decode @ 0xfffffc0000026274 — a cmp
//   ladder accepting exactly three recovery-cause bits, default aborts:
//     0x0200_0000 @0xfffffc0000026288 (`mov w8,#0x2000000; cmp; b.eq`)
//     0x0100_0000 @0xfffffc0000026294 (`mov w8,#0x1000000; cmp; b.eq`)
//     0x0080_0000 @0xfffffc00000262a0 (`cmp x0,#0x800,lsl#12; b.ne` default)
//     default    → `_RTK_abort` "Unexpected recovery type %llu!"
//                  @0xfffffc000002647c-…6498
//
//   Recovery-word firmware-write side — the host-shared status struct is
//   reached through the firmware BSS pointer global 0xfffffc00000_1047b8
//   (set from initdata); the recovery word is `[status+0xc]`. The firmware
//   writes it in exactly two places, both `dsb sy`-fenced, and never reads it:
//     write 1 at recovery START @0xfffffc0000026728
//            (`ldr x8,[x20,#0x7b8]; mov w9,#1; str w9,[x8,#0xc]; dsb sy`)
//     write 0 at recovery END   @0xfffffc0000026d58
//            (`ldr x8,[x20,#0x7b8]; str wzr,[x8,#0xc]; dsb sy`)
//   An exhaustive pointer-tracked store scan finds no firmware write of 2 or
//   3 to this word. This completes — and corrects — the note's "firmware
//   writes incomplete": the firmware side is {1 -> begin, 0 -> complete}; the
//   earlier guess that the firmware writes 2->3 is refuted. States 2 and 3
//   (HostResumedPreRecovery / FirmwareStoppedForPostRecovery) are written by
//   the HOST, and the firmware never observes them because it never reads the
//   word.
//
// OFFLINE-grounded: recovery-type acceptance and the two firmware-write edges.
// LIVE-only (keeps this fail-closed): (a) the host-visible DRAM behind the
// initdata pointer 0x1047b8 actually carrying the 1/0 to the host, and (b) the
// cross-role 0x22 (gfx_iop_msgs) / 0x23 (gfx_iop_ints) delivery to/from the
// GFX1 power role — the endpoint descriptors are chained-fixup DATA, not in
// `__text`, and delivery requires the peer ASC. So
// `EXECUTABLE_G17_RECOVERY_HANDSHAKE_AVAILABLE` stays false.
// -------------------------------------------------------------------------

/// Firmware VA of the `AGFASchedulerProcessRecovery` outer decoder.
pub(crate) const G17_RECOVERY_DISPATCH_VA: u64 = 0xffff_fc00_0002_6274;
/// Firmware VA of the "Unexpected recovery type" abort pad.
pub(crate) const G17_RECOVERY_TYPE_ABORT_VA: u64 = 0xffff_fc00_0002_647c;
/// The only three recovery-cause bits the outer decoder accepts.
pub(crate) const G17_RECOVERY_TYPE_ACCEPT: [u64; 3] =
    [0x0080_0000, 0x0100_0000, 0x0200_0000];

/// Firmware BSS pointer global holding the host-shared status struct.
pub(crate) const G17_FW_STATUS_STRUCT_PTR_GLOBAL_VA: u64 = 0xffff_fc00_0010_47b8;
/// Byte offset of the recovery word inside the host-shared status struct.
pub(crate) const G17_RECOVERY_WORD_STRUCT_OFFSET: u32 = 0xc;
/// Firmware store site that writes 1 at recovery start.
pub(crate) const G17_RECOVERY_WORD_FW_WRITE_BEGIN_VA: u64 = 0xffff_fc00_0002_6728;
/// Firmware store site that writes 0 at recovery complete.
pub(crate) const G17_RECOVERY_WORD_FW_WRITE_COMPLETE_VA: u64 = 0xffff_fc00_0002_6d58;
/// The only two values the firmware ever writes to the recovery word.
pub(crate) const G17_RECOVERY_WORD_FW_WRITTEN_VALUES: [u32; 2] = [1, 0];

/// Recovery-type acceptance and the recovery-word firmware-write side are
/// byte-grounded. This does not make the handshake executable: the cross-role
/// 0x22/0x23 delivery and the host-visible DRAM are live-only (see the note).
pub(crate) const GROUNDED_RECOVERY_WORD_FIRMWARE_WRITE_SIDE_AVAILABLE: bool = true;

/// The two firmware-driven recovery-word edges, grounded from the two stores.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum FirmwareRecoveryEdge {
    /// Recovery start: firmware writes 1 (`FirmwareStoppedForPreRecovery`).
    Begin,
    /// Recovery complete: firmware writes 0 (`RunningOrPostRecoveryResumed`).
    Complete,
}

/// The state the firmware writes to the recovery word for a given edge.
pub(crate) const fn firmware_recovery_word_write(
    edge: FirmwareRecoveryEdge,
) -> RecoveryWordState {
    match edge {
        FirmwareRecoveryEdge::Begin => RecoveryWordState::FirmwareStoppedForPreRecovery,
        FirmwareRecoveryEdge::Complete => RecoveryWordState::RunningOrPostRecoveryResumed,
    }
}

/// Whether the firmware ever writes a given raw value to the recovery word.
/// Grounded to exactly {0, 1}; a model claiming the firmware writes 2 or 3 is
/// refuted by the exhaustive store scan.
pub(crate) const fn firmware_writes_recovery_value(raw: u32) -> bool {
    raw == 0 || raw == 1
}

/// Fail-closed recovery-type validation error.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryTypeError {
    UnexpectedRecoveryType,
}

/// Fail-closed: only the three accepted recovery-cause bits avoid the
/// firmware's "Unexpected recovery type" abort.
pub(crate) const fn validate_recovery_type(raw: u64) -> Result<(), RecoveryTypeError> {
    let mut i = 0;
    while i < G17_RECOVERY_TYPE_ACCEPT.len() {
        if raw == G17_RECOVERY_TYPE_ACCEPT[i] {
            return Ok(());
        }
        i += 1;
    }
    Err(RecoveryTypeError::UnexpectedRecoveryType)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The J700 GPU node observation exactly as its Linux device tree makes
    /// it (CPU physical addresses, per-mailbox named IRQs, role strings).
    fn j700_linux_dt_topology() -> T8140G17PLinuxDtTopology {
        T8140G17PLinuxDtTopology {
            gfx: T8140G17PLinuxDtAscObservation {
                role_is_gfx1: false,
                wrapper_cpu_base: 0x4_8260_0000,
                wrapper_size: 0x0008_8000,
                iorvbar_cpu_base: 0x4_8205_0000,
                iorvbar_size: 8,
                send_empty_irq: 993,
                recv_not_empty_irq: 996,
            },
            gfx1: T8140G17PLinuxDtAscObservation {
                role_is_gfx1: true,
                wrapper_cpu_base: 0x4_82e0_0000,
                wrapper_size: 0x0008_8000,
                iorvbar_cpu_base: 0x4_8285_0000,
                iorvbar_size: 8,
                send_empty_irq: 997,
                recv_not_empty_irq: 1000,
            },
        }
    }

    #[test]
    fn linux_dt_observation_translates_to_the_pinned_asc_providers() {
        // The J700 Linux DT records the same providers as the pinned D93AP
        // ADT constants, shifted by the arm-io CPU translation.
        assert_eq!(
            decode_t8140_g17p_asc_providers_from_linux_dt(&j700_linux_dt_topology()),
            Some(D93AP_T8140_G17P_ASC_PROVIDERS)
        );
    }

    #[test]
    fn linux_dt_observation_fails_closed_on_role_or_address_mismatch() {
        // Swapped role labels never decode.
        let mut swapped = j700_linux_dt_topology();
        swapped.gfx.role_is_gfx1 = true;
        assert_eq!(
            decode_t8140_g17p_asc_providers_from_linux_dt(&swapped),
            None
        );

        // An address below the bus translation window cannot be translated.
        let mut untranslatable = j700_linux_dt_topology();
        untranslatable.gfx1.wrapper_cpu_base = 0x1000;
        assert_eq!(
            decode_t8140_g17p_asc_providers_from_linux_dt(&untranslatable),
            None
        );

        // A perturbed-but-decodable observation decodes to providers that do
        // NOT match the pinned expectation: admission (the caller's
        // comparison) is what rejects it.
        let mut perturbed = j700_linux_dt_topology();
        perturbed.gfx.wrapper_cpu_base += 0x4000;
        let decoded = decode_t8140_g17p_asc_providers_from_linux_dt(&perturbed).unwrap();
        assert_ne!(decoded, D93AP_T8140_G17P_ASC_PROVIDERS);
    }

    #[test]
    fn sleep_and_nap_require_the_numeric_0x21_bridge() {
        for edge in [
            (GFX1_POWER_SLEEP, GFX1_POWER_INTERMEDIATE_21),
            (GFX1_POWER_NAP, GFX1_POWER_INTERMEDIATE_21),
            (GFX1_POWER_INTERMEDIATE_21, GFX1_POWER_SLEEP),
            (GFX1_POWER_INTERMEDIATE_21, GFX1_POWER_NAP),
        ] {
            assert_eq!(validate_g17s_gfx1_power_transition(edge.0, edge.1), Ok(()));
        }

        for edge in [
            (GFX1_POWER_SLEEP, GFX1_POWER_NAP),
            (GFX1_POWER_NAP, GFX1_POWER_SLEEP),
        ] {
            assert_eq!(
                validate_g17s_gfx1_power_transition(edge.0, edge.1),
                Err(PowerTransitionError::DirectSleepNapIsFatal)
            );
        }
    }

    #[test]
    fn unproved_power_edges_fail_closed() {
        for edge in [
            (GFX1_POWER_SLEEP, GFX1_POWER_SLEEP),
            (GFX1_POWER_NAP, GFX1_POWER_NAP),
            (GFX1_POWER_INTERMEDIATE_21, GFX1_POWER_INTERMEDIATE_21),
            (0, GFX1_POWER_INTERMEDIATE_21),
            (GFX1_POWER_INTERMEDIATE_21, 0xff),
        ] {
            assert_eq!(
                validate_g17s_gfx1_power_transition(edge.0, edge.1),
                Err(PowerTransitionError::UnprovenTransition)
            );
        }
    }

    #[test]
    fn g17_callback_ack_prefix_is_exact_and_target_pinned() {
        assert_eq!(
            g17_gfx1_callback_ack_evidence(G17Gfx1PowerEvidenceTarget::G17P),
            G17Gfx1CallbackAckEvidence {
                worker_text_offset: 0x0003_b030,
                release_helper_text_offset: 0x0003_5484,
                payload_mapper_text_offset: 0x0003_8dc8,
                sender_text_offset: 0x0003_dc00,
            }
        );
        assert_eq!(
            g17_gfx1_callback_ack_evidence(G17Gfx1PowerEvidenceTarget::G17G),
            G17Gfx1CallbackAckEvidence {
                worker_text_offset: 0x0003_ae34,
                release_helper_text_offset: 0x0003_52e8,
                payload_mapper_text_offset: 0x0003_8bdc,
                sender_text_offset: 0x0003_da48,
            }
        );
        assert_eq!(
            g17_gfx1_callback_ack_evidence(G17Gfx1PowerEvidenceTarget::G17S),
            G17Gfx1CallbackAckEvidence {
                worker_text_offset: 0x0004_0258,
                release_helper_text_offset: 0x0003_a70c,
                payload_mapper_text_offset: 0x0003_e000,
                sender_text_offset: 0x0004_2f00,
            }
        );

        for target in G17_GFX1_POWER_EVIDENCE_TARGETS {
            assert!(validate_g17_gfx1_callback_ack_prefix(
                target,
                &G17_GFX1_CALLBACK_ACK_PREFIX
            ));
        }

        let mut wrong_order = G17_GFX1_CALLBACK_ACK_PREFIX;
        wrong_order.swap(2, 3);
        assert!(!validate_g17_gfx1_callback_ack_prefix(
            G17Gfx1PowerEvidenceTarget::G17S,
            &wrong_order
        ));
        assert!(!validate_g17_gfx1_callback_ack_prefix(
            G17Gfx1PowerEvidenceTarget::G17P,
            &G17_GFX1_CALLBACK_ACK_PREFIX[..5]
        ));

        let mut wrong_sender = G17_GFX1_CALLBACK_ACK_PREFIX;
        wrong_sender[5] = G17Gfx1CallbackAckStep::MapCompletionPayload;
        assert!(!validate_g17_gfx1_callback_ack_prefix(
            G17Gfx1PowerEvidenceTarget::G17G,
            &wrong_sender
        ));
        assert_eq!(
            G17_GFX1_CALLBACK_ACK_MESSAGE_TYPE_BITS,
            0x00b0_0000_0000_0000
        );
        assert_eq!(G17_GFX1_CALLBACK_ACK_RETRY_STATUS, 0x8000_000e);
        assert!(!EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE);
    }

    #[test]
    fn g17_callback_ack_payload_mapper_fails_closed() {
        let exact_cases = [
            (4, 3),
            (5, 0x203),
            (6, 0),
            (7, 0),
            (GFX1_POWER_SLEEP, 0x201),
            (GFX1_POWER_NAP, 0),
            (0x20, 0x10),
            (GFX1_POWER_INTERMEDIATE_21, 0x20),
        ];

        for target in G17_GFX1_POWER_EVIDENCE_TARGETS {
            for (internal_state, payload) in exact_cases {
                assert_eq!(
                    g17_gfx1_callback_ack_payload(target, internal_state),
                    Ok(payload)
                );
            }
            assert_eq!(
                g17_gfx1_callback_ack_payload(target, 3),
                Err(G17Gfx1CallbackAckError::FatalInternalState)
            );
            for internal_state in [0, 1, 2, 9, 0x22, u8::MAX] {
                assert_eq!(
                    g17_gfx1_callback_ack_payload(target, internal_state),
                    Err(G17Gfx1CallbackAckError::UnprovenInternalState)
                );
            }
        }

        assert!(!EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE);
        assert!(!EXECUTABLE_G17_RECOVERY_HANDSHAKE_AVAILABLE);
    }

    #[test]
    fn g17_host_management_power_routes_are_target_pinned() {
        let exact = G17HostManagementRouteEvidence {
            management_handler_text_offset: 0x0002_cda4,
            type0b_remote_state_branch_text_offset: 0x0002_ce90,
            wait_remote_state_text_offset: 0x0002_d908,
            handle_power_ack_text_offset: 0x0002_dea0,
            perform_power_change_text_offset: 0x0001_abf8,
            set_iop_status_text_offset: 0x0001_ca64,
        };

        for target in G17_GFX1_POWER_EVIDENCE_TARGETS {
            assert_eq!(g17_host_management_route_evidence(target), exact);
        }

        assert_eq!(G17_HOST_MANAGEMENT_MESSAGE_TYPE_SHIFT, 52);
        assert_eq!(G17_HOST_MANAGEMENT_MESSAGE_TYPE_MASK, 0xf);
        assert_eq!(G17_HOST_POWER_ACK_MESSAGE_TYPE, 7);
        assert_eq!(G17_HOST_GFX1_CALLBACK_MESSAGE_TYPE, 0xb);
        assert_eq!(G17_HOST_MANAGEMENT_REMOTE_STATE_WORD_OFFSET, 0x108);
        assert_eq!(G17_HOST_RTBUDDY_IOP_STATUS_WORD_OFFSET, 0x158);
        assert!(G17_GFX1_CALLBACK_MANAGEMENT_ENDPOINT_LINK_PROVEN);
        assert!(!EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE);
        assert!(!EXECUTABLE_G17_RECOVERY_HANDSHAKE_AVAILABLE);
    }

    #[test]
    fn g17_gfx1_callback_endpoint_zero_dataflow_is_target_pinned() {
        let evidence = [
            (
                G17Gfx1PowerEvidenceTarget::G17P,
                G17Gfx1CallbackEndpointEvidence {
                    endpoint_request_store_text_offset: 0x0003_2d7c,
                    endpoint_allocator_call_text_offset: 0x0003_2d80,
                    endpoint_allocator_sentinel_compare_text_offset: 0x0003_6c70,
                    endpoint_allocator_dynamic_store_text_offset: 0x0003_6cc4,
                    endpoint_result_load_text_offset: 0x0003_2da4,
                    endpoint_handle_shift_text_offset: 0x0003_2e84,
                    callback_endpoint_store_text_offset: 0x0003_2ea0,
                    callback_registration_load_text_offset: 0x0003_2f20,
                    callback_registration_context_text_offset: 0x0003_e088,
                    callback_handler_text_offset: 0x0003_89e4,
                    callback_context_store_text_offset: 0x0003_8af0,
                    completion_context_load_text_offset: 0x0003_b184,
                    completion_endpoint_load_text_offset: 0x0003_b198,
                    completion_send_call_text_offset: 0x0003_b1a0,
                },
            ),
            (
                G17Gfx1PowerEvidenceTarget::G17G,
                G17Gfx1CallbackEndpointEvidence {
                    endpoint_request_store_text_offset: 0x0003_2be8,
                    endpoint_allocator_call_text_offset: 0x0003_2bec,
                    endpoint_allocator_sentinel_compare_text_offset: 0x0003_6a84,
                    endpoint_allocator_dynamic_store_text_offset: 0x0003_6ad8,
                    endpoint_result_load_text_offset: 0x0003_2c10,
                    endpoint_handle_shift_text_offset: 0x0003_2cec,
                    callback_endpoint_store_text_offset: 0x0003_2d08,
                    callback_registration_load_text_offset: 0x0003_2d88,
                    callback_registration_context_text_offset: 0x0003_df28,
                    callback_handler_text_offset: 0x0003_87f8,
                    callback_context_store_text_offset: 0x0003_8904,
                    completion_context_load_text_offset: 0x0003_af88,
                    completion_endpoint_load_text_offset: 0x0003_af9c,
                    completion_send_call_text_offset: 0x0003_afa4,
                },
            ),
            (
                G17Gfx1PowerEvidenceTarget::G17S,
                G17Gfx1CallbackEndpointEvidence {
                    endpoint_request_store_text_offset: 0x0003_800c,
                    endpoint_allocator_call_text_offset: 0x0003_8010,
                    endpoint_allocator_sentinel_compare_text_offset: 0x0003_bea8,
                    endpoint_allocator_dynamic_store_text_offset: 0x0003_befc,
                    endpoint_result_load_text_offset: 0x0003_8034,
                    endpoint_handle_shift_text_offset: 0x0003_8110,
                    callback_endpoint_store_text_offset: 0x0003_812c,
                    callback_registration_load_text_offset: 0x0003_81ac,
                    callback_registration_context_text_offset: 0x0004_33c0,
                    callback_handler_text_offset: 0x0003_dc1c,
                    callback_context_store_text_offset: 0x0003_dd28,
                    completion_context_load_text_offset: 0x0004_03ac,
                    completion_endpoint_load_text_offset: 0x0004_03c0,
                    completion_send_call_text_offset: 0x0004_03c8,
                },
            ),
        ];

        for (target, exact) in evidence {
            assert_eq!(g17_gfx1_callback_endpoint_evidence(target), exact);
            assert!(g17_gfx1_callback_endpoint_link_is_exact(
                target, 0, 0, 0, 0, 0
            ));
            assert!(!g17_gfx1_callback_endpoint_link_is_exact(
                target, 0xff, 0, 0, 0, 0
            ));
            assert!(!g17_gfx1_callback_endpoint_link_is_exact(
                target, 0, 1, 1 << 16, 1 << 16, 1 << 16
            ));
            assert!(!g17_gfx1_callback_endpoint_link_is_exact(
                target, 0, 0, 1, 0, 0
            ));
            assert!(!g17_gfx1_callback_endpoint_link_is_exact(
                target, 0, 0, 0, 1, 0
            ));
            assert!(!g17_gfx1_callback_endpoint_link_is_exact(
                target, 0, 0, 0, 0, 1
            ));
        }

        assert_eq!(G17_GFX1_CALLBACK_REQUESTED_ENDPOINT_SLOT, 0);
        assert_eq!(G17_GFX1_CALLBACK_DYNAMIC_ENDPOINT_SENTINEL, 0xff);
        assert_eq!(G17_GFX1_CALLBACK_ENDPOINT_HANDLE_SHIFT, 16);
        assert_eq!(G17_GFX1_CALLBACK_ENDPOINT_FIELD_OFFSET, 8);
        assert_eq!(G17_GFX1_CALLBACK_CONTEXT_FIELD_OFFSET, 0x40);
        assert!(G17_GFX1_CALLBACK_MANAGEMENT_ENDPOINT_LINK_PROVEN);
        assert!(!EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE);
        assert!(!EXECUTABLE_G17_RECOVERY_HANDSHAKE_AVAILABLE);
    }

    #[test]
    fn g17_host_management_type0b_is_not_the_iop_status_power_ack() {
        let power_ack = 0x0070_0000_0000_0001;

        for target in G17_GFX1_POWER_EVIDENCE_TARGETS {
            let power_route = g17_host_management_message_route(target, power_ack).unwrap();
            assert_eq!(
                power_route,
                G17HostManagementMessageRoute::IopStatusPowerAck
            );
            assert_eq!(
                g17_host_management_route_wake_field(power_route),
                (
                    G17HostManagementWakeObject::RtBuddy,
                    G17_HOST_RTBUDDY_IOP_STATUS_WORD_OFFSET,
                )
            );

            for internal_state in [4, 5, 6, 7, 8, 0x10, 0x20, 0x21] {
                let payload = g17_gfx1_callback_ack_payload(target, internal_state).unwrap();
                let completion = G17_GFX1_CALLBACK_ACK_MESSAGE_TYPE_BITS | u64::from(payload);
                let completion_route =
                    g17_host_management_message_route(target, completion).unwrap();

                assert_eq!(
                    completion_route,
                    G17HostManagementMessageRoute::Gfx1CallbackRemoteState
                );
                assert_eq!(
                    g17_host_management_route_wake_field(completion_route),
                    (
                        G17HostManagementWakeObject::ManagementEndpoint,
                        G17_HOST_MANAGEMENT_REMOTE_STATE_WORD_OFFSET,
                    )
                );
                assert_eq!(completion as u32, u32::from(payload));
                assert_ne!(completion_route, power_route);
            }

            for message_type in [0, 1, 4, 6, 8, 0xc, 0xf] {
                assert_eq!(
                    g17_host_management_message_route(target, message_type << 52),
                    Err(G17HostManagementMessageError::UnprovenMessageType)
                );
            }
        }

        assert_ne!(
            G17HostManagementWakeObject::ManagementEndpoint,
            G17HostManagementWakeObject::RtBuddy
        );
        assert_ne!(
            G17_HOST_MANAGEMENT_REMOTE_STATE_WORD_OFFSET,
            G17_HOST_RTBUDDY_IOP_STATUS_WORD_OFFSET
        );
        assert!(G17_GFX1_CALLBACK_MANAGEMENT_ENDPOINT_LINK_PROVEN);
        assert!(!EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE);
        assert!(!EXECUTABLE_G17_RECOVERY_HANDSHAKE_AVAILABLE);
    }

    #[test]
    fn g17_pre_idle_helper_is_shared_but_direct_pio_prefix_is_not() {
        assert_eq!(
            G17_GFX1_POWER_EVIDENCE_TARGETS,
            [
                G17Gfx1PowerEvidenceTarget::G17P,
                G17Gfx1PowerEvidenceTarget::G17G,
                G17Gfx1PowerEvidenceTarget::G17S,
            ]
        );
        assert_eq!(
            g17_gfx1_pre_idle_entry_evidence(G17Gfx1PowerEvidenceTarget::G17P),
            G17Gfx1PreIdleEntryEvidence {
                helper_text_offset: 0x0003_5f5c,
                helper_object_vm_offset: 0x000f_90f0,
                direct_pio_prefix_before_helper: false,
            }
        );
        assert_eq!(
            g17_gfx1_pre_idle_entry_evidence(G17Gfx1PowerEvidenceTarget::G17G),
            G17Gfx1PreIdleEntryEvidence {
                helper_text_offset: 0x0003_5db4,
                helper_object_vm_offset: 0x000f_3c30,
                direct_pio_prefix_before_helper: false,
            }
        );
        assert_eq!(
            g17_gfx1_pre_idle_entry_evidence(G17Gfx1PowerEvidenceTarget::G17S),
            G17Gfx1PreIdleEntryEvidence {
                helper_text_offset: 0x0003_b1d8,
                helper_object_vm_offset: 0x0010_0df0,
                direct_pio_prefix_before_helper: true,
            }
        );
        assert_eq!(G17_GFX1_PRE_IDLE_HELPER_WINDOW_SIZE, 0xd4);
        assert_eq!(G17_GFX1_PRE_IDLE_HELPER_INSTRUCTION_COUNT, 53);
        assert_eq!(G17_GFX1_PRE_IDLE_HELPER_CAS_COMPARE, 1);
        assert_eq!(G17_GFX1_PRE_IDLE_HELPER_CAS_REPLACEMENT, 0);
        assert!(!G17_GFX1_PRE_IDLE_HELPER_CONTAINS_DIRECT_PIO_ACCESS);

        let valid_edges = [
            (GFX1_POWER_SLEEP, GFX1_POWER_INTERMEDIATE_21),
            (GFX1_POWER_NAP, GFX1_POWER_INTERMEDIATE_21),
            (GFX1_POWER_INTERMEDIATE_21, GFX1_POWER_SLEEP),
            (GFX1_POWER_INTERMEDIATE_21, GFX1_POWER_NAP),
        ];
        for target in G17_GFX1_POWER_EVIDENCE_TARGETS {
            for edge in valid_edges {
                assert_eq!(
                    validate_g17_gfx1_power_transition(target, edge.0, edge.1),
                    Ok(())
                );
            }
            assert_eq!(
                validate_g17_gfx1_power_transition(
                    target,
                    GFX1_POWER_SLEEP,
                    GFX1_POWER_NAP,
                ),
                Err(PowerTransitionError::DirectSleepNapIsFatal)
            );
            assert_eq!(
                validate_g17_gfx1_power_transition(
                    target,
                    GFX1_POWER_INTERMEDIATE_21,
                    GFX1_POWER_INTERMEDIATE_21,
                ),
                Err(PowerTransitionError::UnprovenTransition)
            );
            assert!(!g17_gfx1_idle_poll_satisfied(target, 0));
            assert!(g17_gfx1_idle_poll_satisfied(target, 1));
            assert!(!g17_gfx1_idle_poll_satisfied(target, 2));
            assert!(g17_gfx1_idle_poll_satisfied(target, u32::MAX));
        }

        assert_eq!(G17_GFX1_IDLE_POLL_MASK, 1);
        assert_eq!(G17_GFX1_IDLE_POLL_EXPECTED, 1);
        assert_eq!(G17_GFX1_IDLE_POLL_TIMEOUT_TICKS, 0x1770);
        assert_eq!(G17_GFX1_IDLE_POLL_REPORTED_MAX_US, 250);
        assert_eq!(
            g17_gfx1_report_timebase_delta_us(G17_GFX1_IDLE_POLL_TIMEOUT_TICKS),
            G17_GFX1_IDLE_POLL_REPORTED_MAX_US
        );
        assert!(!EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE);
    }

    #[test]
    fn g17s_power_pio_prefixes_require_exact_direction_order_and_kind() {
        assert!(validate_g17s_gfx1_21_to_sleep_pio_prefix(
            &G17S_GFX1_21_TO_SLEEP_PIO_PREFIX
        ));
        assert!(validate_g17s_gfx1_low_power_to_21_pio_prefix(
            &G17S_GFX1_LOW_POWER_TO_21_PIO_PREFIX
        ));
        assert_eq!(
            G17S_GFX1_SLEEP_TO_21_PIO_PREFIX,
            G17S_GFX1_21_TO_SLEEP_PIO_PREFIX
        );
        assert_eq!(
            G17S_GFX1_21_TO_LOW_POWER_PIO_PREFIX,
            G17S_GFX1_LOW_POWER_TO_21_PIO_PREFIX
        );
        assert!(validate_g17s_gfx1_sleep_to_21_pio_prefix(
            &G17S_GFX1_21_TO_SLEEP_PIO_PREFIX
        ));
        assert!(validate_g17s_gfx1_21_to_low_power_pio_prefix(
            &G17S_GFX1_LOW_POWER_TO_21_PIO_PREFIX
        ));

        let mut wrong_order = G17S_GFX1_21_TO_SLEEP_PIO_PREFIX;
        wrong_order.swap(0, 1);
        assert!(!validate_g17s_gfx1_21_to_sleep_pio_prefix(&wrong_order));

        let mut wrong_value = G17S_GFX1_21_TO_SLEEP_PIO_PREFIX;
        wrong_value[1] = G17PioOp::Write {
            slot: G17PioSlot::PushTelemetryDashboardConfigSpace,
            offset: 0x354,
            value: 0x0020_0001,
        };
        assert!(!validate_g17s_gfx1_21_to_sleep_pio_prefix(&wrong_value));

        let mut wrong_kind = G17S_GFX1_LOW_POWER_TO_21_PIO_PREFIX;
        wrong_kind[1] = G17PioOp::Write {
            slot: G17PioSlot::PushTelemetryDashboardConfigSpace,
            offset: 0x758,
            value: 0x0000_0b00,
        };
        assert!(!validate_g17s_gfx1_low_power_to_21_pio_prefix(&wrong_kind));
        assert!(!validate_g17s_gfx1_21_to_sleep_pio_prefix(&[]));
    }

    #[test]
    fn g17s_pio_type_names_and_descriptor_indices_are_exact() {
        assert_eq!(G17PioSlot::GpcAicSwInt as u8, 0x00);
        assert_eq!(
            G17PioSlot::PushTelemetryDashboardConfigSpace as u8,
            0x0b
        );
        assert_eq!(G17PioSlot::FenderRegsMcw as u8, 0x24);
        assert_eq!(G17S_PIO_DESCRIPTOR_SIZE, 0x28);
        assert_eq!(G17S_PIO_DESCRIPTOR_POINTER_OFFSET, 8);
        assert_eq!(
            g17s_pio_mapped_pointer_offset(G17PioSlot::GpcAicSwInt),
            0x008
        );
        assert_eq!(
            g17s_pio_mapped_pointer_offset(
                G17PioSlot::PushTelemetryDashboardConfigSpace
            ),
            0x1c0
        );
        assert_eq!(
            g17s_pio_mapped_pointer_offset(G17PioSlot::FenderRegsMcw),
            0x5a8
        );
    }

    #[test]
    fn g17s_pre_idle_firmware_ops_require_exact_order_and_width() {
        assert!(validate_g17s_gfx1_pre_idle_firmware_ops(
            &G17S_GFX1_PRE_IDLE_FIRMWARE_OPS
        ));

        let mut wrong_order = G17S_GFX1_PRE_IDLE_FIRMWARE_OPS;
        wrong_order.swap(0, 1);
        assert!(!validate_g17s_gfx1_pre_idle_firmware_ops(&wrong_order));

        let mut wrong_width = G17S_GFX1_PRE_IDLE_FIRMWARE_OPS;
        wrong_width[3] = G17SFirmwareOp::AtomicAdd32 {
            global_offset: 0x0000_11a0,
            addend: 0x0800_0000,
        };
        assert!(!validate_g17s_gfx1_pre_idle_firmware_ops(&wrong_width));

        let mut wrong_helper_object = G17S_GFX1_PRE_IDLE_FIRMWARE_OPS;
        wrong_helper_object[5] = G17SFirmwareOp::OpaqueHelperCall {
            helper_text_offset: 0x0003_b1d8,
            object_global_offset: 0x0000_0df0,
        };
        assert!(!validate_g17s_gfx1_pre_idle_firmware_ops(
            &wrong_helper_object
        ));
        assert!(!validate_g17s_gfx1_pre_idle_firmware_ops(&[]));
    }

    #[test]
    fn g17s_idle_poll_has_exact_map_predicate_and_reported_deadline() {
        assert_eq!(G17PioSlot::FenderRegsMcw as u8, 0x24);
        assert_eq!(G17_PIO_SLOT_24_RELATIVE_OFFSET, 0x00d4_0000);
        assert_eq!(G17_PIO_SLOT_24_SIZE, 0x4000);
        assert_eq!(G17S_GFX1_IDLE_STATUS_PHYS, 0x21_00d4_0000);
        assert_eq!(G17S_GFX1_IDLE_POLL_MASK, 1);
        assert_eq!(G17S_GFX1_IDLE_POLL_EXPECTED, 1);
        assert!(!g17s_gfx1_idle_poll_satisfied(0));
        assert!(g17s_gfx1_idle_poll_satisfied(1));
        assert!(!g17s_gfx1_idle_poll_satisfied(2));
        assert!(g17s_gfx1_idle_poll_satisfied(u32::MAX));
        assert_eq!(
            g17s_gfx1_report_timebase_delta_us(G17S_GFX1_IDLE_POLL_TIMEOUT_TICKS),
            G17S_GFX1_IDLE_POLL_REPORTED_MAX_US
        );
        assert_eq!(
            G17_PIO_SLOT_24_MAP_EVIDENCE_TARGETS,
            [
                G17PioMapEvidenceTarget::G17P,
                G17PioMapEvidenceTarget::G17G,
                G17PioMapEvidenceTarget::G17S,
            ]
        );
    }

    #[test]
    fn g17g_and_g17p_physical_pio_bindings_are_target_specific() {
        let g17p = g17_host_pio_physical_evidence(G17HostPioPhysicalEvidenceTarget::G17P);
        let g17g = g17_host_pio_physical_evidence(G17HostPioPhysicalEvidenceTarget::G17G);

        assert_eq!(g17p.sgx_phys_base, 0x2_7000_0000);
        assert_eq!(g17g.sgx_phys_base, 0x3_7000_0000);
        assert_eq!(g17p.host_io_page_size, 0x4000);
        assert_eq!(g17g.host_io_page_size, 0x4000);
        assert_eq!(
            g17p.meta_sw_interrupt,
            G17MetaSwInterruptEvidence {
                first_u64: 0x3_0101_4048,
                second_u64: 0x3_0101_4248,
                trailing_u32: 0x4000,
            }
        );
        assert_eq!(
            g17g.meta_sw_interrupt,
            G17MetaSwInterruptEvidence {
                first_u64: 0x3_8101_4048,
                second_u64: 0x3_8101_4248,
                trailing_u32: 0x4000,
            }
        );
        assert_eq!(
            G17_HOST_PIO_PHYSICAL_EVIDENCE_TARGETS,
            [
                G17HostPioPhysicalEvidenceTarget::G17P,
                G17HostPioPhysicalEvidenceTarget::G17G,
                G17HostPioPhysicalEvidenceTarget::G17S,
            ]
        );
        assert!(!G17_HOST_PIO_LINUX_MMIO_OWNERSHIP_PROVEN);
        assert!(!EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE);
    }

    #[test]
    fn t8140_g17p_adt_topology_is_exact_and_fail_closed() {
        let exact = D93AP_T8140_G17P_ADT_TOPOLOGY;

        assert!(validate_t8140_g17p_adt_topology(&exact));
        let providers = decode_t8140_g17p_asc_providers(&exact).unwrap();
        assert_eq!(providers, D93AP_T8140_G17P_ASC_PROVIDERS);
        assert_eq!(providers[0].role, T8140G17PAscRole::Gfx);
        assert_eq!(providers[0].send_empty_irq, 993);
        assert_eq!(providers[0].recv_not_empty_irq, 996);
        assert_eq!(providers[1].role, T8140G17PAscRole::Gfx1);
        assert_eq!(providers[1].send_empty_irq, 997);
        assert_eq!(providers[1].recv_not_empty_irq, 1000);
        assert_eq!(providers[0].wrapper_regs.size, 0x88000);
        assert_eq!(providers[1].wrapper_regs.size, 0x88000);
        assert_eq!(providers[0].iorvbar_regs.size, 8);
        assert_eq!(providers[1].iorvbar_regs.size, 8);
        assert_eq!(
            providers[0].wrapper_regs.phys_base + u64::from(T8140_ASCWRAP_V6_MAILBOX_OFFSET),
            0x2_7260_8000
        );
        assert_eq!(
            providers[1].wrapper_regs.phys_base + u64::from(T8140_ASCWRAP_V6_MAILBOX_OFFSET),
            0x2_72e0_8000
        );
        assert_eq!(exact.sgx_power_gates, [235, 236]);
        assert_eq!(exact.sgx_power_gate_parents, [Some(40), None]);
        assert_eq!(exact.gfx_power_domain, 40);
        assert_eq!(exact.gfx_power_domain_parents, [29, 31]);
        assert_eq!(exact.gfx_asc_power_gate, 237);
        assert_eq!(exact.gfx_asc_power_gate_parent, 40);
        assert_eq!(exact.gfx1_asc_power_gate, 321);
        assert_eq!(exact.gfx1_asc_power_gate_parent, 40);
        assert_eq!(
            D93AP_T8140_G17P_PMGR_GATES,
            [
                T8140G17PPmgrGate {
                    device_id: 235,
                    name: T8140G17PPmgrDeviceName::GfxSgx,
                    flags: 16,
                    index: 0,
                    alias: Some(40),
                },
                T8140G17PPmgrGate {
                    device_id: 236,
                    name: T8140G17PPmgrDeviceName::GfxBusy,
                    flags: 16,
                    index: 0,
                    alias: None,
                },
                T8140G17PPmgrGate {
                    device_id: 237,
                    name: T8140G17PPmgrDeviceName::GfxAsc,
                    flags: 16,
                    index: 0,
                    alias: Some(40),
                },
                T8140G17PPmgrGate {
                    device_id: 321,
                    name: T8140G17PPmgrDeviceName::GfxAsc1,
                    flags: 16,
                    index: 0,
                    alias: Some(40),
                },
            ]
        );
        assert!(T8140_G17P_PMGR_GATE_TOPOLOGY_PROVEN);
        assert!(!T8140_G17P_LIVE_POWER_SEQUENCE_PROVEN);
        assert_eq!(exact.sgx_interrupts_valid, 0xdf);
        assert!(T8140_G17P_SGX_EVENT_SOURCE_MAP_PROVEN);
        assert!(T8140_G17P_FIRMWARE_EVENT_INGRESS_PROVEN);
        assert!(!T8140_G17P_SKSM_COMPLETION_IRQ_PROVEN);
        for expected in D93AP_T8140_G17P_SGX_HOST_IRQ_ROUTES {
            assert_eq!(
                decode_t8140_g17p_sgx_host_irq_route(&exact, expected.source_index),
                Some(expected)
            );
        }
        assert_eq!(decode_t8140_g17p_sgx_host_irq_route(&exact, 5), None);
        assert_eq!(decode_t8140_g17p_sgx_host_irq_route(&exact, 8), None);
        assert_eq!(
            D93AP_T8140_G17P_FIRMWARE_EVENT_INGRESS,
            T8140G17PFirmwareEventIngress {
                physical_aic_irq: 1001,
                source_index: 4,
                rtbuddy_akf_message_type: 2,
            }
        );
        assert_eq!(
            decode_t8140_g17p_sgx_host_irq_route(&exact, 4),
            Some(T8140G17PSgxHostIrqRoute {
                source_index: 4,
                aic_irq: 1001,
                action: T8140G17PSgxHostAction::DrainFirmwareRings,
            })
        );
        assert_eq!(
            exact.gfx_power_management,
            G17AdtPowerManagementProperty::UserPowerManaged
        );
        assert_eq!(
            exact.gfx1_power_management,
            G17AdtPowerManagementProperty::PowerManaged
        );
        assert!(exact.shared_iommu_mapper);
        assert_eq!(exact.perf_state_count, 0);
        assert_eq!(exact.gpu_num_perf_states, 2);
        assert_eq!(exact.perf_state_record_count, 16);
        assert!(exact.perf_state_records_all_zero);
        assert!(!exact.perf_states_sram_present);

        let mut wrong_gfx1_transport = exact;
        wrong_gfx1_transport.gfx1_asc_regs[0].phys_base = exact.gfx_asc_regs[0].phys_base;
        assert!(!validate_t8140_g17p_adt_topology(&wrong_gfx1_transport));
        assert_eq!(decode_t8140_g17p_asc_providers(&wrong_gfx1_transport), None);

        let mut wrong_active_irq = exact;
        wrong_active_irq.gfx1_asc_interrupts[2] = exact.gfx_asc_interrupts[2];
        assert_eq!(decode_t8140_g17p_asc_providers(&wrong_active_irq), None);

        let mut invented_valid_slot = exact;
        invented_valid_slot.sgx_interrupts_valid |= 1 << 5;
        assert!(!validate_t8140_g17p_adt_topology(&invented_valid_slot));
        assert_eq!(
            decode_t8140_g17p_sgx_host_irq_route(&invented_valid_slot, 5),
            None
        );

        let mut detached_gfx1_gate = exact;
        detached_gfx1_gate.gfx1_asc_power_gate_parent = 0;
        assert!(!validate_t8140_g17p_adt_topology(&detached_gfx1_gate));

        let mut invented_legacy_opp_table = exact;
        invented_legacy_opp_table.perf_state_count = 2;
        assert!(!validate_t8140_g17p_adt_topology(
            &invented_legacy_opp_table
        ));

        let mut invented_sram_table = exact;
        invented_sram_table.perf_states_sram_present = true;
        assert!(!validate_t8140_g17p_adt_topology(
            &invented_sram_table
        ));
    }

    #[test]
    fn g17g_and_g17p_pio_bindings_fail_closed_for_unbound_slot_0b() {
        let g17p_slot0 = Some(G17HostPioPhysicalBinding {
            phys_base: 0x3_0101_4000,
            size: 0x4000,
            source: G17HostPioBindingSource::MetaSwInterrupt,
        });
        let g17g_slot0 = Some(G17HostPioPhysicalBinding {
            phys_base: 0x3_8101_4000,
            size: 0x4000,
            source: G17HostPioBindingSource::MetaSwInterrupt,
        });
        let g17p_slot24 = Some(G17HostPioPhysicalBinding {
            phys_base: 0x2_70d4_0000,
            size: 0x4000,
            source: G17HostPioBindingSource::SgxRelativeOffsetTable,
        });
        let g17g_slot24 = Some(G17HostPioPhysicalBinding {
            phys_base: 0x3_70d4_0000,
            size: 0x4000,
            source: G17HostPioBindingSource::SgxRelativeOffsetTable,
        });

        assert!(validate_g17_host_pio_physical_binding(
            G17HostPioPhysicalEvidenceTarget::G17P,
            G17PioSlot::GpcAicSwInt,
            g17p_slot0,
        ));
        assert!(validate_g17_host_pio_physical_binding(
            G17HostPioPhysicalEvidenceTarget::G17G,
            G17PioSlot::GpcAicSwInt,
            g17g_slot0,
        ));
        assert!(validate_g17_host_pio_physical_binding(
            G17HostPioPhysicalEvidenceTarget::G17P,
            G17PioSlot::FenderRegsMcw,
            g17p_slot24,
        ));
        assert!(validate_g17_host_pio_physical_binding(
            G17HostPioPhysicalEvidenceTarget::G17G,
            G17PioSlot::FenderRegsMcw,
            g17g_slot24,
        ));

        for target in [
            G17HostPioPhysicalEvidenceTarget::G17P,
            G17HostPioPhysicalEvidenceTarget::G17G,
        ] {
            assert_eq!(
                g17_host_pio_physical_binding(
                    target,
                    G17PioSlot::PushTelemetryDashboardConfigSpace
                ),
                None
            );
            assert!(validate_g17_host_pio_physical_binding(
                target,
                G17PioSlot::PushTelemetryDashboardConfigSpace,
                None,
            ));
            assert!(!validate_g17_host_pio_physical_binding(
                target,
                G17PioSlot::PushTelemetryDashboardConfigSpace,
                g17p_slot0,
            ));
        }

        assert!(!validate_g17_host_pio_physical_binding(
            G17HostPioPhysicalEvidenceTarget::G17G,
            G17PioSlot::GpcAicSwInt,
            g17p_slot0,
        ));
        assert!(!validate_g17_host_pio_physical_binding(
            G17HostPioPhysicalEvidenceTarget::G17P,
            G17PioSlot::FenderRegsMcw,
            Some(G17HostPioPhysicalBinding {
                phys_base: 0x2_70d4_0000,
                size: 0x2000,
                source: G17HostPioBindingSource::SgxRelativeOffsetTable,
            }),
        ));
    }

    #[test]
    fn g17s_physical_pio_bindings_join_adt_table_and_target_override() {
        let evidence = g17_host_pio_physical_evidence(G17HostPioPhysicalEvidenceTarget::G17S);
        let target_configure = evidence.target_configure.unwrap();
        let slot0 = Some(G17HostPioPhysicalBinding {
            phys_base: 0x2_8041_4000,
            size: 0x4000,
            source: G17HostPioBindingSource::TargetConfigureConstant,
        });
        let slot0b = Some(G17HostPioPhysicalBinding {
            phys_base: 0x2_8425_8000,
            size: 0x4000,
            source: G17HostPioBindingSource::TargetConfigureConstant,
        });
        let slot24 = Some(G17HostPioPhysicalBinding {
            phys_base: 0x21_00d4_0000,
            size: 0x4000,
            source: G17HostPioBindingSource::SgxRelativeOffsetTable,
        });

        assert_eq!(evidence.sgx_phys_base, 0x21_0000_0000);
        assert_eq!(evidence.host_io_page_size, 0x4000);
        assert_eq!(
            evidence.meta_sw_interrupt,
            G17MetaSwInterruptEvidence {
                first_u64: 0x2_8041_4084,
                second_u64: 0x2_8041_4284,
                trailing_u32: 0x80,
            }
        );
        assert_eq!(target_configure.slot_0_phys_base, 0x2_8041_4000);
        assert_eq!(target_configure.slot_0b_phys_base, 0x2_8425_8000);
        assert_eq!(target_configure.slot_size, 0x4000);
        assert_eq!(
            evidence.meta_sw_interrupt.first_u64
                & !((evidence.host_io_page_size as u64) - 1),
            target_configure.slot_0_phys_base
        );
        assert_eq!(G17S_HOST_PIO_RELATIVE_TABLE_TYPES.len(), 19);
        assert!(!G17S_HOST_PIO_RELATIVE_TABLE_TYPES
            .iter()
            .any(|pio_type| *pio_type == G17PioSlot::PushTelemetryDashboardConfigSpace as u8));
        assert!(G17S_HOST_PIO_RELATIVE_TABLE_TYPES
            .iter()
            .any(|pio_type| *pio_type == G17PioSlot::FenderRegsMcw as u8));

        assert!(validate_g17_host_pio_physical_binding(
            G17HostPioPhysicalEvidenceTarget::G17S,
            G17PioSlot::GpcAicSwInt,
            slot0,
        ));
        assert!(validate_g17_host_pio_physical_binding(
            G17HostPioPhysicalEvidenceTarget::G17S,
            G17PioSlot::PushTelemetryDashboardConfigSpace,
            slot0b,
        ));
        assert!(validate_g17_host_pio_physical_binding(
            G17HostPioPhysicalEvidenceTarget::G17S,
            G17PioSlot::FenderRegsMcw,
            slot24,
        ));
        assert!(!validate_g17_host_pio_physical_binding(
            G17HostPioPhysicalEvidenceTarget::G17S,
            G17PioSlot::PushTelemetryDashboardConfigSpace,
            None,
        ));
        assert!(!validate_g17_host_pio_physical_binding(
            G17HostPioPhysicalEvidenceTarget::G17S,
            G17PioSlot::GpcAicSwInt,
            Some(G17HostPioPhysicalBinding {
                phys_base: 0x2_8041_4000,
                size: 0x4000,
                source: G17HostPioBindingSource::MetaSwInterrupt,
            }),
        ));
        assert_eq!(G17S_GFX1_IDLE_STATUS_PHYS, slot24.unwrap().phys_base);
        assert!(!G17_HOST_PIO_LINUX_MMIO_OWNERSHIP_PROVEN);
        assert!(!EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE);
        assert!(!EXECUTABLE_G17_RECOVERY_HANDSHAKE_AVAILABLE);
    }

    #[test]
    fn recovery_word_decoder_has_no_fifth_state() {
        for raw in 0..=3 {
            assert_eq!(decode_recovery_word(raw).unwrap() as u32, raw);
        }
        for raw in [4, 0x22, 0x23, u32::MAX] {
            assert_eq!(
                decode_recovery_word(raw),
                Err(RecoveryError::UnknownWordValue)
            );
        }
    }

    #[test]
    fn g15_and_g16_variants_use_artifact_specific_mapped_state_offsets() {
        assert_eq!(G15_AND_G16G_RECOVERY_WORD_OFFSET, 0x5190);
        assert_eq!(G16X_RECOVERY_WORD_OFFSET, 0x4ae0);
        assert_eq!(
            G15_G16_RECOVERY_LAYOUT_EVIDENCE_TARGETS,
            [
                G15G16RecoveryLayoutEvidenceTarget::G15G,
                G15G16RecoveryLayoutEvidenceTarget::G15S,
                G15G16RecoveryLayoutEvidenceTarget::G15C,
                G15G16RecoveryLayoutEvidenceTarget::G15D,
                G15G16RecoveryLayoutEvidenceTarget::G16G,
                G15G16RecoveryLayoutEvidenceTarget::G16S,
                G15G16RecoveryLayoutEvidenceTarget::G16C,
            ]
        );

        let targets_and_offsets = [
            (G15G16RecoveryLayoutEvidenceTarget::G15G, 0x5190),
            (G15G16RecoveryLayoutEvidenceTarget::G15S, 0x5190),
            (G15G16RecoveryLayoutEvidenceTarget::G15C, 0x5190),
            (G15G16RecoveryLayoutEvidenceTarget::G15D, 0x5190),
            (G15G16RecoveryLayoutEvidenceTarget::G16G, 0x5190),
            (G15G16RecoveryLayoutEvidenceTarget::G16S, 0x4ae0),
            (G15G16RecoveryLayoutEvidenceTarget::G16C, 0x4ae0),
        ];
        for (target, expected_offset) in targets_and_offsets {
            assert_eq!(g15_g16_recovery_word_offset(target), expected_offset);
            for raw in 0..=3 {
                assert_eq!(
                    decode_g15_g16_recovery_word_at_offset(target, expected_offset, raw),
                    Ok(decode_recovery_word(raw).unwrap())
                );
            }
            let other_layout_offset = if expected_offset == 0x5190 {
                0x4ae0
            } else {
                0x5190
            };
            for wrong_offset in [
                0,
                other_layout_offset,
                expected_offset - 4,
                expected_offset + 4,
                u32::MAX,
            ] {
                assert_eq!(
                    decode_g15_g16_recovery_word_at_offset(target, wrong_offset, 1),
                    Err(RecoveryError::UnexpectedWordOffset)
                );
            }
        }

        for raw in [4, 0x22, 0x23, u32::MAX] {
            assert_eq!(
                decode_g15_g16_recovery_word_at_offset(
                    G15G16RecoveryLayoutEvidenceTarget::G15G,
                    G15_AND_G16G_RECOVERY_WORD_OFFSET,
                    raw,
                ),
                Err(RecoveryError::UnknownWordValue)
            );
        }
    }

    #[test]
    fn g17p_recovery_word_uses_the_cross_g17_mapped_state_offset() {
        assert_eq!(G17_RECOVERY_WORD_OFFSET, 0x4ae0);
        assert_eq!(
            G17_RECOVERY_LAYOUT_EVIDENCE_TARGETS,
            [
                G17RecoveryLayoutEvidenceTarget::G17P,
                G17RecoveryLayoutEvidenceTarget::G17G,
                G17RecoveryLayoutEvidenceTarget::G17S,
            ]
        );

        for target in G17_RECOVERY_LAYOUT_EVIDENCE_TARGETS {
            assert_eq!(g17_recovery_word_offset(target), 0x4ae0);
            for raw in 0..=3 {
                assert_eq!(
                    decode_g17_recovery_word_at_offset(target, 0x4ae0, raw),
                    Ok(decode_recovery_word(raw).unwrap())
                );
            }
            for wrong_offset in [0, 0x4adc, 0x4ae4, u32::MAX] {
                assert_eq!(
                    decode_g17_recovery_word_at_offset(target, wrong_offset, 1),
                    Err(RecoveryError::UnexpectedWordOffset)
                );
            }
        }

        for raw in [4, 0x22, 0x23, u32::MAX] {
            assert_eq!(
                decode_g17_recovery_word_at_offset(
                    G17RecoveryLayoutEvidenceTarget::G17P,
                    G17_RECOVERY_WORD_OFFSET,
                    raw,
                ),
                Err(RecoveryError::UnknownWordValue)
            );
        }
    }

    #[test]
    fn host_owns_only_the_two_proven_resume_writes() {
        assert_eq!(
            host_resume_recovery(HostResumePhase::PreRecovery, 1),
            Ok(RecoveryWordState::HostResumedPreRecovery)
        );
        assert_eq!(
            host_resume_recovery(HostResumePhase::PostRecovery, 3),
            Ok(RecoveryWordState::RunningOrPostRecoveryResumed)
        );

        for raw in 0..=3 {
            if raw != 1 {
                assert_eq!(
                    host_resume_recovery(HostResumePhase::PreRecovery, raw),
                    Err(RecoveryError::FirmwareNotStoppedForPhase)
                );
            }
            if raw != 3 {
                assert_eq!(
                    host_resume_recovery(HostResumePhase::PostRecovery, raw),
                    Err(RecoveryError::FirmwareNotStoppedForPhase)
                );
            }
        }
    }

    #[test]
    fn recovery_topology_and_g17s_events_are_bounded() {
        assert_eq!(G17_RECOVERY_INSPECTION_INGRESS_ROLE, 0);
        assert_eq!(G17_RECOVERY_LOG_ROLE_MASK, 0b11);
        assert!(HOST_RESET_FOR_RECOVERY_IS_NOOP);
        assert_eq!(G17S_GFX1_RECOVERY_POWER_EVENTS, [0x22, 0x23]);
        assert!(validate_g17s_gfx1_recovery_power_event(0x22));
        assert!(validate_g17s_gfx1_recovery_power_event(0x23));
        for event in [0, 0x21, 0x24, u8::MAX] {
            assert!(!validate_g17s_gfx1_recovery_power_event(event));
        }
    }

    #[test]
    fn g17_pm_config_follows_the_proven_hal_boundary() {
        assert_eq!(
            g17_pm_config_for_evidence_target(G17PmConfigEvidenceTarget::G17P),
            &G17_HAL200_PM_CONFIG_OPAQUE
        );
        for target in [
            G17PmConfigEvidenceTarget::G17G,
            G17PmConfigEvidenceTarget::G17S,
        ] {
            assert_eq!(
                g17_pm_config_for_evidence_target(target),
                &G17_HAL300_PM_CONFIG_OPAQUE
            );
        }
        assert_ne!(G17_HAL200_PM_CONFIG_OPAQUE, G17_HAL300_PM_CONFIG_OPAQUE);
        assert_eq!(
            G17_PM_CONFIG_EVIDENCE_TARGETS,
            [
                G17PmConfigEvidenceTarget::G17P,
                G17PmConfigEvidenceTarget::G17G,
                G17PmConfigEvidenceTarget::G17S,
            ]
        );
    }

    #[test]
    fn t6050_perf_states1_properties_are_exact_table1_mirrors() {
        const FREQUENCIES: [u32; 14] = [
            0,
            338_000_000,
            486_000_000,
            636_000_000,
            796_000_000,
            888_000_000,
            988_000_000,
            1_084_000_000,
            1_182_000_000,
            1_278_000_000,
            1_374_000_000,
            1_470_000_000,
            1_578_000_000,
            1_620_000_000,
        ];
        const CORE_VOLTS_0: [u32; 14] = [
            125, 605, 620, 655, 705, 715, 745, 770, 800, 840, 880, 925, 1000, 1025,
        ];
        const CORE_VOLTS_1: [u32; 14] = [
            125, 605, 635, 655, 705, 715, 745, 770, 800, 840, 880, 925, 1000, 1040,
        ];
        const SRAM_VOLTS_0: [u32; 14] = [
            775, 775, 775, 775, 775, 775, 775, 775, 800, 840, 880, 925, 1000, 1025,
        ];
        const SRAM_VOLTS_1: [u32; 14] = [
            775, 775, 775, 775, 775, 775, 775, 775, 800, 840, 880, 925, 1000, 1040,
        ];
        const EMPTY: T6050PerfState = T6050PerfState {
            freq_hz: 0,
            volt_mv: 0,
        };

        let mut core = [EMPTY; 28];
        let mut sram = [EMPTY; 28];
        let mut core_mirror = [EMPTY; 14];
        let mut sram_mirror = [EMPTY; 14];
        for index in 0..14 {
            core[index] = T6050PerfState {
                freq_hz: FREQUENCIES[index],
                volt_mv: CORE_VOLTS_0[index],
            };
            core[index + 14] = T6050PerfState {
                freq_hz: FREQUENCIES[index],
                volt_mv: CORE_VOLTS_1[index],
            };
            sram[index] = T6050PerfState {
                freq_hz: FREQUENCIES[index],
                volt_mv: SRAM_VOLTS_0[index],
            };
            sram[index + 14] = T6050PerfState {
                freq_hz: FREQUENCIES[index],
                volt_mv: SRAM_VOLTS_1[index],
            };
            core_mirror[index] = core[index + 14];
            sram_mirror[index] = sram[index + 14];
        }

        let validate = |core: &[T6050PerfState],
                        sram: &[T6050PerfState],
                        core_mirror: &[T6050PerfState],
                        sram_mirror: &[T6050PerfState]| {
            validate_t6050_g17s_perf_state_mirrors(
                T6050_G17S_PERF_STATE_COUNT,
                T6050_G17S_PERF_STATE_TABLE_COUNT,
                core,
                sram,
                core_mirror,
                sram_mirror,
            )
        };

        assert_eq!(validate(&core, &sram, &core_mirror, &sram_mirror), Ok(()));
        assert_eq!(
            &core_mirror[..],
            &core[T6050_G17S_PERF_STATE_COUNT as usize..]
        );
        assert_ne!(
            &core_mirror[..],
            &core[..T6050_G17S_PERF_STATE_COUNT as usize]
        );

        core_mirror[2].volt_mv -= 1;
        assert_eq!(
            validate(&core, &sram, &core_mirror, &sram_mirror),
            Err(T6050PerfStateMirrorError::CoreMirrorMismatch)
        );
        core_mirror[2].volt_mv += 1;
        sram_mirror[13].volt_mv -= 1;
        assert_eq!(
            validate(&core, &sram, &core_mirror, &sram_mirror),
            Err(T6050PerfStateMirrorError::SramMirrorMismatch)
        );
        sram_mirror[13].volt_mv += 1;

        assert_eq!(
            validate_t6050_g17s_perf_state_mirrors(
                13,
                T6050_G17S_PERF_STATE_TABLE_COUNT,
                &core,
                &sram,
                &core_mirror,
                &sram_mirror,
            ),
            Err(T6050PerfStateMirrorError::UnexpectedPerfStateCount)
        );
        assert_eq!(
            validate_t6050_g17s_perf_state_mirrors(
                T6050_G17S_PERF_STATE_COUNT,
                3,
                &core,
                &sram,
                &core_mirror,
                &sram_mirror,
            ),
            Err(T6050PerfStateMirrorError::UnexpectedPerfStateTableCount)
        );
        assert_eq!(
            validate(&core[..27], &sram, &core_mirror, &sram_mirror),
            Err(T6050PerfStateMirrorError::UnexpectedMainCoreShape)
        );
        assert_eq!(
            validate(&core, &sram[..27], &core_mirror, &sram_mirror),
            Err(T6050PerfStateMirrorError::UnexpectedMainSramShape)
        );
        assert_eq!(
            validate(&core, &sram, &core_mirror[..13], &sram_mirror),
            Err(T6050PerfStateMirrorError::UnexpectedCoreMirrorShape)
        );
        assert_eq!(
            validate(&core, &sram, &core_mirror, &sram_mirror[..13]),
            Err(T6050PerfStateMirrorError::UnexpectedSramMirrorShape)
        );
    }

    #[test]
    fn grounded_model_does_not_open_runtime_gates() {
        assert!(GROUNDED_POWER_RECOVERY_MODEL_AVAILABLE);
        assert!(!EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE);
        assert!(!EXECUTABLE_G17_RECOVERY_HANDSHAKE_AVAILABLE);
        assert_eq!(
            RECOVERY_EVIDENCE_TARGETS,
            [
                RecoveryEvidenceTarget::G15G,
                RecoveryEvidenceTarget::G15S,
                RecoveryEvidenceTarget::G15C,
                RecoveryEvidenceTarget::G15D,
                RecoveryEvidenceTarget::G16G,
                RecoveryEvidenceTarget::G16S,
                RecoveryEvidenceTarget::G16C,
                RecoveryEvidenceTarget::G17P,
                RecoveryEvidenceTarget::G17G,
                RecoveryEvidenceTarget::G17S,
            ]
        );
    }

    #[test]
    fn remote_power_kick_acceptance_is_byte_exact() {
        // Grounded decoder 0xfffffc00000237e4: exactly three accepted bits.
        assert_eq!(G17_REMOTE_POWER_KICK_ACCEPT, [0x0010_0000, 0x0100_0000, 0x8000_0000]);
        assert_eq!(G17_REMOTE_POWER_KICK_DECODER_VA, 0xffff_fc00_0002_37e4);
        assert_eq!(G17_REMOTE_POWER_KICK_ABORT_VA, 0xffff_fc00_0002_3970);
        for accepted in G17_REMOTE_POWER_KICK_ACCEPT {
            assert_eq!(classify_remote_power_kick(accepted), RemotePowerKickClass::Accepted);
            assert!(validate_remote_power_kick_emit(accepted).is_ok());
        }
        for forbidden in G17_REMOTE_POWER_KICK_FORBIDDEN {
            assert_eq!(
                classify_remote_power_kick(forbidden),
                RemotePowerKickClass::ForbiddenPanic
            );
            assert_eq!(
                validate_remote_power_kick_emit(forbidden),
                Err(PowerKickEmitError::ForbiddenPanicBit)
            );
        }
        // An unlisted value is silently ignored by the firmware, so emitting
        // it is a driver bug, not a panic.
        assert_eq!(classify_remote_power_kick(0x1234), RemotePowerKickClass::IgnoredNoOp);
        assert_eq!(
            validate_remote_power_kick_emit(0x1234),
            Err(PowerKickEmitError::UnrecognizedNoOp)
        );
        assert!(GROUNDED_POWER_KICK_COMMAND_DECODE_AVAILABLE);
        // Deepening the offline model must not silently open the live gate.
        assert!(!EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE);
    }

    #[test]
    fn scheduler_events_and_power_server_bound_are_exact() {
        for event in G17_SCHEDULER_POWER_EVENT_ACCEPT {
            assert!(scheduler_power_event_recognized(event));
        }
        assert!(!scheduler_power_event_recognized(0x1));
        assert!(!scheduler_power_event_recognized(0x8000_0000));
        // Power-server pending mask: no set bit at index >= 2.
        assert!(power_server_kick_mask_in_bounds(0x1));
        assert!(power_server_kick_mask_in_bounds(0x2));
        assert!(power_server_kick_mask_in_bounds(0x3));
        assert!(!power_server_kick_mask_in_bounds(0x4));
        assert!(!power_server_kick_mask_in_bounds(0x5));
    }

    #[test]
    fn recovery_type_and_firmware_word_writes_are_byte_exact() {
        // Outer decode 0xfffffc0000026274: three accepted recovery-cause bits.
        assert_eq!(G17_RECOVERY_TYPE_ACCEPT, [0x0080_0000, 0x0100_0000, 0x0200_0000]);
        assert_eq!(G17_RECOVERY_DISPATCH_VA, 0xffff_fc00_0002_6274);
        assert_eq!(G17_RECOVERY_TYPE_ABORT_VA, 0xffff_fc00_0002_647c);
        for accepted in G17_RECOVERY_TYPE_ACCEPT {
            assert!(validate_recovery_type(accepted).is_ok());
        }
        assert_eq!(
            validate_recovery_type(0x40_0000),
            Err(RecoveryTypeError::UnexpectedRecoveryType)
        );
        assert_eq!(
            validate_recovery_type(0),
            Err(RecoveryTypeError::UnexpectedRecoveryType)
        );

        // Recovery-word firmware-write side: writes 1 at begin, 0 at complete.
        assert_eq!(G17_FW_STATUS_STRUCT_PTR_GLOBAL_VA, 0xffff_fc00_0010_47b8);
        assert_eq!(G17_RECOVERY_WORD_STRUCT_OFFSET, 0xc);
        assert_eq!(G17_RECOVERY_WORD_FW_WRITE_BEGIN_VA, 0xffff_fc00_0002_6728);
        assert_eq!(G17_RECOVERY_WORD_FW_WRITE_COMPLETE_VA, 0xffff_fc00_0002_6d58);
        assert_eq!(G17_RECOVERY_WORD_FW_WRITTEN_VALUES, [1, 0]);
        assert_eq!(
            firmware_recovery_word_write(FirmwareRecoveryEdge::Begin),
            RecoveryWordState::FirmwareStoppedForPreRecovery
        );
        assert_eq!(
            firmware_recovery_word_write(FirmwareRecoveryEdge::Complete),
            RecoveryWordState::RunningOrPostRecoveryResumed
        );
        // The firmware writes only {0,1}; the earlier 2->3 guess is refuted.
        assert!(firmware_writes_recovery_value(0));
        assert!(firmware_writes_recovery_value(1));
        assert!(!firmware_writes_recovery_value(2));
        assert!(!firmware_writes_recovery_value(3));
        assert!(GROUNDED_RECOVERY_WORD_FIRMWARE_WRITE_SIDE_AVAILABLE);
        // The cross-role 0x22/0x23 delivery stays live-only.
        assert!(!EXECUTABLE_G17_RECOVERY_HANDSHAKE_AVAILABLE);
    }
}
