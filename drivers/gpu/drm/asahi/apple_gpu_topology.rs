// SPDX-License-Identifier: GPL-2.0-only OR MIT

#![allow(dead_code)] // Evidence ledger: runtime consumers remain deliberately gated.

//! Exact, non-executable Apple DeviceTree topology for scoped M3-M5 GPUs.
//!
//! The constants below come from the signed macOS build 25G83 trees in
//! `Firmware/all_flash`. Every 25G83 board with the same `gpu,t*` compatible
//! has the same SGX/ASC/PMGR projection modeled here. This module deliberately
//! describes resources only: it does not authorize power writes, submission,
//! or interrupt acknowledgement.

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum AppleGpuTarget {
    T8122G15G,
    T6030G15S,
    T6031G15C,
    T6034G15D,
    T8132G16G,
    T6040G16S,
    T6041G16C,
    T8142G17G,
    T6050G17S,
    T6050G17C,
}

pub(crate) const APPLE_GPU_TARGETS: [AppleGpuTarget; 10] = [
    AppleGpuTarget::T8122G15G,
    AppleGpuTarget::T6030G15S,
    AppleGpuTarget::T6031G15C,
    AppleGpuTarget::T6034G15D,
    AppleGpuTarget::T8132G16G,
    AppleGpuTarget::T6040G16S,
    AppleGpuTarget::T6041G16C,
    AppleGpuTarget::T8142G17G,
    AppleGpuTarget::T6050G17S,
    AppleGpuTarget::T6050G17C,
];

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct AppleGpuTopologyEvidence {
    pub(crate) board: &'static str,
    pub(crate) im4p_sha256: &'static str,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct AppleGpuRegisterRange {
    pub(crate) phys_base: u64,
    pub(crate) size: u32,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum AppleGpuRawInterrupts {
    G15([u32; 7]),
    G16Plus([u32; 8]),
}

impl AppleGpuRawInterrupts {
    pub(crate) const fn len(self) -> usize {
        match self {
            Self::G15(_) => 7,
            Self::G16Plus(_) => 8,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum AppleGpuAscRole {
    Gfx,
    Gfx1,
}

/// Both ASC nodes are `iop,ascwrap-v6` with `iop-version = 1`.
///
/// The ranges retain DeviceTree `reg` ordering. `wrapper_regs` is slot zero;
/// `iorvbar_regs` is slot one, whose size is platform-specific and therefore
/// must not be inferred from newer eight-byte instances.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct AppleGpuAscProvider {
    pub(crate) role: AppleGpuAscRole,
    pub(crate) compatible: &'static str,
    pub(crate) iop_version: u8,
    pub(crate) wrapper_regs: AppleGpuRegisterRange,
    pub(crate) iorvbar_regs: AppleGpuRegisterRange,
    pub(crate) interrupts: [u32; 4],
    pub(crate) clock_gate_id: u32,
    pub(crate) power_gate_id: u32,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum AppleGpuPmgrGateRole {
    GfxSgx,
    GfxBusy,
    GfxAsc,
    GfxAsc1,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct AppleGpuPmgrGate {
    pub(crate) role: AppleGpuPmgrGateRole,
    pub(crate) id: u32,
    pub(crate) name: &'static str,
    pub(crate) flags: u8,
    pub(crate) index: u8,
    pub(crate) alias: Option<u32>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum AppleGpuPmgrGates {
    SingleAsc([AppleGpuPmgrGate; 3]),
    DualAsc([AppleGpuPmgrGate; 4]),
}

impl AppleGpuPmgrGates {
    pub(crate) fn as_slice(&self) -> &[AppleGpuPmgrGate] {
        match self {
            Self::SingleAsc(gates) => gates,
            Self::DualAsc(gates) => gates,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct AppleGpuStaticTopology {
    pub(crate) compatible: &'static str,
    pub(crate) sgx_regs: [AppleGpuRegisterRange; 2],
    pub(crate) sgx_clock_gate_ids: [u32; 2],
    pub(crate) sgx_power_gate_ids: [u32; 2],
    pub(crate) clock_id: Option<u32>,
    pub(crate) sgx_interrupts: AppleGpuRawInterrupts,
    pub(crate) sgx_interrupts_valid: u8,
    pub(crate) gfx_asc: AppleGpuAscProvider,
    pub(crate) gfx1_asc: Option<AppleGpuAscProvider>,
    pub(crate) pmgr_gates: AppleGpuPmgrGates,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct PinnedAppleGpuTopology {
    pub(crate) target: AppleGpuTarget,
    pub(crate) evidence: AppleGpuTopologyEvidence,
    pub(crate) topology: AppleGpuStaticTopology,
}

const fn regs(phys_base: u64, size: u32) -> AppleGpuRegisterRange {
    AppleGpuRegisterRange { phys_base, size }
}

const fn asc(
    role: AppleGpuAscRole,
    wrapper_regs: AppleGpuRegisterRange,
    iorvbar_regs: AppleGpuRegisterRange,
    interrupts: [u32; 4],
    gate_id: u32,
) -> AppleGpuAscProvider {
    AppleGpuAscProvider {
        role,
        compatible: "iop,ascwrap-v6",
        iop_version: 1,
        wrapper_regs,
        iorvbar_regs,
        interrupts,
        clock_gate_id: gate_id,
        power_gate_id: gate_id,
    }
}

const fn gate(
    role: AppleGpuPmgrGateRole,
    id: u32,
    name: &'static str,
    alias: Option<u32>,
) -> AppleGpuPmgrGate {
    AppleGpuPmgrGate {
        role,
        id,
        name,
        flags: 16,
        index: 0,
        alias,
    }
}

pub(crate) const T8122_G15G: PinnedAppleGpuTopology = PinnedAppleGpuTopology {
    target: AppleGpuTarget::T8122G15G,
    evidence: AppleGpuTopologyEvidence {
        board: "j433ap",
        im4p_sha256: "a1d6bf04214c262341e8715d45688ff2e666edf53bf847534fb5143724c1e34f",
    },
    topology: AppleGpuStaticTopology {
        compatible: "gpu,t8122",
        sgx_regs: [
            regs(0x8000_0000, 0x0400_0000),
            regs(0x80d0_0000, 0x0012_c000),
        ],
        sgx_clock_gate_ids: [357, 358],
        sgx_power_gate_ids: [357, 358],
        clock_id: Some(358),
        sgx_interrupts: AppleGpuRawInterrupts::G15([706, 707, 708, 709, 727, 729, 721]),
        sgx_interrupts_valid: 0x5f,
        gfx_asc: asc(
            AppleGpuAscRole::Gfx,
            regs(0x8240_0000, 0x0006_c000),
            regs(0x8205_0000, 8),
            [724, 723, 726, 725],
            359,
        ),
        gfx1_asc: None,
        pmgr_gates: AppleGpuPmgrGates::SingleAsc([
            gate(AppleGpuPmgrGateRole::GfxSgx, 357, "GFX-SGX", Some(44)),
            gate(AppleGpuPmgrGateRole::GfxBusy, 358, "GFX-BUSY", None),
            gate(AppleGpuPmgrGateRole::GfxAsc, 359, "GFX-ASC", Some(44)),
        ]),
    },
};

pub(crate) const T6030_G15S: PinnedAppleGpuTopology = PinnedAppleGpuTopology {
    target: AppleGpuTarget::T6030G15S,
    evidence: AppleGpuTopologyEvidence {
        board: "j514sap",
        im4p_sha256: "6e45d2bc6d8dcf75099b179c8366d0871f634f016e55ebc5d2bd4474f36a4eef",
    },
    topology: AppleGpuStaticTopology {
        compatible: "gpu,t6030",
        sgx_regs: [
            regs(0x8000_0000, 0x032a_4000),
            regs(0x80d0_0000, 0x0016_c000),
        ],
        sgx_clock_gate_ids: [339, 340],
        sgx_power_gate_ids: [339, 340],
        clock_id: None,
        sgx_interrupts: AppleGpuRawInterrupts::G15([815, 816, 817, 818, 836, 838, 830]),
        sgx_interrupts_valid: 0x5f,
        gfx_asc: asc(
            AppleGpuAscRole::Gfx,
            regs(0x8240_0000, 0x0006_c000),
            regs(0x8205_0000, 0x0006_c000),
            [833, 832, 835, 834],
            341,
        ),
        gfx1_asc: None,
        pmgr_gates: AppleGpuPmgrGates::SingleAsc([
            gate(AppleGpuPmgrGateRole::GfxSgx, 339, "GFX-SGX", Some(226)),
            gate(AppleGpuPmgrGateRole::GfxBusy, 340, "GFX-BUSY", None),
            gate(AppleGpuPmgrGateRole::GfxAsc, 341, "GFX-ASC", Some(226)),
        ]),
    },
};

pub(crate) const T6031_G15C: PinnedAppleGpuTopology = PinnedAppleGpuTopology {
    target: AppleGpuTarget::T6031G15C,
    evidence: AppleGpuTopologyEvidence {
        board: "j514cap",
        im4p_sha256: "a17481ac3ca9e6820a69646def55b2e0b19fa7e95582df15a8b8efac7893d401",
    },
    topology: AppleGpuStaticTopology {
        compatible: "gpu,t6031",
        sgx_regs: [
            regs(0x2_0800_0000, 0x03fd_c000),
            regs(0x2_08d0_0000, 0x0016_c000),
        ],
        sgx_clock_gate_ids: [381, 383],
        sgx_power_gate_ids: [381, 383],
        clock_id: Some(315),
        sgx_interrupts: AppleGpuRawInterrupts::G15([1224, 1225, 1226, 1227, 1247, 1249, 1239]),
        sgx_interrupts_valid: 0x5f,
        gfx_asc: asc(
            AppleGpuAscRole::Gfx,
            regs(0x2_0a40_0000, 0x0006_c000),
            regs(0x2_0a05_0000, 0x0006_0000),
            [1244, 1243, 1246, 1245],
            382,
        ),
        gfx1_asc: None,
        pmgr_gates: AppleGpuPmgrGates::SingleAsc([
            gate(AppleGpuPmgrGateRole::GfxSgx, 381, "GFX-SGX", Some(367)),
            gate(AppleGpuPmgrGateRole::GfxBusy, 383, "GFX-BUSY", None),
            gate(AppleGpuPmgrGateRole::GfxAsc, 382, "GFX-ASC", Some(367)),
        ]),
    },
};

pub(crate) const T6034_G15D: PinnedAppleGpuTopology = PinnedAppleGpuTopology {
    target: AppleGpuTarget::T6034G15D,
    evidence: AppleGpuTopologyEvidence {
        board: "j514map",
        im4p_sha256: "4a978c446af8624eb2c67dbdcd7b7a6a894f8fe87fec33ce67e5ae35eadcc67c",
    },
    topology: AppleGpuStaticTopology {
        compatible: "gpu,t6034",
        ..T6031_G15C.topology
    },
};

pub(crate) const T8132_G16G: PinnedAppleGpuTopology = PinnedAppleGpuTopology {
    target: AppleGpuTarget::T8132G16G,
    evidence: AppleGpuTopologyEvidence {
        board: "j604ap",
        im4p_sha256: "5fd70c98a4949c4259d8385c2b734d3df4e4c821da8edc187063ec965920b384",
    },
    topology: AppleGpuStaticTopology {
        compatible: "gpu,t8132",
        sgx_regs: [
            regs(0x1_0000_0000, 0x0400_0000),
            regs(0x1_00d0_0000, 0x0018_0000),
        ],
        sgx_clock_gate_ids: [317, 316],
        sgx_power_gate_ids: [317, 316],
        clock_id: Some(381),
        sgx_interrupts: AppleGpuRawInterrupts::G16Plus([975, 976, 977, 978, 997, 999, 990, 992]),
        sgx_interrupts_valid: 0xdf,
        gfx_asc: asc(
            AppleGpuAscRole::Gfx,
            regs(0x1_0260_0000, 0x0008_8000),
            regs(0x1_0205_0000, 8),
            [994, 993, 996, 995],
            315,
        ),
        gfx1_asc: None,
        pmgr_gates: AppleGpuPmgrGates::SingleAsc([
            gate(AppleGpuPmgrGateRole::GfxSgx, 317, "GFX-SGX", Some(55)),
            gate(AppleGpuPmgrGateRole::GfxBusy, 316, "GFX-BUSY", None),
            gate(AppleGpuPmgrGateRole::GfxAsc, 315, "GFX-ASC", Some(55)),
        ]),
    },
};

pub(crate) const T6040_G16S: PinnedAppleGpuTopology = PinnedAppleGpuTopology {
    target: AppleGpuTarget::T6040G16S,
    evidence: AppleGpuTopologyEvidence {
        board: "j614sap",
        im4p_sha256: "7c304ceb808200c7de3d5c76eafad2a54dcdfff2748b874547ddf2744b5d5ff4",
    },
    topology: AppleGpuStaticTopology {
        compatible: "gpu,t6040",
        sgx_regs: [
            regs(0x8800_0000, 0x0375_8000),
            regs(0x88d0_0000, 0x0016_c000),
        ],
        sgx_clock_gate_ids: [400, 402],
        sgx_power_gate_ids: [400, 402],
        clock_id: Some(315),
        sgx_interrupts: AppleGpuRawInterrupts::G16Plus([
            1481, 1482, 1483, 1484, 1505, 1507, 1496, 1498,
        ]),
        sgx_interrupts_valid: 0xdf,
        gfx_asc: asc(
            AppleGpuAscRole::Gfx,
            regs(0x8a60_0000, 0x0008_8000),
            regs(0x8a05_0000, 0x0006_0000),
            [1502, 1501, 1504, 1503],
            401,
        ),
        gfx1_asc: None,
        pmgr_gates: AppleGpuPmgrGates::SingleAsc([
            gate(AppleGpuPmgrGateRole::GfxSgx, 400, "GFX-SGX", Some(386)),
            gate(AppleGpuPmgrGateRole::GfxBusy, 402, "GFX-BUSY", None),
            gate(AppleGpuPmgrGateRole::GfxAsc, 401, "GFX-ASC", Some(386)),
        ]),
    },
};

pub(crate) const T6041_G16C: PinnedAppleGpuTopology = PinnedAppleGpuTopology {
    target: AppleGpuTarget::T6041G16C,
    evidence: AppleGpuTopologyEvidence {
        board: "j575cap",
        im4p_sha256: "23ea93889995174937e7475e345eedcadbeddbdb3eb7081bacfe6068f5733ea2",
    },
    topology: AppleGpuStaticTopology {
        compatible: "gpu,t6041",
        ..T6040_G16S.topology
    },
};

pub(crate) const T8142_G17G: PinnedAppleGpuTopology = PinnedAppleGpuTopology {
    target: AppleGpuTarget::T8142G17G,
    evidence: AppleGpuTopologyEvidence {
        board: "j813ap",
        im4p_sha256: "3bb7729352c1157738259c6705246eed631752077240225eeae0eebf902b1ada",
    },
    topology: AppleGpuStaticTopology {
        compatible: "gpu,t8142",
        sgx_regs: [
            regs(0x3_7000_0000, 0x0400_0000),
            regs(0x3_70d0_0000, 0x0018_0000),
        ],
        sgx_clock_gate_ids: [515, 516],
        sgx_power_gate_ids: [515, 516],
        clock_id: Some(381),
        sgx_interrupts: AppleGpuRawInterrupts::G16Plus([
            1104, 1105, 1106, 1107, 1131, 1133, 1119, 1121,
        ]),
        sgx_interrupts_valid: 0xdf,
        gfx_asc: asc(
            AppleGpuAscRole::Gfx,
            regs(0x3_7260_0000, 0x0008_8000),
            regs(0x3_7205_0000, 8),
            [1124, 1123, 1126, 1125],
            517,
        ),
        gfx1_asc: Some(asc(
            AppleGpuAscRole::Gfx1,
            regs(0x3_72e0_0000, 0x0008_8000),
            regs(0x3_7285_0000, 8),
            [1128, 1127, 1130, 1129],
            518,
        )),
        pmgr_gates: AppleGpuPmgrGates::DualAsc([
            gate(AppleGpuPmgrGateRole::GfxSgx, 515, "GFX-SGX", Some(514)),
            gate(AppleGpuPmgrGateRole::GfxBusy, 516, "GFX-BUSY", None),
            gate(AppleGpuPmgrGateRole::GfxAsc, 517, "GFX-ASC", Some(514)),
            gate(AppleGpuPmgrGateRole::GfxAsc1, 518, "GFX-ASC1", Some(514)),
        ]),
    },
};

pub(crate) const T6050_G17S: PinnedAppleGpuTopology = PinnedAppleGpuTopology {
    target: AppleGpuTarget::T6050G17S,
    evidence: AppleGpuTopologyEvidence {
        board: "j714sap",
        im4p_sha256: "2fe1da5d71964ad981d8b5daf46cb9d9a1603a36f4db56dd18ca587b6f8b1837",
    },
    topology: AppleGpuStaticTopology {
        compatible: "gpu,t6050",
        sgx_regs: [
            regs(0x21_0000_0000, 0x03fd_c000),
            regs(0x21_00d0_0000, 0x0017_7000),
        ],
        sgx_clock_gate_ids: [616, 615],
        sgx_power_gate_ids: [616, 615],
        clock_id: None,
        sgx_interrupts: AppleGpuRawInterrupts::G16Plus([
            2448, 2449, 2450, 2451, 450, 2806, 2463, 2465,
        ]),
        sgx_interrupts_valid: 0xdf,
        gfx_asc: asc(
            AppleGpuAscRole::Gfx,
            regs(0x21_0260_0000, 0x0008_8000),
            regs(0x21_0205_0000, 8),
            [2798, 2797, 2800, 2799],
            614,
        ),
        gfx1_asc: Some(asc(
            AppleGpuAscRole::Gfx1,
            regs(0x21_02e0_0000, 0x0008_8000),
            regs(0x21_0285_0000, 0x0006_0000),
            [2802, 2801, 2804, 2803],
            657,
        )),
        pmgr_gates: AppleGpuPmgrGates::DualAsc([
            gate(AppleGpuPmgrGateRole::GfxSgx, 616, "GFX_SGX", Some(362)),
            gate(AppleGpuPmgrGateRole::GfxBusy, 615, "GFX_BUSY", None),
            gate(AppleGpuPmgrGateRole::GfxAsc, 614, "GFX_ASC", Some(362)),
            gate(AppleGpuPmgrGateRole::GfxAsc1, 657, "GFX_ASC1", Some(362)),
        ]),
    },
};

pub(crate) const T6050_G17C: PinnedAppleGpuTopology = PinnedAppleGpuTopology {
    target: AppleGpuTarget::T6050G17C,
    evidence: AppleGpuTopologyEvidence {
        board: "j714cap",
        im4p_sha256: "497a18d20c91c1865abaa01c9144d5427de2d1a487195f6c122e2a6bd8c2e1b5",
    },
    topology: T6050_G17S.topology,
};

pub(crate) const fn pinned_topology(target: AppleGpuTarget) -> &'static PinnedAppleGpuTopology {
    match target {
        AppleGpuTarget::T8122G15G => &T8122_G15G,
        AppleGpuTarget::T6030G15S => &T6030_G15S,
        AppleGpuTarget::T6031G15C => &T6031_G15C,
        AppleGpuTarget::T6034G15D => &T6034_G15D,
        AppleGpuTarget::T8132G16G => &T8132_G16G,
        AppleGpuTarget::T6040G16S => &T6040_G16S,
        AppleGpuTarget::T6041G16C => &T6041_G16C,
        AppleGpuTarget::T8142G17G => &T8142_G17G,
        AppleGpuTarget::T6050G17S => &T6050_G17S,
        AppleGpuTarget::T6050G17C => &T6050_G17C,
    }
}

/// Reject any partial, cross-target, or mutated static topology.
pub(crate) fn validate_static_topology(
    target: AppleGpuTarget,
    observed: &AppleGpuStaticTopology,
) -> bool {
    observed == &pinned_topology(target).topology
}

/// Static resource extraction is complete; runtime ownership remains unknown.
pub(crate) const STATIC_TOPOLOGY_PROVEN: bool = true;
pub(crate) const LIVE_POWER_SEQUENCE_PROVEN: bool = false;
pub(crate) const LIVE_SUBMISSION_PROVEN: bool = false;

#[cfg(test)]
mod tests {
    use super::*;

    fn has_gate(topology: &AppleGpuStaticTopology, role: AppleGpuPmgrGateRole, id: u32) -> bool {
        topology
            .pmgr_gates
            .as_slice()
            .iter()
            .any(|gate| gate.role == role && gate.id == id)
    }

    #[test]
    fn every_target_is_exact_and_fails_closed_on_each_resource_axis() {
        for target in APPLE_GPU_TARGETS {
            let exact = pinned_topology(target);
            assert_eq!(exact.target, target);
            assert!(validate_static_topology(target, &exact.topology));
            assert_eq!(exact.evidence.im4p_sha256.len(), 64);

            let mut wrong = exact.topology;
            wrong.sgx_regs[0].phys_base ^= 0x4000;
            assert!(!validate_static_topology(target, &wrong));

            let mut wrong = exact.topology;
            wrong.sgx_interrupts = match wrong.sgx_interrupts {
                AppleGpuRawInterrupts::G15(mut irqs) => {
                    irqs[0] ^= 1;
                    AppleGpuRawInterrupts::G15(irqs)
                }
                AppleGpuRawInterrupts::G16Plus(mut irqs) => {
                    irqs[0] ^= 1;
                    AppleGpuRawInterrupts::G16Plus(irqs)
                }
            };
            assert!(!validate_static_topology(target, &wrong));

            let mut wrong = exact.topology;
            wrong.sgx_interrupts_valid ^= 1 << 5;
            assert!(!validate_static_topology(target, &wrong));

            let mut wrong = exact.topology;
            wrong.gfx_asc.interrupts.swap(0, 1);
            assert!(!validate_static_topology(target, &wrong));

            let mut wrong = exact.topology;
            wrong.gfx_asc.wrapper_regs.size ^= 0x4000;
            assert!(!validate_static_topology(target, &wrong));

            let mut wrong = exact.topology;
            wrong.sgx_clock_gate_ids.swap(0, 1);
            assert!(!validate_static_topology(target, &wrong));

            let mut wrong = exact.topology;
            wrong.gfx_asc.power_gate_id ^= 1;
            assert!(!validate_static_topology(target, &wrong));

            let mut wrong = exact.topology;
            wrong.pmgr_gates = match wrong.pmgr_gates {
                AppleGpuPmgrGates::SingleAsc(mut gates) => {
                    gates[0].id ^= 1;
                    AppleGpuPmgrGates::SingleAsc(gates)
                }
                AppleGpuPmgrGates::DualAsc(mut gates) => {
                    gates[0].id ^= 1;
                    AppleGpuPmgrGates::DualAsc(gates)
                }
            };
            assert!(!validate_static_topology(target, &wrong));
        }
    }

    #[test]
    fn interrupt_slot_count_and_valid_mask_fork_exactly_at_g16() {
        for target in APPLE_GPU_TARGETS {
            let topology = &pinned_topology(target).topology;
            match topology.sgx_interrupts {
                AppleGpuRawInterrupts::G15(_) => {
                    assert_eq!(topology.sgx_interrupts.len(), 7);
                    assert_eq!(topology.sgx_interrupts_valid, 0x5f);
                }
                AppleGpuRawInterrupts::G16Plus(_) => {
                    assert_eq!(topology.sgx_interrupts.len(), 8);
                    assert_eq!(topology.sgx_interrupts_valid, 0xdf);
                }
            }
        }
    }

    #[test]
    fn only_g17_has_the_second_asc_provider() {
        for target in APPLE_GPU_TARGETS {
            let topology = &pinned_topology(target).topology;
            let is_g17 = matches!(
                target,
                AppleGpuTarget::T8142G17G | AppleGpuTarget::T6050G17S | AppleGpuTarget::T6050G17C
            );
            assert_eq!(topology.gfx1_asc.is_some(), is_g17);
            assert_eq!(
                matches!(topology.pmgr_gates, AppleGpuPmgrGates::DualAsc(_)),
                is_g17
            );
        }
    }

    #[test]
    fn node_gate_references_resolve_to_exact_named_pmgr_records() {
        for target in APPLE_GPU_TARGETS {
            let topology = &pinned_topology(target).topology;
            assert!(has_gate(
                topology,
                AppleGpuPmgrGateRole::GfxSgx,
                topology.sgx_clock_gate_ids[0]
            ));
            assert!(has_gate(
                topology,
                AppleGpuPmgrGateRole::GfxBusy,
                topology.sgx_clock_gate_ids[1]
            ));
            assert_eq!(topology.sgx_clock_gate_ids, topology.sgx_power_gate_ids);
            assert!(has_gate(
                topology,
                AppleGpuPmgrGateRole::GfxAsc,
                topology.gfx_asc.clock_gate_id
            ));
            assert_eq!(topology.gfx_asc.compatible, "iop,ascwrap-v6");
            assert_eq!(topology.gfx_asc.iop_version, 1);
            assert_eq!(
                topology.gfx_asc.clock_gate_id,
                topology.gfx_asc.power_gate_id
            );
            if let Some(gfx1) = topology.gfx1_asc {
                assert!(has_gate(
                    topology,
                    AppleGpuPmgrGateRole::GfxAsc1,
                    gfx1.clock_gate_id
                ));
                assert_eq!(gfx1.compatible, "iop,ascwrap-v6");
                assert_eq!(gfx1.iop_version, 1);
                assert_eq!(gfx1.clock_gate_id, gfx1.power_gate_id);
            }
            for gate in topology.pmgr_gates.as_slice() {
                assert_eq!(gate.flags, 16);
                assert_eq!(gate.index, 0);
                assert_eq!(
                    gate.role == AppleGpuPmgrGateRole::GfxBusy,
                    gate.alias.is_none()
                );
            }
        }
    }

    #[test]
    fn t6050_preserves_underscore_names_and_shared_s_c_resources() {
        assert_eq!(T6050_G17S.topology, T6050_G17C.topology);
        assert_ne!(T6050_G17S.evidence, T6050_G17C.evidence);
        let names: Vec<_> = T6050_G17S
            .topology
            .pmgr_gates
            .as_slice()
            .iter()
            .map(|gate| gate.name)
            .collect();
        assert_eq!(names, ["GFX_SGX", "GFX_BUSY", "GFX_ASC", "GFX_ASC1"]);
    }

    #[test]
    fn static_evidence_never_opens_live_runtime_gates() {
        assert!(STATIC_TOPOLOGY_PROVEN);
        assert!(!LIVE_POWER_SEQUENCE_PROVEN);
        assert!(!LIVE_SUBMISSION_PROVEN);
    }
}
