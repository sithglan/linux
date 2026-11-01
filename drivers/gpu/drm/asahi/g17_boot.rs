// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! Evidence-backed G17 dual-firmware-role boot contract.
//!
//! G17 has two physical ASC roles. `GFX` is the scheduler/recovery ingress and
//! `GFX1` owns the observed internal power transitions. They have distinct
//! transports and init-data roots, but both roots belong to one GPU/UAT
//! address-space owner. This module models only the proven host-side gates and
//! the hash-pinned G17X host/RTBuddy firmware-start handshake. It deliberately
//! performs no endpoint, firmware, or MMIO operations. Grounded power and
//! recovery validation is kept in the disconnected
//! [`agx_power_recovery`] module.

#[cfg(not(test))]
use crate::{
    agx_power_recovery, g17_firmware, g17_initdata, g17_resources, g17_rtkit, g17_submission,
};
#[cfg(test)]
#[path = "agx_power_recovery.rs"]
mod agx_power_recovery;
#[cfg(test)]
#[path = "g17_firmware.rs"]
mod g17_firmware;
#[cfg(test)]
#[path = "g17_initdata.rs"]
mod g17_initdata;
#[cfg(test)]
#[path = "g17_resources.rs"]
mod g17_resources;
#[cfg(test)]
#[path = "g17_rtkit.rs"]
mod g17_rtkit;
#[cfg(test)]
#[path = "g17_submission.rs"]
mod g17_submission;

/// Design generation that requires the dual-role contract.
const G17_GENERATION: u32 = 17;

/// Evidence required before Linux may hand init-data to either G17 role.
pub(crate) mod missing {
    pub(crate) const FIRMWARE_IDENTITY: u16 = 1 << 0;
    pub(crate) const ROLE_RESOURCES: u16 = 1 << 1;
    pub(crate) const TOPOLOGY_REGISTERS: u16 = 1 << 2;
    pub(crate) const UAT_HANDOFF: u16 = 1 << 3;
    pub(crate) const INITDATA_GRAPH: u16 = 1 << 4;
    pub(crate) const RTKIT_TRANSPORT: u16 = 1 << 5;
    pub(crate) const GFX1_POWER_SEQUENCE: u16 = 1 << 6;
    pub(crate) const RECOVERY_HANDSHAKE: u16 = 1 << 7;
    pub(crate) const SUBMISSION_TRANSPORT: u16 = 1 << 8;
}

const REQUIRED_PRE_HANDOFF_EVIDENCE: u16 = missing::FIRMWARE_IDENTITY
    | missing::ROLE_RESOURCES
    | missing::TOPOLOGY_REGISTERS
    | missing::UAT_HANDOFF
    | missing::INITDATA_GRAPH
    | missing::RTKIT_TRANSPORT
    | missing::GFX1_POWER_SEQUENCE
    | missing::RECOVERY_HANDSHAKE
    | missing::SUBMISSION_TRANSPORT;

/// Exact missing-evidence result returned to the probe boundary.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct MissingEvidence(u16);

impl MissingEvidence {
    pub(crate) const fn bits(self) -> u16 {
        self.0
    }

    pub(crate) const fn contains(self, evidence: u16) -> bool {
        self.0 & evidence != 0
    }
}

/// The AGX3 topology/fault register bank is recovered and wired.
///
/// All eight AGX3 kexts in the pinned `26A5378n` corpus use the G14X register
/// family: per-cluster core-enable masks are three u32 words at `0xe01500`
/// (AGXG17X `readChipInfo` @ 0xfffffe0008b4b220, AGXG15G @
/// 0xfffffe0008af22f0), and the MMU fault registers are
/// FAULT_INFO=`0xd8c0`/FAULT_ADDR=`0xd8c8` with the acknowledge write at
/// `0xd800` (AGXG17X @ 0xfffffe0008b58c04). `regs.rs` decodes hardware
/// families 0x7/0x8/0xa/0xb with this bank.
pub(crate) const AGX3_TOPOLOGY_REGISTERS_IMPLEMENTED: bool = true;

const fn implemented_pre_handoff_evidence(
    expected_variant: Option<G17FirmwareVariant>,
    firmware: Option<&g17_firmware::G17FirmwareIdentityAdmission>,
) -> u16 {
    let mut implemented = 0;

    // Only the opaque result of atomic GFX/GFX1 byte validation may satisfy
    // firmware identity. Selecting the expected static table is insufficient,
    // and an admission for one GPU variant cannot satisfy another variant.
    if let (Some(expected), Some(admission)) = (expected_variant, firmware) {
        if expected as u8 == admission.variant() as u8 {
            implemented |= missing::FIRMWARE_IDENTITY;
        }
    }

    // These constants deliberately distinguish useful offline codecs from an
    // executable firmware ABI. When either module becomes complete, this gate
    // changes with the implementation rather than a disconnected probe flag.
    if AGX3_TOPOLOGY_REGISTERS_IMPLEMENTED {
        implemented |= missing::TOPOLOGY_REGISTERS;
    }
    // The old M5 role model below remains diagnostic. T8140 has its own
    // source-backed allocator and absent-handoff UAT constructor.
    if GROUNDED_ROLE_RESOURCE_OWNERSHIP_AVAILABLE && EXECUTABLE_ROLE_RESOURCES_AVAILABLE {
        implemented |= missing::ROLE_RESOURCES;
    }
    if let Some(G17FirmwareVariant::G17P) = expected_variant {
        if g17_resources::EXECUTABLE_T8140_ROLE_RESOURCES_AVAILABLE {
            implemented |= missing::ROLE_RESOURCES;
        }
        if g17_resources::EXECUTABLE_T8140_SHARED_UAT_AVAILABLE {
            implemented |= missing::UAT_HANDOFF;
        }
    }
    if g17_initdata::GROUNDED_LAYOUT_AVAILABLE && g17_initdata::PARSE_ACCEPTED_GRAPH_AVAILABLE {
        implemented |= missing::INITDATA_GRAPH;
    }
    if g17_rtkit::GROUNDED_CODEC_AVAILABLE && g17_rtkit::EXECUTABLE_TRANSPORT_AVAILABLE {
        implemented |= missing::RTKIT_TRANSPORT;
    }
    if agx_power_recovery::GROUNDED_POWER_RECOVERY_MODEL_AVAILABLE
        && agx_power_recovery::EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE
    {
        implemented |= missing::GFX1_POWER_SEQUENCE;
    }
    if agx_power_recovery::GROUNDED_POWER_RECOVERY_MODEL_AVAILABLE
        && agx_power_recovery::EXECUTABLE_G17_RECOVERY_HANDSHAKE_AVAILABLE
    {
        implemented |= missing::RECOVERY_HANDSHAKE;
    }
    // The host equations are grounded, including the G17P fixed-slot scratch
    // geometry and SKSM configuration/addKicks words. Submission transport is
    // complete once the ioctl work item publishes through that register window
    // and routes the matching completion IRQ.
    if g17_submission::GROUNDED_SUBMISSION_PRODUCER_AVAILABLE
        && g17_submission::EXECUTABLE_SUBMISSION_TRANSPORT_AVAILABLE
    {
        implemented |= missing::SUBMISSION_TRANSPORT;
    }

    implemented
}

/// Require all runtime evidence before any G17 ASC is started.
///
/// Older, already-supported generations do not use this gate. The caller maps
/// an error to `ENODEV` before MMIO initialization or firmware CPU startup.
pub(crate) const fn pre_handoff_gate(gpu_generation: u32) -> Result<(), MissingEvidence> {
    apply_pre_handoff_gate(gpu_generation, None, None)
}

/// Apply target-specific implementation evidence without claiming firmware
/// identity. T8140 uses this path because iBoot loads its G17P role images.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const fn pre_handoff_gate_for_variant(
    gpu_generation: u32,
    expected_variant: G17FirmwareVariant,
) -> Result<(), MissingEvidence> {
    apply_pre_handoff_gate(gpu_generation, Some(expected_variant), None)
}

/// Apply the pre-handoff gate with an optional byte-validated role pair.
///
/// The diagnostic G17S probe supplies the opaque result of the exact versioned
/// m1n1 loaded-segment handoff validator here. Other variants remain on
/// [`pre_handoff_gate`]. This seam prevents a static expected-identity lookup
/// or unrelated firmware file from clearing the bit.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const fn pre_handoff_gate_with_identity(
    gpu_generation: u32,
    expected_variant: G17FirmwareVariant,
    firmware: Option<&g17_firmware::G17FirmwareIdentityAdmission>,
) -> Result<(), MissingEvidence> {
    apply_pre_handoff_gate(gpu_generation, Some(expected_variant), firmware)
}

const fn apply_pre_handoff_gate(
    gpu_generation: u32,
    expected_variant: Option<G17FirmwareVariant>,
    firmware: Option<&g17_firmware::G17FirmwareIdentityAdmission>,
) -> Result<(), MissingEvidence> {
    if gpu_generation != G17_GENERATION {
        return Ok(());
    }

    let missing_bits = REQUIRED_PRE_HANDOFF_EVIDENCE
        & !implemented_pre_handoff_evidence(expected_variant, firmware);
    if missing_bits == 0 {
        Ok(())
    } else {
        Err(MissingEvidence(missing_bits))
    }
}

/// The two mandatory G17 firmware roles.
///
/// The order in which they are kicked is a [`KickOrder`] parameter (default
/// primary-first, matching the pinned macOS host and the working bare-metal
/// T8140 host); see that type for why the order is not a constant.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum FirmwareRole {
    /// Scheduler and recovery-ingress role ("primary").
    Gfx = 0,
    /// Power-management / control-only peer role ("secondary").
    Gfx1 = 1,
}

/// G17 variants the firmware-identity machinery can name.
///
/// G17S and G17C carry exact paired nested-firmware evidence from the pinned
/// FTAB corpus. G17P (A18 Pro / T8140) is a named variant because it is a
/// live bring-up target, but the pinned corpus contains **no** role-paired
/// nested images for it: on T8140 the role images are iBoot-loaded carveouts
/// (`gfx-data-base`/`gfx1-data-base`) and the firmware's own init-data
/// version-word gate ("Host and FW Interface is mismatched", see
/// [`crate::g17_initdata::INITDATA_VERSION_WORD`]) is the grounded identity
/// check. [`g17_nested_firmware_identity`] therefore returns `None` for G17P,
/// and every identity-admission path fails closed on it. G17G and other
/// variants remain absent entirely.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum G17FirmwareVariant {
    G17S = b'S',
    G17C = b'C',
    G17P = b'P',
}

/// Strict variant decoding for diagnostic firmware selection.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum FirmwareIdentityError {
    UnknownVariant,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const fn decode_g17_firmware_variant(
    raw: u8,
) -> Result<G17FirmwareVariant, FirmwareIdentityError> {
    match raw {
        b'S' => Ok(G17FirmwareVariant::G17S),
        b'C' => Ok(G17FirmwareVariant::G17C),
        b'P' => Ok(G17FirmwareVariant::G17P),
        _ => Err(FirmwareIdentityError::UnknownVariant),
    }
}

/// One exact nested Mach-O identity from the pinned role-paired FTAB corpus.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct G17NestedFirmwareIdentity {
    pub(crate) entry_tag: [u8; 4],
    pub(crate) size: u32,
    pub(crate) uuid: [u8; 16],
    pub(crate) sha256: [u8; 32],
}

#[cfg_attr(not(test), allow(dead_code))]
const G17S_GFX_FIRMWARE_IDENTITY: G17NestedFirmwareIdentity = G17NestedFirmwareIdentity {
    entry_tag: *b"a010",
    size: 0x11_9a90,
    uuid: [
        0x05, 0x23, 0x65, 0x20, 0x33, 0xf1, 0x3d, 0x41, 0x83, 0xcb, 0x16, 0x9f, 0xaa, 0xad, 0xbc,
        0xa7,
    ],
    sha256: [
        0x31, 0x7d, 0xe3, 0x5b, 0x44, 0x7e, 0x2a, 0x4c, 0xce, 0x0f, 0xb4, 0x67, 0xf0, 0xab, 0x04,
        0xba, 0xa4, 0x2f, 0xce, 0xee, 0x8a, 0x3f, 0x55, 0x99, 0x8e, 0xcf, 0x3c, 0x62, 0x4d, 0x30,
        0x03, 0x1b,
    ],
};

#[cfg_attr(not(test), allow(dead_code))]
const G17C_GFX_FIRMWARE_IDENTITY: G17NestedFirmwareIdentity = G17NestedFirmwareIdentity {
    entry_tag: *b"a000",
    size: 0x11_9a90,
    uuid: [
        0x21, 0xa1, 0x77, 0xa0, 0xc2, 0x7b, 0x30, 0x59, 0xa3, 0xb0, 0x05, 0x6c, 0xdd, 0xac, 0x65,
        0x8b,
    ],
    sha256: [
        0x61, 0xa3, 0x59, 0x3e, 0x78, 0x68, 0x67, 0x29, 0x40, 0x5b, 0x77, 0xe1, 0x67, 0xdd, 0x74,
        0xbf, 0xb2, 0x5b, 0x0c, 0xe5, 0x82, 0x29, 0x65, 0x08, 0x54, 0x40, 0x4b, 0x3f, 0xd5, 0x3f,
        0xd8, 0x9a,
    ],
};

#[cfg_attr(not(test), allow(dead_code))]
const G17S_GFX1_FIRMWARE_IDENTITY: G17NestedFirmwareIdentity = G17NestedFirmwareIdentity {
    entry_tag: *b"a010",
    size: 0x11_5aa8,
    uuid: [
        0x0e, 0x98, 0xaf, 0xb8, 0x06, 0x80, 0x3b, 0x15, 0x93, 0x2e, 0x91, 0x0d, 0x0a, 0x1c, 0xcd,
        0x78,
    ],
    sha256: [
        0x5d, 0x71, 0x92, 0x08, 0xe1, 0x2b, 0xe6, 0xbf, 0xa7, 0x54, 0xca, 0x84, 0x86, 0xf3, 0x67,
        0x39, 0xfc, 0xe7, 0x4b, 0x0c, 0x45, 0x61, 0x8a, 0xd9, 0xa0, 0xf4, 0x4a, 0x18, 0x9f, 0xee,
        0xb3, 0x45,
    ],
};

#[cfg_attr(not(test), allow(dead_code))]
const G17C_GFX1_FIRMWARE_IDENTITY: G17NestedFirmwareIdentity = G17NestedFirmwareIdentity {
    entry_tag: *b"a000",
    size: 0x11_5aa8,
    uuid: [
        0xfe, 0xb8, 0x1f, 0x4a, 0xf0, 0x75, 0x34, 0x49, 0xa6, 0x62, 0x3c, 0x01, 0xc0, 0x99, 0x4b,
        0x4f,
    ],
    sha256: [
        0xcf, 0x81, 0x13, 0xa8, 0xbc, 0x4a, 0x62, 0xc3, 0x76, 0xd1, 0x1a, 0x4d, 0xc5, 0xd5, 0xc5,
        0x42, 0x99, 0x14, 0xb9, 0x5a, 0xb2, 0xd2, 0x19, 0x0a, 0x0e, 0xba, 0xd3, 0x27, 0x8a, 0x32,
        0x34, 0x68,
    ],
};

/// Select only an exact role/variant nested-firmware identity.
///
/// This proves the contents of the pinned containers, not a live G17C target
/// or Linux ownership of FTAB extraction, fixups, or firmware loading.
/// Returns `None` for G17P: the pinned corpus has no role-paired nested
/// images for it (see [`G17FirmwareVariant`]), so no byte identity exists to
/// select, so no caller can admit the image.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const fn g17_nested_firmware_identity(
    role: FirmwareRole,
    variant: G17FirmwareVariant,
) -> Option<&'static G17NestedFirmwareIdentity> {
    match (role, variant) {
        (FirmwareRole::Gfx, G17FirmwareVariant::G17S) => Some(&G17S_GFX_FIRMWARE_IDENTITY),
        (FirmwareRole::Gfx, G17FirmwareVariant::G17C) => Some(&G17C_GFX_FIRMWARE_IDENTITY),
        (FirmwareRole::Gfx1, G17FirmwareVariant::G17S) => Some(&G17S_GFX1_FIRMWARE_IDENTITY),
        (FirmwareRole::Gfx1, G17FirmwareVariant::G17C) => Some(&G17C_GFX1_FIRMWARE_IDENTITY),
        (_, G17FirmwareVariant::G17P) => None,
    }
}

/// Validate tag, UUID, and hash together; no field is sufficient by itself.
/// A variant with no pinned identity (G17P) validates nothing.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn validate_g17_nested_firmware_identity(
    role: FirmwareRole,
    variant: G17FirmwareVariant,
    observed: &G17NestedFirmwareIdentity,
) -> bool {
    match g17_nested_firmware_identity(role, variant) {
        Some(expected) => observed == expected,
        None => false,
    }
}

/// The G17P/G17G/G17S host/RTBuddy firmware-start model is exact but
/// diagnostic only.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const GROUNDED_G17X_FIRMWARE_START_MODEL_AVAILABLE: bool = true;

/// Linux cannot execute this handshake until the opaque loader boundary and
/// its power, cache, interrupt, and completion ownership are recovered.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const EXECUTABLE_G17X_FIRMWARE_START_HANDSHAKE_AVAILABLE: bool = false;

/// Pinned host targets which prove the dual-role boot lifecycle.
///
/// G17P is the A18 Pro HAL200 target, G17G is M5, and G17S is M5 Pro/Max.
/// Requiring an explicit target is load-bearing: G17G shifts multiple host
/// object fields relative to G17P/G17S even though the control flow is the
/// same.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum G17HostBootEvidenceTarget {
    G17P,
    G17G,
    G17S,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const G17_HOST_BOOT_EVIDENCE_TARGETS: [G17HostBootEvidenceTarget; 3] = [
    G17HostBootEvidenceTarget::G17P,
    G17HostBootEvidenceTarget::G17G,
    G17HostBootEvidenceTarget::G17S,
];

/// Target-specific host object layout used by the boot and cold-boot gates.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct G17HostBootLayout {
    pub(crate) role_record_base: u32,
    pub(crate) combined_started: u32,
    pub(crate) accelerator: u16,
    pub(crate) event_source: u16,
    pub(crate) host_booted: u16,
    pub(crate) cold_boot_role0: u16,
    pub(crate) cold_boot_role1: u16,
}

/// Exact text entry points in each hash-pinned host image.
///
/// Sources are the `com.apple.AGXG17P`, `com.apple.AGXG17G`, and
/// `com.apple.AGXG17X` images pinned by
/// `GPU_G17_POWER_BOOT_RECOVERY_2026-08-24.md`. These addresses are
/// provenance anchors, not callable Linux addresses.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct G17HostBootTextEvidence {
    pub(crate) boot_firmware_va: u64,
    pub(crate) notify_started_va: u64,
    pub(crate) complete_cold_boot_va: u64,
    pub(crate) cold_boot_done_va: u64,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const fn g17_host_boot_layout(target: G17HostBootEvidenceTarget) -> G17HostBootLayout {
    match target {
        G17HostBootEvidenceTarget::G17P | G17HostBootEvidenceTarget::G17S => G17HostBootLayout {
            role_record_base: 0x1980,
            combined_started: 0x1a6d,
            accelerator: 0x2a0,
            event_source: 0x3d0,
            host_booted: 0x2b0,
            cold_boot_role0: 0x0b78,
            cold_boot_role1: 0x0ca8,
        },
        G17HostBootEvidenceTarget::G17G => G17HostBootLayout {
            role_record_base: 0x19c8,
            combined_started: 0x1ab5,
            accelerator: 0x2a8,
            event_source: 0x3e0,
            host_booted: 0x2b8,
            cold_boot_role0: 0x0b80,
            cold_boot_role1: 0x0cb0,
        },
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const fn g17_host_boot_text_evidence(
    target: G17HostBootEvidenceTarget,
) -> G17HostBootTextEvidence {
    match target {
        G17HostBootEvidenceTarget::G17P => G17HostBootTextEvidence {
            boot_firmware_va: 0xffff_fe00_08a4_4ce0,
            notify_started_va: 0xffff_fe00_08a2_e744,
            complete_cold_boot_va: 0xffff_fe00_08a2_368c,
            cold_boot_done_va: 0xffff_fe00_08a2_36c0,
        },
        G17HostBootEvidenceTarget::G17G => G17HostBootTextEvidence {
            boot_firmware_va: 0xffff_fe00_08b7_666c,
            notify_started_va: 0xffff_fe00_08b5_fe60,
            complete_cold_boot_va: 0xffff_fe00_08b5_4de4,
            cold_boot_done_va: 0xffff_fe00_08b5_4e18,
        },
        G17HostBootEvidenceTarget::G17S => G17HostBootTextEvidence {
            boot_firmware_va: 0xffff_fe00_08b3_9754,
            notify_started_va: 0xffff_fe00_08b2_25c0,
            complete_cold_boot_va: 0xffff_fe00_08b1_765c,
            cold_boot_done_va: 0xffff_fe00_08b1_7690,
        },
    }
}

/// Exact host-side offsets for one role record in a pinned G17 driver.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct G17xRoleHostOffsets {
    pub(crate) rtbuddy_object: u32,
    pub(crate) token_source_object: u32,
    pub(crate) firmware_map: u32,
    pub(crate) started: u32,
    pub(crate) timestamp: u32,
}

const G17X_ROLE_RECORD_STRIDE: u32 = 0x38;

/// Derive the exact GFX/GFX1 record fields used by `bootFirmware()` and
/// `notifyFirmwareStarted()`. Both roles use the same layout, not the same
/// transport object.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const fn g17_role_host_offsets(
    target: G17HostBootEvidenceTarget,
    role: FirmwareRole,
) -> G17xRoleHostOffsets {
    let record =
        g17_host_boot_layout(target).role_record_base + role as u32 * G17X_ROLE_RECORD_STRIDE;

    G17xRoleHostOffsets {
        rtbuddy_object: record,
        token_source_object: record + 0x10,
        firmware_map: record + 0x20,
        started: record + 0x28,
        timestamp: record + 0x30,
    }
}

/// One exact host operation in `AGXArmFirmware::bootFirmware()`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum G17xHostBootOp {
    ClearRoleStarted {
        role: FirmwareRole,
        offset: u32,
    },
    ClearCombinedStarted {
        offset: u32,
    },
    StartRole {
        role: FirmwareRole,
        rtbuddy_object: u32,
        firmware_map: u32,
        vtable_slot: u16,
    },
    RequireBothRoleStarts,
    WaitForEitherRoleStarted {
        combined_offset: u32,
        accelerator_offset: u16,
        event_source_offset: u16,
        event_vtable_slot: u16,
        deadline_helper_arg0: u32,
        deadline_helper_arg1: u32,
        event_arg3: u32,
        continue_result: u32,
    },
    MarkHostBootedAndNotifyAccelerator {
        offset: u16,
        name_vtable_slot: u16,
        notify_vtable_slot: u16,
    },
}

/// G17 adds a second host role; the role-start virtual slot itself is shared
/// with independently pinned G15C and G16X host implementations.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const G15C_G16X_G17X_ROLE_START_VTABLE_SLOT: u16 = 0x888;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const G15C_G16X_BOOT_ROLE_COUNT: u8 = 1;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const G17X_BOOT_ROLE_COUNT: u8 = 2;

/// Exact target-specific G17 boot order. The wait is satisfied by either
/// role's callback, but both role-start calls must first return nonzero.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const fn g17_host_boot_ops(target: G17HostBootEvidenceTarget) -> [G17xHostBootOp; 8] {
    let layout = g17_host_boot_layout(target);
    let gfx = g17_role_host_offsets(target, FirmwareRole::Gfx);
    let gfx1 = g17_role_host_offsets(target, FirmwareRole::Gfx1);

    [
        G17xHostBootOp::ClearRoleStarted {
            role: FirmwareRole::Gfx,
            offset: gfx.started,
        },
        G17xHostBootOp::ClearRoleStarted {
            role: FirmwareRole::Gfx1,
            offset: gfx1.started,
        },
        G17xHostBootOp::ClearCombinedStarted {
            offset: layout.combined_started,
        },
        G17xHostBootOp::StartRole {
            role: FirmwareRole::Gfx,
            rtbuddy_object: gfx.rtbuddy_object,
            firmware_map: gfx.firmware_map,
            vtable_slot: G15C_G16X_G17X_ROLE_START_VTABLE_SLOT,
        },
        G17xHostBootOp::StartRole {
            role: FirmwareRole::Gfx1,
            rtbuddy_object: gfx1.rtbuddy_object,
            firmware_map: gfx1.firmware_map,
            vtable_slot: G15C_G16X_G17X_ROLE_START_VTABLE_SLOT,
        },
        G17xHostBootOp::RequireBothRoleStarts,
        G17xHostBootOp::WaitForEitherRoleStarted {
            combined_offset: layout.combined_started,
            accelerator_offset: layout.accelerator,
            event_source_offset: layout.event_source,
            event_vtable_slot: 0x210,
            deadline_helper_arg0: 50,
            deadline_helper_arg1: 1_000_000,
            event_arg3: 0,
            continue_result: 1,
        },
        G17xHostBootOp::MarkHostBootedAndNotifyAccelerator {
            offset: layout.host_booted,
            name_vtable_slot: 0x38,
            notify_vtable_slot: 0xa68,
        },
    ]
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn validate_g17_host_boot_ops(
    target: G17HostBootEvidenceTarget,
    ops: &[G17xHostBootOp],
) -> bool {
    ops == g17_host_boot_ops(target)
}

/// Role-local transport endpoint binding recovered from G17X RTBuddy.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct G17xRtBuddyEndpointBinding {
    pub(crate) service_id: u8,
    pub(crate) object_offset: u16,
}

/// These endpoint IDs and object offsets are also byte-identical in the
/// pinned G15C and G16X RTBuddy implementations. Arrival order is not fixed.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const G17X_RTBUDDY_ENDPOINT_BINDINGS: [G17xRtBuddyEndpointBinding; 2] = [
    G17xRtBuddyEndpointBinding {
        service_id: 0x20,
        object_offset: 0x128,
    },
    G17xRtBuddyEndpointBinding {
        service_id: 0x21,
        object_offset: 0x130,
    },
];

/// Ordered convergence after both endpoint objects are non-null.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum G17xRtBuddyStartOp {
    RequireBothEndpoints {
        first_object_offset: u16,
        second_object_offset: u16,
        predicate_vtable_slot: u16,
    },
    EnableEndpoint {
        service_id: u8,
        object_offset: u16,
        vtable_slot: u16,
    },
    NotifyHostRole {
        host_object_offset: u16,
        role_offset: u16,
        host_vtable_slot: u16,
    },
}

/// The authenticated pointer in the pinned `AGXArmFirmwareASC` vtable at
/// object slot `+0xb10` decodes exactly to `notifyFirmwareStarted(role)`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const G17X_NOTIFY_FIRMWARE_STARTED_VTABLE_SLOT: u16 = 0xb10;

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const G17X_RTBUDDY_START_OPS: [G17xRtBuddyStartOp; 4] = [
    G17xRtBuddyStartOp::RequireBothEndpoints {
        first_object_offset: 0x128,
        second_object_offset: 0x130,
        predicate_vtable_slot: 0x890,
    },
    G17xRtBuddyStartOp::EnableEndpoint {
        service_id: 0x20,
        object_offset: 0x128,
        vtable_slot: 0x170,
    },
    G17xRtBuddyStartOp::EnableEndpoint {
        service_id: 0x21,
        object_offset: 0x130,
        vtable_slot: 0x170,
    },
    G17xRtBuddyStartOp::NotifyHostRole {
        host_object_offset: 0x170,
        role_offset: 0x120,
        host_vtable_slot: G17X_NOTIFY_FIRMWARE_STARTED_VTABLE_SLOT,
    },
];

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn validate_g17x_rtbuddy_start_ops(ops: &[G17xRtBuddyStartOp]) -> bool {
    ops == G17X_RTBUDDY_START_OPS
}

/// Exact host callback operations, parameterized by the role-record base.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum G17xNotifyStartedOp {
    SetRoleStarted {
        relative_offset: u8,
    },
    StoreTimestamp {
        relative_offset: u8,
    },
    SetCombinedStarted {
        absolute_offset: u32,
    },
    WakeCombinedStarted {
        accelerator_offset: u16,
        event_source_offset: u16,
        vtable_slot: u16,
        argument: u32,
    },
    QueryRoleToken {
        source_relative_offset: u8,
        source_vtable_slot: u16,
        firmware_vtable_slot: u16,
        firmware_argument: u32,
    },
    EncodeStartedMessage {
        prefix: u64,
        token_bits: u8,
    },
    SendRoleMessage {
        endpoint_relative_offset: u8,
        vtable_slot: u16,
    },
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const fn g17_notify_started_ops(
    target: G17HostBootEvidenceTarget,
) -> [G17xNotifyStartedOp; 7] {
    let layout = g17_host_boot_layout(target);

    [
        G17xNotifyStartedOp::SetRoleStarted {
            relative_offset: 0x28,
        },
        G17xNotifyStartedOp::StoreTimestamp {
            relative_offset: 0x30,
        },
        G17xNotifyStartedOp::SetCombinedStarted {
            absolute_offset: layout.combined_started,
        },
        G17xNotifyStartedOp::WakeCombinedStarted {
            accelerator_offset: layout.accelerator,
            event_source_offset: layout.event_source,
            vtable_slot: 0x208,
            argument: 0,
        },
        G17xNotifyStartedOp::QueryRoleToken {
            source_relative_offset: 0x10,
            source_vtable_slot: 0x158,
            firmware_vtable_slot: 0x2d0,
            firmware_argument: 0,
        },
        G17xNotifyStartedOp::EncodeStartedMessage {
            prefix: 0x0081_0000_0000_0000,
            token_bits: 44,
        },
        G17xNotifyStartedOp::SendRoleMessage {
            endpoint_relative_offset: 0,
            vtable_slot: 0x898,
        },
    ]
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn validate_g17_notify_started_ops(
    target: G17HostBootEvidenceTarget,
    ops: &[G17xNotifyStartedOp],
) -> bool {
    ops == g17_notify_started_ops(target)
}

/// Exact remaining boundaries which keep the diagnostic model non-executable.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct G17xFirmwareStartGaps {
    pub(crate) load_firmware_mmio_and_power_order_proven: bool,
    pub(crate) load_firmware_cache_maintenance_proven: bool,
    pub(crate) power_callback_to_endpoint_causality_proven: bool,
    pub(crate) interrupt_and_ack_route_proven: bool,
    pub(crate) both_cold_boot_flags_to_submission_causality_proven: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const G17X_FIRMWARE_START_GAPS: G17xFirmwareStartGaps = G17xFirmwareStartGaps {
    load_firmware_mmio_and_power_order_proven: false,
    load_firmware_cache_maintenance_proven: true,
    power_callback_to_endpoint_causality_proven: false,
    interrupt_and_ack_route_proven: false,
    both_cold_boot_flags_to_submission_causality_proven: false,
};

const ROLE_COUNT: usize = 2;

/// The order in which the two role start calls are issued.
///
/// The evidence for T8140/G17P is genuinely mixed and this is deliberately a
/// parameter rather than a constant:
///
/// - The pinned macOS host (`AGXArmFirmware::bootFirmware()`, see
///   [`g17_host_boot_ops`]) and the working bare-metal host that booted both
///   T8140 instances issue the **primary (`GFX`) start first**, then `GFX1`.
/// - The published T8140 firmware ABI spec
///   (`docs/t8140-g17p-firmware-abi-spec.md`, "Startup Sequence" steps 7-9)
///   instead documents booting the **secondary/control-only instance first**
///   and sending its initdata before the primary.
///
/// Both cannot be the unique requirement; most likely the constraint is only
/// on descriptor/acknowledgement interleaving rather than raw start order.
/// Until a live A/B experiment settles it, primary-first is the default
/// (matching the host that has actually driven this hardware) and the
/// spec-documented order stays selectable.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum KickOrder {
    /// `GFX` first, then `GFX1`. Matches the pinned macOS host and the
    /// working bare-metal T8140 host. Default.
    PrimaryFirst,
    /// `GFX1` first, then `GFX`. Matches the published T8140 firmware ABI
    /// spec's startup sequence. Not hardware-refuted; not the default.
    SecondaryFirst,
}

impl KickOrder {
    pub(crate) const fn role_at(self, index: usize) -> FirmwareRole {
        match (self, index) {
            (Self::PrimaryFirst, 0) | (Self::SecondaryFirst, 1) => FirmwareRole::Gfx,
            _ => FirmwareRole::Gfx1,
        }
    }
}

impl FirmwareRole {
    const fn index(self) -> usize {
        self as usize
    }
}

/// One GPU allocator/UAT owner shared by both role records.
///
/// This is intentionally stored once in [`DualRoleBoot`]. A role never owns a
/// second UAT or allocator.
#[derive(Debug)]
pub(crate) struct SharedGpuUatOwner;

#[derive(Debug, Copy, Clone, Default)]
struct RoleState {
    initdata_root_ready: bool,
    kick_succeeded: bool,
    started_notified: bool,
    cold_boot_complete: bool,
}

/// Errors in the host-observed boot sequence.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum BootError {
    /// Both role roots must be built and validated before the first kick.
    InitdataRootsIncomplete,
    /// A role was kicked out of the exact order selected by [`KickOrder`].
    WrongKickOrder,
    /// A role start call failed; the boot attempt is permanently failed.
    KickFailed,
    /// A root was changed after the first role kick.
    RootMutationAfterKick,
    /// An earlier sequencing error already failed this boot attempt.
    AlreadyFailed,
}

/// Pure state model for one G17 dual-role cold boot.
///
/// Notifications may race with the host's start calls, so notification methods
/// only record flags. The three public gate predicates remain independent, and
/// [`Self::handoff_ready`] composes them before submission can be exposed.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct DualRoleBoot {
    owner: SharedGpuUatOwner,
    roles: [RoleState; ROLE_COUNT],
    kick_order: KickOrder,
    next_kick: usize,
    failed: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl DualRoleBoot {
    /// Create an empty boot attempt with one shared GPU/UAT owner, using the
    /// default primary-first kick order (see [`KickOrder`]).
    pub(crate) const fn new() -> Self {
        Self::new_with_order(KickOrder::PrimaryFirst)
    }

    /// Create an empty boot attempt with an explicit kick order.
    pub(crate) const fn new_with_order(kick_order: KickOrder) -> Self {
        Self {
            owner: SharedGpuUatOwner,
            roles: [
                RoleState {
                    initdata_root_ready: false,
                    kick_succeeded: false,
                    started_notified: false,
                    cold_boot_complete: false,
                },
                RoleState {
                    initdata_root_ready: false,
                    kick_succeeded: false,
                    started_notified: false,
                    cold_boot_complete: false,
                },
            ],
            kick_order,
            next_kick: 0,
            failed: false,
        }
    }

    /// The kick order this boot attempt enforces.
    pub(crate) const fn kick_order(&self) -> KickOrder {
        self.kick_order
    }

    /// Return the single address-space owner used by either role.
    pub(crate) const fn owner_for(&self, _role: FirmwareRole) -> &SharedGpuUatOwner {
        &self.owner
    }

    /// Mark one role-specific init-data root ready for the shared UAT.
    pub(crate) fn mark_initdata_root_ready(&mut self, role: FirmwareRole) -> Result<(), BootError> {
        if self.failed {
            return Err(BootError::AlreadyFailed);
        }
        if self.next_kick != 0 {
            self.failed = true;
            return Err(BootError::RootMutationAfterKick);
        }

        self.roles[role.index()].initdata_root_ready = true;
        Ok(())
    }

    /// Record the return value of the next role start call.
    ///
    /// Both roots must already exist, and calls must be recorded in the exact
    /// order selected at construction ([`KickOrder`]; primary-first by
    /// default). Any failure permanently closes all gates.
    pub(crate) fn record_kick(
        &mut self,
        role: FirmwareRole,
        succeeded: bool,
    ) -> Result<(), BootError> {
        if self.failed {
            return Err(BootError::AlreadyFailed);
        }
        if self.roles.iter().any(|state| !state.initdata_root_ready) {
            self.failed = true;
            return Err(BootError::InitdataRootsIncomplete);
        }
        if self.next_kick >= ROLE_COUNT || self.kick_order.role_at(self.next_kick) != role {
            self.failed = true;
            return Err(BootError::WrongKickOrder);
        }
        if !succeeded {
            self.failed = true;
            return Err(BootError::KickFailed);
        }

        self.roles[role.index()].kick_succeeded = true;
        self.next_kick += 1;
        Ok(())
    }

    /// Record the role's started callback.
    ///
    /// Apple's combined started byte is set by either callback; it is not an
    /// AND of both role flags.
    pub(crate) fn notify_started(&mut self, role: FirmwareRole) {
        self.roles[role.index()].started_notified = true;
    }

    /// Record one role's later cold-boot-complete callback.
    pub(crate) fn notify_cold_boot_complete(&mut self, role: FirmwareRole) {
        self.roles[role.index()].cold_boot_complete = true;
    }

    /// Both ordered role start calls returned success.
    pub(crate) fn kick_gate_open(&self) -> bool {
        !self.failed
            && self.next_kick == ROLE_COUNT
            && self.roles.iter().all(|state| state.kick_succeeded)
    }

    /// At least one role delivered its started callback.
    pub(crate) fn early_start_gate_open(&self) -> bool {
        !self.failed && self.roles.iter().any(|state| state.started_notified)
    }

    /// Both per-role cold-boot flags are set.
    pub(crate) fn cold_boot_gate_open(&self) -> bool {
        !self.failed && self.roles.iter().all(|state| state.cold_boot_complete)
    }

    /// Full readiness requires every independent host-observed gate.
    pub(crate) fn handoff_ready(&self) -> bool {
        self.kick_gate_open() && self.early_start_gate_open() && self.cold_boot_gate_open()
    }
}

// -------------------------------------------------------------------------
// ROLE_RESOURCES — per-role firmware resource ownership.
//
// Grounded 2026-08-25 by disassembling the on-disk M5 firmware
// `GFX.a010.macho` / `GFX1.a010.macho` (`otool -tvV`). The three facts below
// are read first-hand from the bytes; every address is `0xfffffc...`.
//
//   FW-1  The init-data parser copies the 0xc8-byte root header into the
//         firmware global at 0x104890 (`adrp x9, 0xfffffc0000104000 ;
//         add x9, x9, #0x890` @ 0x212e8-0x212ec, `str q0,[x9]` @ 0x212f0).
//   FW-2  The *only* parse-time value gate is the host_mapped_allocations
//         word at root+0x2c = global 0x1048bc (`ldr w9,[x9,#0x2c]` @ 0x2132c;
//         `cbz w9, abort` @ 0x21330). Already modeled by g17_initdata.
//   FW-3  The role selector root+0x28 = global 0x1048b8 is written by the
//         header copy but is **never read** by any GFX firmware code: page
//         0xfffffc0000104000 is referenced 418 times, none at offset 0x8b8,
//         and the only two 0x8b8 loads/stores in the image (0x39fe0/0x39fec)
//         target the unrelated page 0xfffffc0000072000. So the firmware does
//         **not** self-validate the host-declared role.
//   FW-4  Role 1 (GFX1) owns a role-specific runtime resource at
//         runtime_pointers+0x481 (unaligned): the `#0x481` field offset is
//         referenced 78 times in `GFX1.a010.macho` and 0 times in
//         `GFX.a010.macho`. Its host allocation size (0x12688) is a host
//         alloc-table fact, not a firmware constant.
//
// Consequence: ROLE_RESOURCES is *not* a firmware parse-time acceptance
// check. Role identity is intrinsic to which nested image is loaded
// (a010/a000 per variant), and because the firmware trusts root+0x28 blindly
// (FW-3), the driver's own validator is the sole boundary that keeps a role
// from being handed the wrong resource set. The ownership *contract* is
// groundable and encodable offline; the actual per-role allocation and UAT
// mapping is live DMA. The admission bit therefore remains unset:
// `EXECUTABLE_ROLE_RESOURCES_AVAILABLE = false`.
// -------------------------------------------------------------------------

/// Firmware global that receives the copied init-data root header (FW-1).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const G17_ROOT_HEADER_FW_GLOBAL: u64 = 0xffff_fc00_0010_4890;
/// Byte offset of the role selector inside the copied root header (root+0x28).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const G17_ROOT_ROLE_SELECTOR_OFFSET: u32 = 0x28;
/// Number of times the GFX1-only runtime resource offset (+0x481) is
/// referenced in each role image: proven role-specific ownership (FW-4).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const G17_GFX1_RUNTIME_RESOURCE_REFS_IN_GFX1: u32 = 78;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const G17_GFX1_RUNTIME_RESOURCE_REFS_IN_GFX: u32 = 0;

/// The per-role resource-ownership *contract* is byte-grounded and encodable
/// offline (FW-1..FW-4). This does not assert that the driver has wired the
/// live allocation/mapping.
pub(crate) const GROUNDED_ROLE_RESOURCE_OWNERSHIP_AVAILABLE: bool = true;
/// Whether Linux may execute the per-role resource plumbing. It may not:
/// there is no firmware parse-time role check to satisfy (FW-3), so the
/// driver's validator is the safety boundary, and the actual sub-allocation
/// and UAT mapping is a live DMA activity that cannot be validated from the
/// machos. A wrong role/resource pairing is silently accepted by the firmware
/// and only fails as a functional fault a live M5 boot exposes.
pub(crate) const EXECUTABLE_ROLE_RESOURCES_AVAILABLE: bool = false;

/// Strict per-role resource-ownership description.
///
/// Both roles share exactly one GPU/UAT owner ([`SharedGpuUatOwner`]); only
/// GFX1 owns the runtime resource at runtime_pointers+0x481 (FW-4).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct RoleResourceOwnership {
    pub(crate) role: FirmwareRole,
    /// A non-zero GPU VA for the GFX1-only runtime resource (rt+0x481).
    /// Required for GFX1, must be zero for GFX.
    pub(crate) gfx1_runtime_resource: u64,
}

/// Role-resource validation errors.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum RoleResourceError {
    /// GFX carried a GFX1-only resource, or GFX1 omitted its required one.
    RoleSpecificResourceViolation,
    /// A required resource pointer was null.
    NullRequiredResource,
    /// The two roles did not resolve to one shared GPU/UAT owner.
    SharedOwnerAliasViolation,
}

/// Validate one role's resource ownership.
///
/// Because the firmware never self-validates the role (FW-3), this is the only
/// place a role/resource mismatch is caught before handoff. It refuses a null
/// required resource and any cross-role resource ownership.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn validate_role_resource_ownership(
    ownership: &RoleResourceOwnership,
) -> Result<(), RoleResourceError> {
    match ownership.role {
        FirmwareRole::Gfx => {
            if ownership.gfx1_runtime_resource != 0 {
                return Err(RoleResourceError::RoleSpecificResourceViolation);
            }
        }
        FirmwareRole::Gfx1 => {
            if ownership.gfx1_runtime_resource == 0 {
                return Err(RoleResourceError::NullRequiredResource);
            }
        }
    }
    Ok(())
}

/// Validate that the two roles in a boot resolve to one GPU/UAT owner and hold
/// mutually consistent, role-correct resources.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn validate_dual_role_resources(
    boot: &DualRoleBoot,
    ownership: &[RoleResourceOwnership; ROLE_COUNT],
) -> Result<(), RoleResourceError> {
    if !core::ptr::eq(
        boot.owner_for(FirmwareRole::Gfx),
        boot.owner_for(FirmwareRole::Gfx1),
    ) {
        return Err(RoleResourceError::SharedOwnerAliasViolation);
    }
    if ownership[FirmwareRole::Gfx.index()].role != FirmwareRole::Gfx
        || ownership[FirmwareRole::Gfx1.index()].role != FirmwareRole::Gfx1
    {
        return Err(RoleResourceError::RoleSpecificResourceViolation);
    }
    for one in ownership {
        validate_role_resource_ownership(one)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots_ready() -> DualRoleBoot {
        let mut boot = DualRoleBoot::new();
        boot.mark_initdata_root_ready(FirmwareRole::Gfx).unwrap();
        boot.mark_initdata_root_ready(FirmwareRole::Gfx1).unwrap();
        boot
    }

    fn roles_kicked() -> DualRoleBoot {
        let mut boot = roots_ready();
        boot.record_kick(FirmwareRole::Gfx, true).unwrap();
        boot.record_kick(FirmwareRole::Gfx1, true).unwrap();
        boot
    }

    #[test]
    fn both_roles_resolve_to_one_gpu_uat_owner() {
        let boot = DualRoleBoot::new();
        assert!(core::ptr::eq(
            boot.owner_for(FirmwareRole::Gfx),
            boot.owner_for(FirmwareRole::Gfx1)
        ));
    }

    #[test]
    fn both_roots_are_required_before_first_kick() {
        let mut boot = DualRoleBoot::new();
        boot.mark_initdata_root_ready(FirmwareRole::Gfx).unwrap();
        assert_eq!(
            boot.record_kick(FirmwareRole::Gfx, true),
            Err(BootError::InitdataRootsIncomplete)
        );
        assert!(!boot.kick_gate_open());
        assert_eq!(
            boot.mark_initdata_root_ready(FirmwareRole::Gfx1),
            Err(BootError::AlreadyFailed)
        );
    }

    #[test]
    fn kick_order_is_exact_and_both_calls_must_succeed() {
        let mut wrong_order = roots_ready();
        assert_eq!(
            wrong_order.record_kick(FirmwareRole::Gfx1, true),
            Err(BootError::WrongKickOrder)
        );
        assert!(!wrong_order.kick_gate_open());

        let mut second_failed = roots_ready();
        second_failed.record_kick(FirmwareRole::Gfx, true).unwrap();
        assert_eq!(
            second_failed.record_kick(FirmwareRole::Gfx1, false),
            Err(BootError::KickFailed)
        );
        assert!(!second_failed.kick_gate_open());

        assert!(roles_kicked().kick_gate_open());
    }

    #[test]
    fn kick_order_is_a_parameter_with_primary_first_default() {
        // Default matches the working T8140 host: primary first.
        assert_eq!(DualRoleBoot::new().kick_order(), KickOrder::PrimaryFirst);
        assert_eq!(KickOrder::PrimaryFirst.role_at(0), FirmwareRole::Gfx);
        assert_eq!(KickOrder::PrimaryFirst.role_at(1), FirmwareRole::Gfx1);
        // The spec-documented alternative stays selectable.
        assert_eq!(KickOrder::SecondaryFirst.role_at(0), FirmwareRole::Gfx1);
        assert_eq!(KickOrder::SecondaryFirst.role_at(1), FirmwareRole::Gfx);

        let mut boot = DualRoleBoot::new_with_order(KickOrder::SecondaryFirst);
        boot.mark_initdata_root_ready(FirmwareRole::Gfx).unwrap();
        boot.mark_initdata_root_ready(FirmwareRole::Gfx1).unwrap();
        // Under secondary-first, kicking GFX first is the order violation.
        assert_eq!(
            boot.record_kick(FirmwareRole::Gfx, true),
            Err(BootError::WrongKickOrder)
        );

        let mut boot = DualRoleBoot::new_with_order(KickOrder::SecondaryFirst);
        boot.mark_initdata_root_ready(FirmwareRole::Gfx).unwrap();
        boot.mark_initdata_root_ready(FirmwareRole::Gfx1).unwrap();
        boot.record_kick(FirmwareRole::Gfx1, true).unwrap();
        boot.record_kick(FirmwareRole::Gfx, true).unwrap();
        assert!(boot.kick_gate_open());
    }

    #[test]
    fn g17p_variant_decodes_but_pins_no_firmware_identity() {
        assert_eq!(
            decode_g17_firmware_variant(b'P'),
            Ok(G17FirmwareVariant::G17P)
        );
        for role in [FirmwareRole::Gfx, FirmwareRole::Gfx1] {
            assert!(g17_nested_firmware_identity(role, G17FirmwareVariant::G17P).is_none());
        }
    }

    #[test]
    fn early_gate_is_either_role_not_both_roles() {
        for role in [FirmwareRole::Gfx, FirmwareRole::Gfx1] {
            let mut boot = roles_kicked();
            assert!(!boot.early_start_gate_open());
            boot.notify_started(role);
            assert!(boot.early_start_gate_open());
            assert!(!boot.cold_boot_gate_open());
            assert!(!boot.handoff_ready());
        }
    }

    #[test]
    fn cold_gate_requires_both_roles() {
        let mut boot = roles_kicked();
        boot.notify_started(FirmwareRole::Gfx);
        boot.notify_cold_boot_complete(FirmwareRole::Gfx);
        assert!(!boot.cold_boot_gate_open());
        assert!(!boot.handoff_ready());

        boot.notify_cold_boot_complete(FirmwareRole::Gfx1);
        assert!(boot.cold_boot_gate_open());
        assert!(boot.handoff_ready());
    }

    #[test]
    fn notifications_do_not_bypass_failed_kick_gate() {
        let mut boot = roots_ready();
        boot.record_kick(FirmwareRole::Gfx, false).unwrap_err();
        for role in [FirmwareRole::Gfx, FirmwareRole::Gfx1] {
            boot.notify_started(role);
            boot.notify_cold_boot_complete(role);
        }
        assert!(!boot.kick_gate_open());
        assert!(!boot.early_start_gate_open());
        assert!(!boot.cold_boot_gate_open());
        assert!(!boot.handoff_ready());
    }

    #[test]
    fn root_mutation_after_kick_is_invalid() {
        let mut boot = roots_ready();
        boot.record_kick(FirmwareRole::Gfx, true).unwrap();
        assert_eq!(
            boot.mark_initdata_root_ready(FirmwareRole::Gfx1),
            Err(BootError::RootMutationAfterKick)
        );
        assert!(!boot.kick_gate_open());
    }

    #[test]
    fn nested_firmware_identity_is_exact_per_role_and_variant() {
        for role in [FirmwareRole::Gfx, FirmwareRole::Gfx1] {
            // G17P has no pinned nested images; selection is unavailable.
            assert!(g17_nested_firmware_identity(role, G17FirmwareVariant::G17P).is_none());

            let g17s = g17_nested_firmware_identity(role, G17FirmwareVariant::G17S).unwrap();
            let g17c = g17_nested_firmware_identity(role, G17FirmwareVariant::G17C).unwrap();
            assert_eq!(g17s.entry_tag, *b"a010");
            assert_eq!(g17c.entry_tag, *b"a000");
            assert_eq!(g17s.size, g17c.size);
            assert_eq!(
                g17s.size,
                match role {
                    FirmwareRole::Gfx => 0x11_9a90,
                    FirmwareRole::Gfx1 => 0x11_5aa8,
                }
            );
            assert_ne!(g17s.uuid, g17c.uuid);
            assert_ne!(g17s.sha256, g17c.sha256);
            assert!(validate_g17_nested_firmware_identity(
                role,
                G17FirmwareVariant::G17S,
                g17s,
            ));
            assert!(validate_g17_nested_firmware_identity(
                role,
                G17FirmwareVariant::G17C,
                g17c,
            ));

            let mut changed = *g17s;
            changed.sha256[0] ^= 1;
            assert!(!validate_g17_nested_firmware_identity(
                role,
                G17FirmwareVariant::G17S,
                &changed,
            ));
            assert!(!validate_g17_nested_firmware_identity(
                role,
                G17FirmwareVariant::G17C,
                g17s,
            ));
            // A variant with no pinned identity validates nothing.
            assert!(!validate_g17_nested_firmware_identity(
                role,
                G17FirmwareVariant::G17P,
                g17s,
            ));
        }

        assert_eq!(
            decode_g17_firmware_variant(b'S'),
            Ok(G17FirmwareVariant::G17S)
        );
        assert_eq!(
            decode_g17_firmware_variant(b'C'),
            Ok(G17FirmwareVariant::G17C)
        );
        assert_eq!(
            decode_g17_firmware_variant(b'P'),
            Ok(G17FirmwareVariant::G17P)
        );
        for raw in [b'G', b'A', 0, u8::MAX] {
            assert_eq!(
                decode_g17_firmware_variant(raw),
                Err(FirmwareIdentityError::UnknownVariant)
            );
        }

        let missing = pre_handoff_gate(G17_GENERATION).unwrap_err();
        assert!(missing.contains(missing::FIRMWARE_IDENTITY));
    }

    #[test]
    fn only_an_atomic_pair_admission_clears_firmware_identity() {
        let default_missing = pre_handoff_gate(G17_GENERATION).unwrap_err();
        assert!(default_missing.contains(missing::FIRMWARE_IDENTITY));

        for variant in [G17FirmwareVariant::G17S, G17FirmwareVariant::G17C] {
            let admission = g17_firmware::G17FirmwareIdentityAdmission::test_only(variant);
            assert_eq!(admission.variant(), variant);
            let admitted =
                pre_handoff_gate_with_identity(G17_GENERATION, variant, Some(&admission))
                    .unwrap_err();
            assert!(!admitted.contains(missing::FIRMWARE_IDENTITY));
            assert_eq!(
                admitted.bits(),
                default_missing.bits() & !missing::FIRMWARE_IDENTITY
            );

            for generation in 13..G17_GENERATION {
                assert!(
                    pre_handoff_gate_with_identity(generation, variant, Some(&admission)).is_ok()
                );
            }

            let wrong_variant = match variant {
                G17FirmwareVariant::G17S => G17FirmwareVariant::G17C,
                G17FirmwareVariant::G17C | G17FirmwareVariant::G17P => G17FirmwareVariant::G17S,
            };
            let mismatched =
                pre_handoff_gate_with_identity(G17_GENERATION, wrong_variant, Some(&admission))
                    .unwrap_err();
            assert!(mismatched.contains(missing::FIRMWARE_IDENTITY));
            assert_eq!(mismatched.bits(), default_missing.bits());
        }
    }

    #[test]
    fn g17_role_records_are_target_specific_and_role_distinct() {
        assert_eq!(G15C_G16X_BOOT_ROLE_COUNT, 1);
        assert_eq!(G17X_BOOT_ROLE_COUNT, 2);
        assert_eq!(G15C_G16X_G17X_ROLE_START_VTABLE_SLOT, 0x888);
        assert_eq!(
            G17_HOST_BOOT_EVIDENCE_TARGETS,
            [
                G17HostBootEvidenceTarget::G17P,
                G17HostBootEvidenceTarget::G17G,
                G17HostBootEvidenceTarget::G17S,
            ]
        );

        assert_eq!(
            g17_role_host_offsets(G17HostBootEvidenceTarget::G17S, FirmwareRole::Gfx),
            G17xRoleHostOffsets {
                rtbuddy_object: 0x1980,
                token_source_object: 0x1990,
                firmware_map: 0x19a0,
                started: 0x19a8,
                timestamp: 0x19b0,
            }
        );
        assert_eq!(
            g17_role_host_offsets(G17HostBootEvidenceTarget::G17S, FirmwareRole::Gfx1),
            G17xRoleHostOffsets {
                rtbuddy_object: 0x19b8,
                token_source_object: 0x19c8,
                firmware_map: 0x19d8,
                started: 0x19e0,
                timestamp: 0x19e8,
            }
        );
        assert_eq!(
            g17_role_host_offsets(G17HostBootEvidenceTarget::G17G, FirmwareRole::Gfx),
            G17xRoleHostOffsets {
                rtbuddy_object: 0x19c8,
                token_source_object: 0x19d8,
                firmware_map: 0x19e8,
                started: 0x19f0,
                timestamp: 0x19f8,
            }
        );
        assert_eq!(
            g17_role_host_offsets(G17HostBootEvidenceTarget::G17G, FirmwareRole::Gfx1),
            G17xRoleHostOffsets {
                rtbuddy_object: 0x1a00,
                token_source_object: 0x1a10,
                firmware_map: 0x1a20,
                started: 0x1a28,
                timestamp: 0x1a30,
            }
        );
        assert_eq!(
            g17_role_host_offsets(G17HostBootEvidenceTarget::G17P, FirmwareRole::Gfx),
            g17_role_host_offsets(G17HostBootEvidenceTarget::G17S, FirmwareRole::Gfx)
        );
    }

    #[test]
    fn g17_host_boot_order_and_either_role_wait_are_target_exact() {
        let g17s = g17_host_boot_ops(G17HostBootEvidenceTarget::G17S);
        assert!(validate_g17_host_boot_ops(
            G17HostBootEvidenceTarget::G17S,
            &g17s
        ));
        assert_eq!(
            g17s[3],
            G17xHostBootOp::StartRole {
                role: FirmwareRole::Gfx,
                rtbuddy_object: 0x1980,
                firmware_map: 0x19a0,
                vtable_slot: 0x888,
            }
        );
        assert_eq!(
            g17s[4],
            G17xHostBootOp::StartRole {
                role: FirmwareRole::Gfx1,
                rtbuddy_object: 0x19b8,
                firmware_map: 0x19d8,
                vtable_slot: 0x888,
            }
        );
        assert_eq!(
            g17s[6],
            G17xHostBootOp::WaitForEitherRoleStarted {
                combined_offset: 0x1a6d,
                accelerator_offset: 0x2a0,
                event_source_offset: 0x3d0,
                event_vtable_slot: 0x210,
                deadline_helper_arg0: 50,
                deadline_helper_arg1: 1_000_000,
                event_arg3: 0,
                continue_result: 1,
            }
        );
        assert_eq!(
            g17s[7],
            G17xHostBootOp::MarkHostBootedAndNotifyAccelerator {
                offset: 0x2b0,
                name_vtable_slot: 0x38,
                notify_vtable_slot: 0xa68,
            }
        );

        // D93AP uses two physical ASC transports. The pinned G17P host starts
        // GFX, then GFX1, requires both calls to succeed, and only then waits
        // on the combined-started byte. Its object layout matches G17S.
        let g17p = g17_host_boot_ops(G17HostBootEvidenceTarget::G17P);
        assert!(validate_g17_host_boot_ops(
            G17HostBootEvidenceTarget::G17P,
            &g17p
        ));
        assert_eq!(g17p, g17s);
        assert_eq!(g17p[5], G17xHostBootOp::RequireBothRoleStarts);
        assert!(matches!(
            g17p[6],
            G17xHostBootOp::WaitForEitherRoleStarted { .. }
        ));

        let g17g = g17_host_boot_ops(G17HostBootEvidenceTarget::G17G);
        assert!(validate_g17_host_boot_ops(
            G17HostBootEvidenceTarget::G17G,
            &g17g
        ));
        assert_eq!(
            g17g[6],
            G17xHostBootOp::WaitForEitherRoleStarted {
                combined_offset: 0x1ab5,
                accelerator_offset: 0x2a8,
                event_source_offset: 0x3e0,
                event_vtable_slot: 0x210,
                deadline_helper_arg0: 50,
                deadline_helper_arg1: 1_000_000,
                event_arg3: 0,
                continue_result: 1,
            }
        );
        assert!(!validate_g17_host_boot_ops(
            G17HostBootEvidenceTarget::G17S,
            &g17g
        ));

        let mut wrong_order = g17s;
        wrong_order.swap(3, 4);
        assert!(!validate_g17_host_boot_ops(
            G17HostBootEvidenceTarget::G17S,
            &wrong_order
        ));
        assert!(!validate_g17_host_boot_ops(
            G17HostBootEvidenceTarget::G17S,
            &g17s[..7]
        ));
    }

    #[test]
    fn g17_host_boot_text_and_cold_boot_flags_are_target_pinned() {
        assert_eq!(
            g17_host_boot_text_evidence(G17HostBootEvidenceTarget::G17P),
            G17HostBootTextEvidence {
                boot_firmware_va: 0xffff_fe00_08a4_4ce0,
                notify_started_va: 0xffff_fe00_08a2_e744,
                complete_cold_boot_va: 0xffff_fe00_08a2_368c,
                cold_boot_done_va: 0xffff_fe00_08a2_36c0,
            }
        );
        assert_eq!(
            g17_host_boot_text_evidence(G17HostBootEvidenceTarget::G17G),
            G17HostBootTextEvidence {
                boot_firmware_va: 0xffff_fe00_08b7_666c,
                notify_started_va: 0xffff_fe00_08b5_fe60,
                complete_cold_boot_va: 0xffff_fe00_08b5_4de4,
                cold_boot_done_va: 0xffff_fe00_08b5_4e18,
            }
        );
        assert_eq!(
            g17_host_boot_text_evidence(G17HostBootEvidenceTarget::G17S),
            G17HostBootTextEvidence {
                boot_firmware_va: 0xffff_fe00_08b3_9754,
                notify_started_va: 0xffff_fe00_08b2_25c0,
                complete_cold_boot_va: 0xffff_fe00_08b1_765c,
                cold_boot_done_va: 0xffff_fe00_08b1_7690,
            }
        );

        let g17p = g17_host_boot_layout(G17HostBootEvidenceTarget::G17P);
        let g17g = g17_host_boot_layout(G17HostBootEvidenceTarget::G17G);
        let g17s = g17_host_boot_layout(G17HostBootEvidenceTarget::G17S);
        assert_eq!((g17p.cold_boot_role0, g17p.cold_boot_role1), (0xb78, 0xca8));
        assert_eq!((g17g.cold_boot_role0, g17g.cold_boot_role1), (0xb80, 0xcb0));
        assert_eq!((g17s.cold_boot_role0, g17s.cold_boot_role1), (0xb78, 0xca8));
    }

    #[test]
    fn g17x_rtbuddy_convergence_requires_both_endpoints_then_notifies_role() {
        assert_eq!(
            G17X_RTBUDDY_ENDPOINT_BINDINGS,
            [
                G17xRtBuddyEndpointBinding {
                    service_id: 0x20,
                    object_offset: 0x128,
                },
                G17xRtBuddyEndpointBinding {
                    service_id: 0x21,
                    object_offset: 0x130,
                },
            ]
        );
        assert!(validate_g17x_rtbuddy_start_ops(&G17X_RTBUDDY_START_OPS));
        assert_eq!(
            G17X_RTBUDDY_START_OPS[3],
            G17xRtBuddyStartOp::NotifyHostRole {
                host_object_offset: 0x170,
                role_offset: 0x120,
                host_vtable_slot: 0xb10,
            }
        );

        let mut wrong_enable_order = G17X_RTBUDDY_START_OPS;
        wrong_enable_order.swap(1, 2);
        assert!(!validate_g17x_rtbuddy_start_ops(&wrong_enable_order));
    }

    #[test]
    fn g17_started_callback_message_and_wakeup_are_target_exact() {
        let g17s = g17_notify_started_ops(G17HostBootEvidenceTarget::G17S);
        assert!(validate_g17_notify_started_ops(
            G17HostBootEvidenceTarget::G17S,
            &g17s
        ));
        assert_eq!(
            g17s[5],
            G17xNotifyStartedOp::EncodeStartedMessage {
                prefix: 0x0081_0000_0000_0000,
                token_bits: 44,
            }
        );
        assert_eq!(
            g17s[6],
            G17xNotifyStartedOp::SendRoleMessage {
                endpoint_relative_offset: 0,
                vtable_slot: 0x898,
            }
        );

        let g17g = g17_notify_started_ops(G17HostBootEvidenceTarget::G17G);
        assert!(validate_g17_notify_started_ops(
            G17HostBootEvidenceTarget::G17G,
            &g17g
        ));
        assert_eq!(
            g17g[3],
            G17xNotifyStartedOp::WakeCombinedStarted {
                accelerator_offset: 0x2a8,
                event_source_offset: 0x3e0,
                vtable_slot: 0x208,
                argument: 0,
            }
        );
        assert!(!validate_g17_notify_started_ops(
            G17HostBootEvidenceTarget::G17S,
            &g17g
        ));

        let mut wrong_message = g17s;
        wrong_message[5] = G17xNotifyStartedOp::EncodeStartedMessage {
            prefix: 0x0081_0000_0000_0000,
            token_bits: 43,
        };
        assert!(!validate_g17_notify_started_ops(
            G17HostBootEvidenceTarget::G17S,
            &wrong_message
        ));
    }

    #[test]
    fn g17x_firmware_start_remains_non_executable_at_opaque_boundaries() {
        assert!(GROUNDED_G17X_FIRMWARE_START_MODEL_AVAILABLE);
        assert!(!EXECUTABLE_G17X_FIRMWARE_START_HANDSHAKE_AVAILABLE);
        assert_eq!(
            G17X_FIRMWARE_START_GAPS,
            G17xFirmwareStartGaps {
                load_firmware_mmio_and_power_order_proven: false,
                load_firmware_cache_maintenance_proven: true,
                power_callback_to_endpoint_causality_proven: false,
                interrupt_and_ack_route_proven: false,
                both_cold_boot_flags_to_submission_causality_proven: false,
            }
        );
    }

    #[test]
    fn g17_pre_handoff_is_blocked_but_supported_generations_are_unchanged() {
        assert!(pre_handoff_gate(13).is_ok());
        assert!(pre_handoff_gate(14).is_ok());
        assert!(pre_handoff_gate(15).is_ok());
        assert!(pre_handoff_gate(16).is_ok());
        let missing = pre_handoff_gate(17).unwrap_err();
        // Three pre-handoff axes are now byte-grounded against the on-disk M5
        // firmware and wired: the topology/fault register bank, the init-data
        // object graph the firmware parser accepts, and the GPU RTKit wire
        // transport (with EP0 owned by the version-12-capable apple-rtkit
        // core). Everything else remains unsupported pending more offline RE or a
        // live firmware ACK.
        let grounded =
            missing::TOPOLOGY_REGISTERS | missing::INITDATA_GRAPH | missing::RTKIT_TRANSPORT;
        assert!(AGX3_TOPOLOGY_REGISTERS_IMPLEMENTED);
        for evidence in [
            missing::TOPOLOGY_REGISTERS,
            missing::INITDATA_GRAPH,
            missing::RTKIT_TRANSPORT,
        ] {
            assert!(!missing.contains(evidence));
        }
        assert_eq!(missing.bits(), REQUIRED_PRE_HANDOFF_EVIDENCE & !grounded);
        for evidence in [
            missing::FIRMWARE_IDENTITY,
            missing::ROLE_RESOURCES,
            missing::UAT_HANDOFF,
            missing::GFX1_POWER_SEQUENCE,
            missing::RECOVERY_HANDSHAKE,
            missing::SUBMISSION_TRANSPORT,
        ] {
            assert!(missing.contains(evidence));
        }
        // The boot as a whole is still refused: the missing set is non-empty.
        assert_ne!(missing.bits(), 0);
        assert!(g17_initdata::GROUNDED_LAYOUT_AVAILABLE);
        assert!(g17_initdata::PARSE_ACCEPTED_GRAPH_AVAILABLE);
        assert!(!g17_initdata::ALL_HW_GLOBALS_VALUES_GROUNDED);
        assert!(g17_rtkit::GROUNDED_CODEC_AVAILABLE);
        assert!(g17_rtkit::EXECUTABLE_TRANSPORT_AVAILABLE);
        assert!(GROUNDED_G17X_FIRMWARE_START_MODEL_AVAILABLE);
        assert!(!EXECUTABLE_G17X_FIRMWARE_START_HANDSHAKE_AVAILABLE);
        assert!(agx_power_recovery::GROUNDED_POWER_RECOVERY_MODEL_AVAILABLE);
        assert!(
            agx_power_recovery::validate_g17s_gfx1_sleep_to_21_pio_prefix(
                &agx_power_recovery::G17S_GFX1_SLEEP_TO_21_PIO_PREFIX,
            )
        );
        assert!(
            agx_power_recovery::validate_g17s_gfx1_pre_idle_firmware_ops(
                &agx_power_recovery::G17S_GFX1_PRE_IDLE_FIRMWARE_OPS,
            )
        );
        assert_eq!(
            agx_power_recovery::g17s_pio_mapped_pointer_offset(
                agx_power_recovery::G17PioSlot::PushTelemetryDashboardConfigSpace,
            ),
            0x1c0
        );
        assert!(agx_power_recovery::g17s_gfx1_idle_poll_satisfied(1));
        assert_eq!(
            agx_power_recovery::g17s_gfx1_report_timebase_delta_us(
                agx_power_recovery::G17S_GFX1_IDLE_POLL_TIMEOUT_TICKS,
            ),
            agx_power_recovery::G17S_GFX1_IDLE_POLL_REPORTED_MAX_US
        );
        assert!(!agx_power_recovery::EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE);
        assert!(!agx_power_recovery::EXECUTABLE_G17_RECOVERY_HANDSHAKE_AVAILABLE);
        // ROLE_RESOURCES: the ownership contract is grounded, but there is no
        // firmware parse-time role check to satisfy, so it remains unset.
        assert!(GROUNDED_ROLE_RESOURCE_OWNERSHIP_AVAILABLE);
        assert!(!EXECUTABLE_ROLE_RESOURCES_AVAILABLE);
    }

    #[test]
    fn role_resource_ownership_detects_cross_role_and_null() {
        // Byte-grounded asymmetry: only GFX1 references the +0x481 resource.
        assert_eq!(G17_GFX1_RUNTIME_RESOURCE_REFS_IN_GFX, 0);
        assert_eq!(G17_GFX1_RUNTIME_RESOURCE_REFS_IN_GFX1, 78);
        assert_eq!(G17_ROOT_ROLE_SELECTOR_OFFSET, 0x28);
        assert_eq!(G17_ROOT_HEADER_FW_GLOBAL, 0xffff_fc00_0010_4890);

        // GFX must not own the GFX1-only resource.
        assert_eq!(
            validate_role_resource_ownership(&RoleResourceOwnership {
                role: FirmwareRole::Gfx,
                gfx1_runtime_resource: 0x1000,
            }),
            Err(RoleResourceError::RoleSpecificResourceViolation)
        );
        // GFX1 must have a non-null resource.
        assert_eq!(
            validate_role_resource_ownership(&RoleResourceOwnership {
                role: FirmwareRole::Gfx1,
                gfx1_runtime_resource: 0,
            }),
            Err(RoleResourceError::NullRequiredResource)
        );
        // The correct pairing validates.
        assert!(validate_role_resource_ownership(&RoleResourceOwnership {
            role: FirmwareRole::Gfx,
            gfx1_runtime_resource: 0,
        })
        .is_ok());
        assert!(validate_role_resource_ownership(&RoleResourceOwnership {
            role: FirmwareRole::Gfx1,
            gfx1_runtime_resource: 0x2000,
        })
        .is_ok());
    }

    #[test]
    fn t8140_target_clears_only_shared_uat_and_role_resources() {
        let default = pre_handoff_gate(17).unwrap_err();
        let t8140 = pre_handoff_gate_for_variant(17, G17FirmwareVariant::G17P).unwrap_err();

        assert!(default.contains(missing::ROLE_RESOURCES));
        assert!(default.contains(missing::UAT_HANDOFF));
        assert!(!t8140.contains(missing::ROLE_RESOURCES));
        assert!(!t8140.contains(missing::UAT_HANDOFF));
        assert_eq!(
            t8140.bits(),
            default.bits() & !(missing::ROLE_RESOURCES | missing::UAT_HANDOFF)
        );
        assert!(t8140.contains(missing::FIRMWARE_IDENTITY));
        assert!(t8140.contains(missing::GFX1_POWER_SEQUENCE));
        assert!(t8140.contains(missing::RECOVERY_HANDSHAKE));
        assert!(t8140.contains(missing::SUBMISSION_TRANSPORT));
        assert!(g17_resources::EXECUTABLE_T8140_SHARED_UAT_AVAILABLE);
        assert!(g17_resources::EXECUTABLE_T8140_ROLE_RESOURCES_AVAILABLE);
    }

    #[test]
    fn dual_role_resources_require_one_owner_and_role_correct_sets() {
        let boot = DualRoleBoot::new();
        let good = [
            RoleResourceOwnership {
                role: FirmwareRole::Gfx,
                gfx1_runtime_resource: 0,
            },
            RoleResourceOwnership {
                role: FirmwareRole::Gfx1,
                gfx1_runtime_resource: 0x3000,
            },
        ];
        assert!(validate_dual_role_resources(&boot, &good).is_ok());

        // Swapped roles are invalid.
        let swapped = [good[1], good[0]];
        assert_eq!(
            validate_dual_role_resources(&boot, &swapped),
            Err(RoleResourceError::RoleSpecificResourceViolation)
        );

        // The gate branch never sets ROLE_RESOURCES while it is non-executable.
        let missing = pre_handoff_gate(17).unwrap_err();
        assert!(missing.contains(missing::ROLE_RESOURCES));
    }

    #[test]
    fn submission_transport_waits_for_completion_round_trip() {
        // The producer subset is real (doorbell byte-identical to AGX2, SKSM
        // kick word + record forked and cited), but not executable offline.
        assert!(g17_submission::GROUNDED_SUBMISSION_PRODUCER_AVAILABLE);
        assert!(!g17_submission::EXECUTABLE_SUBMISSION_TRANSPORT_AVAILABLE);
        assert!(!g17_submission::SUBMISSION_LIVE_GAPS.all_proven());
        // The gate branch never sets SUBMISSION_TRANSPORT while it is
        // non-executable; the boot as a whole stays refused.
        let missing = pre_handoff_gate(17).unwrap_err();
        assert!(missing.contains(missing::SUBMISSION_TRANSPORT));
    }

    #[test]
    fn submission_kick_constants_do_not_drift_from_the_rtkit_codec() {
        // Both modules cite the same firmware kick-word/record bytes; assert
        // they agree through the public API so a future edit to one cannot
        // silently diverge. The record table geometry must match
        // g17_rtkit::kick_queue_record_offset exactly.
        for qid in [0u8, 1, 0x7f] {
            assert_eq!(
                g17_submission::kick_record_offset(qid).unwrap(),
                g17_rtkit::kick_queue_record_offset(qid).unwrap()
            );
        }
        assert_eq!(
            g17_submission::kick_record_offset(0x80),
            Err(g17_submission::SubmissionError::QueueIdOutOfRange)
        );
        // The decoded kick word agrees field-for-field with KickWord.
        let word = g17_rtkit::KickWord::encode(0x7f, 0x89ab_cdef, 0x42).unwrap();
        assert_eq!(
            g17_submission::encode_kick_word(0x89_abcd_ef42, 0x7f).unwrap(),
            word
        );
        let via_submission = g17_submission::decode_kick_word(word).unwrap();
        let via_rtkit = g17_rtkit::KickWord::decode_strict(word).unwrap();
        assert_eq!(via_submission.timestamp, via_rtkit.timestamp);
        assert_eq!(via_submission.queue_id, via_rtkit.queue_id);
        assert_eq!(via_submission.mask_selector, via_rtkit.mask_selector);
    }
}
