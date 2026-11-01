// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! AGX3 (G15/G16/G17/G18) SoC identification configuration.
//!
//! Unlike the AGX2 [`super::HwConfig`] instances, these entries carry *only*
//! the identification and UAT-geometry facts recovered from the pinned
//! `26A5378n` kext corpus and the live T6050 machine:
//!
//! - chip id and `apple,agx-*` matching,
//! - the hardware family/variant bytes reported at `0xd04000` and the die
//!   count that disambiguates the C/D split,
//! - the decoded design generation and variant (`readChipInfo` decode table,
//!   `GPU_DRIVER_CONTRACT_G15_G17_2026-08-23.md` section 2.4/2.5),
//! - the UAT geometry: 42-bit input roots (six-bit L1 index) and a 42-bit
//!   output address space (`isPhysicalAddressValid` gate), constant across
//!   every AGX3 part.
//!
//! Everything that is per-firmware-ABI — `HwConfigA`/`HwConfigB` tuning
//! blocks, shared tables, IO mappings, preemption buffer sizes, the
//! firmware `GpuCore` id (renumbered per firmware build) — is deliberately
//! absent. No value here permits a firmware handoff; the probe path fails
//! closed before DMA setup or any ASC start for these targets.

#[cfg(not(test))]
use crate::identity::{GpuGen, GpuVariant};
#[cfg(test)]
#[path = "../identity.rs"]
mod identity;
#[cfg(test)]
use identity::{GpuGen, GpuVariant};

/// Identification-only configuration for one AGX3 SoC.
#[derive(Debug)]
pub(crate) struct SocConfig {
    /// Chip ID in hex format (e.g. 0x6050 for t6050).
    pub(crate) chip_id: u32,
    /// Decoded design generation for this chip.
    pub(crate) gpu_gen: GpuGen,
    /// Decoded variant for this chip.
    pub(crate) gpu_variant: GpuVariant,
    /// Hardware family byte expected at `0xd04000[31:24]`.
    pub(crate) hw_family: u8,
    /// Hardware variant byte expected at `0xd04000[23:16]`.
    pub(crate) hw_variant: u8,
    /// Die count expected at `0xd04010[19:16]`.
    pub(crate) num_dies: u32,
    /// Input-address bits per UAT root (42 on every AGX3 part).
    pub(crate) uat_ias: u8,
    /// Output address space (42-bit on every AGX3 part).
    pub(crate) uat_oas: u32,
}

const fn agx3(
    chip_id: u32,
    gpu_gen: GpuGen,
    gpu_variant: GpuVariant,
    hw_family: u8,
    hw_variant: u8,
    num_dies: u32,
) -> SocConfig {
    SocConfig {
        chip_id,
        gpu_gen,
        gpu_variant,
        hw_family,
        hw_variant,
        num_dies,
        uat_ias: 42,
        uat_oas: 42,
    }
}

// G15 (M3 family). Family byte 0x7; decode read at AGXG15G `readChipInfo`
// 0xfffffe0008af2120/0xfffffe0008af21c0. Variant 4 splits C/D on die count.
/// Apple M3 (G15G).
pub(crate) const T8122: SocConfig = agx3(0x8122, GpuGen::G15, GpuVariant::G, 0x7, 2, 1);
/// Apple M3 Pro (G15S).
pub(crate) const T6030: SocConfig = agx3(0x6030, GpuGen::G15, GpuVariant::S, 0x7, 3, 1);
/// Apple M3 Max (G15C).
pub(crate) const T6031: SocConfig = agx3(0x6031, GpuGen::G15, GpuVariant::C, 0x7, 4, 1);
/// Apple M3 Ultra (G15D, two dies).
pub(crate) const T6032: SocConfig = agx3(0x6032, GpuGen::G15, GpuVariant::D, 0x7, 4, 2);

// G16 (M4 family) and A18 Pro. Family byte 0xa is shared with G17P/G17A;
// the variant byte discriminates. Decode read at AGXG16G
// 0xfffffe0008b5abd8 et seq. The T8140/G17P identity and D93AP topology are
// independently pinned in agx_power_recovery.rs; this entry permits only the
// existing read-only diagnostic probe and never a firmware handoff.
/// Apple A18 Pro (G17P).
pub(crate) const T8140: SocConfig = agx3(0x8140, GpuGen::G17, GpuVariant::P, 0xa, 0, 1);
/// Apple M4 (G16G).
pub(crate) const T8132: SocConfig = agx3(0x8132, GpuGen::G16, GpuVariant::G, 0xa, 2, 1);
/// Apple M4 Pro (G16S).
pub(crate) const T6040: SocConfig = agx3(0x6040, GpuGen::G16, GpuVariant::S, 0xa, 3, 1);
/// Apple M4 Max (G16C).
pub(crate) const T6041: SocConfig = agx3(0x6041, GpuGen::G16, GpuVariant::C, 0xa, 4, 1);

// G17 (M5 family). Family byte 0xb. Decode read at AGXG17X
// 0xfffffe0008b4b0a8/0xfffffe0008b4b524/0xfffffe0008b4b538. T6050 is
// additionally verified live (Mac17,9: gpu_gen=17, gpu_var="S", 2 clusters).
/// Apple M5 (G17G).
pub(crate) const T8142: SocConfig = agx3(0x8142, GpuGen::G17, GpuVariant::G, 0xb, 2, 1);
/// Apple M5 Pro (G17S).
pub(crate) const T6050: SocConfig = agx3(0x6050, GpuGen::G17, GpuVariant::S, 0xb, 3, 1);
/// Apple M5 Max (G17C).
pub(crate) const T6051: SocConfig = agx3(0x6051, GpuGen::G17, GpuVariant::C, 0xb, 4, 1);

// G18 (M6 family). Family byte 0xc, variant 2: 26A428
// `AGX·PI_400·G·A0·Accelerator::readChipInfo` @ 0xfffffe0008c3d3a0 compares
// the family byte with 0xc @ 0xfffffe0008c3d49c and gives variant 2 the
// 12-core G configuration @ 0xfffffe0008c3d4bc..0x4c8. Native J873 registry
// (AGXAcceleratorG18G): gpu_gen=18, gpu_var=G, usc_gen=3, is_sksm=1,
// num_cores=12, num_gps=6, num_mgpus=2, core_mask_list=(63,63),
// num_frags=12, kickid_qid_shift=40, kickid_qid_mask=127. The live ID
// registers have not been read yet (the GPU PSD reads idle at cold boot), so
// the T8152 probe identifies the part by chip id and compatible, and only
// logs a mismatching register decode. The ADT uat-vaddr-size is 43 on both
// J813 and J873: 42 bits per root plus the TTBR-select bit, as on T8142.
/// Apple M6 (G18G, J873 Mac mini).
pub(crate) const T8152: SocConfig = agx3(0x8152, GpuGen::G18, GpuVariant::G, 0xc, 2, 1);

/// Native J873 `AGXAcceleratorG18G` registry facts (macOS 26A428 ioreg).
///
/// These are identity facts, not firmware tuning: they size the core-mask
/// decode and cross-check the live ID registers once the GPU is powered.
#[allow(dead_code)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G18RegistryFacts {
    pub(crate) num_cores: u32,
    pub(crate) num_gps: u32,
    pub(crate) num_mgpus: u32,
    /// Per-mGPU (cluster) core masks, `core_mask_list`.
    pub(crate) core_mask_list: [u32; 2],
    pub(crate) num_frags: u32,
    pub(crate) usc_generation: u32,
    pub(crate) kickid_qid_shift: u32,
    pub(crate) kickid_qid_mask: u32,
}

/// J873 registry facts (PROVEN from the native ioreg).
#[allow(dead_code)]
pub(crate) const T8152_REGISTRY: G18RegistryFacts = G18RegistryFacts {
    num_cores: 12,
    num_gps: 6,
    num_mgpus: 2,
    core_mask_list: [0x3f, 0x3f],
    num_frags: 12,
    usc_generation: 3,
    kickid_qid_shift: 40,
    kickid_qid_mask: 127,
};

// t6034 and t6052 appear in the AGXG15C/AGXG17X kext match lists but map to
// no distinct core identity in the readChipInfo decode; what silicon they
// name is unknown, so they deliberately have no entry here.

// -------------------------------------------------------------------------
// T8140 HwConfig grounding ledger.
//
// The legacy `super::HwConfig` carries ~40 fields. Only the identity and
// geometry axes below are grounded for T8140 (from `SocConfig`, the live
// J700 registers, and the G17P init-data model). The rest are AGX2
// firmware-ABI power/tuning constants with no T8140 derivation, so no full
// `HwConfig` can be constructed without inventing them. This ledger names
// exactly which field groups are missing and lets the probe path fail
// closed with a precise reason instead of fabricating a config.
// -------------------------------------------------------------------------

/// The grounded subset of a T8140 `HwConfig`, assembled from evidence. The
/// firmware `GpuCore` id is renumbered per firmware build and not recovered
/// for T8140, so it is deliberately `None` and every consumer fails closed.
///
/// Evidence ledger: its fields are read by the (not-yet-wired) full T8140
/// `HwConfig` builder, so they are allowed to be unread for now.
#[cfg(not(test))]
#[allow(dead_code)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct T8140GroundedHwConfig {
    pub(crate) chip_id: u32,
    /// Base timekeeping clock: the 24 MHz `clkref`, verified in the J700 DT.
    pub(crate) base_clock_hz: u32,
    /// 42-bit UAT output address space.
    pub(crate) uat_oas: u32,
    /// 42-bit UAT input address space per root.
    pub(crate) uat_ias: u8,
    /// Single die (live `0xd04010` read = `0x00110106`).
    pub(crate) num_dies: u32,
    /// Firmware `GpuCore` id, always `None` for T8140 (see above).
    pub(crate) gpu_core: Option<super::GpuCore>,
}

/// The exact grounded T8140 HwConfig subset.
#[cfg(not(test))]
pub(crate) const T8140_GROUNDED_HWCONFIG: T8140GroundedHwConfig = T8140GroundedHwConfig {
    chip_id: 0x8140,
    base_clock_hz: 24_000_000,
    uat_oas: 42,
    uat_ias: 42,
    num_dies: 1,
    gpu_core: None,
};

/// The `HwConfig` field groups that are ungrounded for T8140.
pub(crate) mod hwconfig_gap {
    /// Firmware `GpuCore` id (per-firmware-build ABI; not recovered).
    pub(crate) const GPU_CORE_ID: u16 = 1 << 0;
    /// Preemption buffer sizes (`preempt{1,2,3}_size`, `compute_preempt1`).
    pub(crate) const PREEMPT_SIZES: u16 = 1 << 1;
    /// `HwConfigA`/`HwConfigB` unknown tuning words.
    pub(crate) const HWCONFIG_A_B: u16 = 1 << 2;
    /// Shared power curve tables (`shared1/2/3`, `unk_coef_a/b`, `sram_k`).
    pub(crate) const POWER_CURVES: u16 = 1 << 3;
    /// Thermal sensor masks (`fast_sensor_mask*`, `fast_die0_sensor_present`).
    pub(crate) const SENSOR_MASKS: u16 = 1 << 4;
    /// Firmware MMIO mapping table (`io_mappings`) and SRAM window.
    pub(crate) const IO_MAPPINGS: u16 = 1 << 5;
    /// Clustering metadata block sizes (`HwClusteringConfig`).
    pub(crate) const CLUSTERING: u16 = 1 << 6;
    /// Global misc fields (`idle_off_standby_timer_default`, `unk_hws2_*`,
    /// `global_unk_54`, `global_tab`, `has_csafr`, render tiling control).
    pub(crate) const GLOBAL_MISC: u16 = 1 << 7;
}

/// Every `HwConfig` field group that T8140 does not yet ground.
pub(crate) const T8140_HWCONFIG_MISSING: u16 = hwconfig_gap::GPU_CORE_ID
    | hwconfig_gap::PREEMPT_SIZES
    | hwconfig_gap::HWCONFIG_A_B
    | hwconfig_gap::POWER_CURVES
    | hwconfig_gap::SENSOR_MASKS
    | hwconfig_gap::IO_MAPPINGS
    | hwconfig_gap::CLUSTERING
    | hwconfig_gap::GLOBAL_MISC;

/// Opaque fail-closed result of a T8140 HwConfig construction attempt.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct HwConfigGap(u16);

impl HwConfigGap {
    pub(crate) const fn bits(self) -> u16 {
        self.0
    }

    /// Whether a specific field group is among the missing ones. Used by the
    /// gap tests and the future per-group grounding path.
    #[allow(dead_code)]
    pub(crate) const fn contains(self, group: u16) -> bool {
        self.0 & group != 0
    }
}

/// Attempt to construct a full T8140 `HwConfig`.
///
/// This assembles the grounded subset ([`T8140_GROUNDED_HWCONFIG`]) and then
/// **fails closed**: the ~32 remaining AGX2 firmware-ABI power/tuning fields
/// have no T8140 derivation, so returning a complete `HwConfig` would require
/// inventing them. The returned gap names exactly which field groups are
/// missing, so the probe path can report a precise wall rather than a bare
/// error. When those groups are eventually grounded, this becomes the place
/// that returns `Ok(&'static HwConfig)`.
#[cfg(not(test))]
pub(crate) fn t8140_hwconfig() -> Result<&'static super::HwConfig, HwConfigGap> {
    // Grounded fields exist ([`T8140_GROUNDED_HWCONFIG`]); the rest do not.
    let _grounded = &T8140_GROUNDED_HWCONFIG;
    Err(HwConfigGap(T8140_HWCONFIG_MISSING))
}

#[cfg(test)]
mod tests {
    use super::identity::{
        decode_gpu_identity, FirmwareRoleTopology, GpuGen, GpuHalGeneration, SubmissionTransport,
    };
    use super::*;

    const ALL: [&SocConfig; 12] = [
        &T8122, &T6030, &T6031, &T6032, &T8140, &T8132, &T6040, &T6041, &T8142, &T6050, &T6051,
        &T8152,
    ];

    #[test]
    fn every_soc_decodes_to_its_stated_identity() {
        for soc in ALL {
            let identity = decode_gpu_identity(soc.hw_family, soc.hw_variant, soc.num_dies as u8)
                .expect("static AGX3 identity must decode");
            assert_eq!(identity.gpu_gen, soc.gpu_gen, "chip {:#x}", soc.chip_id);
            assert_eq!(
                identity.gpu_variant, soc.gpu_variant,
                "chip {:#x}",
                soc.chip_id
            );
            // Every Mac AGX3 part is USC generation 3 with 42-bit UAT roots.
            assert_eq!(identity.usc_generation, 3, "chip {:#x}", soc.chip_id);
            assert_eq!(
                identity.uat_input_address_bits, soc.uat_ias,
                "chip {:#x}",
                soc.chip_id
            );
            assert_eq!(soc.uat_ias, 42);
            assert_eq!(soc.uat_oas, 42);
        }
    }

    #[test]
    fn firmware_role_topology_follows_the_generation() {
        for soc in ALL {
            let identity =
                decode_gpu_identity(soc.hw_family, soc.hw_variant, soc.num_dies as u8).unwrap();
            let expected = if matches!(soc.gpu_gen, GpuGen::G17 | GpuGen::G18) {
                FirmwareRoleTopology::Dual
            } else {
                FirmwareRoleTopology::Single
            };
            assert_eq!(identity.firmware_roles, expected, "chip {:#x}", soc.chip_id);
        }
    }

    #[test]
    fn hal300_parts_are_sksm_only() {
        for soc in ALL {
            let identity =
                decode_gpu_identity(soc.hw_family, soc.hw_variant, soc.num_dies as u8).unwrap();
            if identity.gpu_hal_generation == GpuHalGeneration::Hal300 {
                assert_eq!(
                    identity.submission_transport,
                    SubmissionTransport::Sksm,
                    "chip {:#x}",
                    soc.chip_id
                );
            }
        }
    }

    #[test]
    fn chip_ids_are_unique_and_match_their_family_tier() {
        for (i, a) in ALL.iter().enumerate() {
            for b in ALL.iter().skip(i + 1) {
                assert_ne!(a.chip_id, b.chip_id);
            }
            // Mac 0x8xxx chips are single-cluster consumer parts (G variant),
            // except mobile T8140 whose independently decoded tier is P.
            // 0x6xxx chips are the S/C/D tiers.
            if a.chip_id == 0x8140 {
                assert_eq!(a.gpu_variant, GpuVariant::P);
            } else if a.chip_id & 0xf000 == 0x8000 {
                assert_eq!(a.gpu_variant, GpuVariant::G);
            } else {
                assert_ne!(a.gpu_variant, GpuVariant::G);
            }
        }
    }

    #[test]
    fn t8140_is_the_exact_a18_pro_g17p_diagnostic_identity() {
        assert_eq!(T8140.chip_id, 0x8140);
        assert_eq!(T8140.hw_family, 0xa);
        assert_eq!(T8140.hw_variant, 0);
        assert_eq!(T8140.num_dies, 1);
        let identity =
            decode_gpu_identity(T8140.hw_family, T8140.hw_variant, T8140.num_dies as u8).unwrap();
        assert_eq!(identity.gpu_gen, GpuGen::G17);
        assert_eq!(identity.gpu_variant, GpuVariant::P);
    }

    #[test]
    fn t8152_is_the_m6_g18g_identity_and_matches_the_registry() {
        use super::identity::GpuHalGeneration;
        assert_eq!(T8152.chip_id, 0x8152);
        assert_eq!((T8152.hw_family, T8152.hw_variant, T8152.num_dies), (0xc, 2, 1));
        let identity =
            decode_gpu_identity(T8152.hw_family, T8152.hw_variant, T8152.num_dies as u8).unwrap();
        assert_eq!(identity.gpu_gen, GpuGen::G18);
        assert_eq!(identity.gpu_variant, GpuVariant::G);
        assert_eq!(identity.gpu_hal_generation, GpuHalGeneration::Hal400);
        assert_eq!(identity.submission_transport, SubmissionTransport::Sksm);
        assert_eq!(identity.usc_generation, T8152_REGISTRY.usc_generation);
        let total: u32 = T8152_REGISTRY
            .core_mask_list
            .iter()
            .map(|mask| mask.count_ones())
            .sum();
        assert_eq!(total, T8152_REGISTRY.num_cores);
        assert_eq!(T8152_REGISTRY.num_mgpus as usize, T8152_REGISTRY.core_mask_list.len());
        assert_eq!(T8152_REGISTRY.num_frags, T8152_REGISTRY.num_cores);
        assert_eq!((T8152_REGISTRY.kickid_qid_shift, T8152_REGISTRY.kickid_qid_mask), (40, 127));
        assert_eq!((T8152.uat_ias, T8152.uat_oas), (42, 42));
    }

    #[test]
    fn t8140_hwconfig_gap_enumerates_every_ungrounded_field_group() {
        // The gap must name every ungrounded field group and nothing else.
        let gap = HwConfigGap(T8140_HWCONFIG_MISSING);
        for group in [
            hwconfig_gap::GPU_CORE_ID,
            hwconfig_gap::PREEMPT_SIZES,
            hwconfig_gap::HWCONFIG_A_B,
            hwconfig_gap::POWER_CURVES,
            hwconfig_gap::SENSOR_MASKS,
            hwconfig_gap::IO_MAPPINGS,
            hwconfig_gap::CLUSTERING,
            hwconfig_gap::GLOBAL_MISC,
        ] {
            assert!(gap.contains(group));
        }
        // Eight distinct single-bit groups.
        assert_eq!(T8140_HWCONFIG_MISSING.count_ones(), 8);
        assert_eq!(gap.bits(), T8140_HWCONFIG_MISSING);
    }
}
