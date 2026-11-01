// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! G17 cold-boot and power preflight.
//!
//! This module joins three independently grounded contracts without issuing
//! MMIO, mailbox traffic, DMA, or firmware writes:
//!
//! - the decoded hardware identity and HAL/submission axes,
//! - the target-specific G17P/G17G/G17S dual-role host boot layout, and
//! - the bounded GFX1 SLEEP/NAP/0x21 transition grammar.
//!
//! A successful static preflight identifies the model which a future live
//! implementation must use. It is not device admission. Even after the pure
//! cold-boot observer reaches its final state, [`G17PowerPreflight::execution_gate`]
//! remains closed until every named live ownership/causality gap is resolved.

#![cfg_attr(not(test), allow(dead_code))]

#[cfg(not(test))]
use crate::{agx_power_recovery, g17_asc_bringup, g17_boot, identity};
#[cfg(test)]
#[path = "agx_power_recovery.rs"]
mod agx_power_recovery;
#[cfg(test)]
#[path = "g17_asc_bringup.rs"]
mod g17_asc_bringup;
#[cfg(test)]
#[path = "g17_boot.rs"]
mod g17_boot;
#[cfg(test)]
#[path = "identity.rs"]
mod identity;

/// Exact G17 targets with independently pinned host boot and power evidence.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17PowerTarget {
    /// A18 Pro, HAL200, dual firmware roles, classic + SKSM linked.
    G17P,
    /// M5, HAL300, dual firmware roles, SKSM-only host target.
    G17G,
    /// M5 Pro/Max host-family witness, HAL300, dual firmware roles, SKSM-only.
    G17S,
}

pub(crate) const G17_POWER_TARGETS: [G17PowerTarget; 3] = [
    G17PowerTarget::G17P,
    G17PowerTarget::G17G,
    G17PowerTarget::G17S,
];

impl G17PowerTarget {
    const fn boot_target(self) -> g17_boot::G17HostBootEvidenceTarget {
        match self {
            Self::G17P => g17_boot::G17HostBootEvidenceTarget::G17P,
            Self::G17G => g17_boot::G17HostBootEvidenceTarget::G17G,
            Self::G17S => g17_boot::G17HostBootEvidenceTarget::G17S,
        }
    }

    const fn gfx1_target(self) -> agx_power_recovery::G17Gfx1PowerEvidenceTarget {
        match self {
            Self::G17P => agx_power_recovery::G17Gfx1PowerEvidenceTarget::G17P,
            Self::G17G => agx_power_recovery::G17Gfx1PowerEvidenceTarget::G17G,
            Self::G17S => agx_power_recovery::G17Gfx1PowerEvidenceTarget::G17S,
        }
    }

    const fn pm_target(self) -> agx_power_recovery::G17PmConfigEvidenceTarget {
        match self {
            Self::G17P => agx_power_recovery::G17PmConfigEvidenceTarget::G17P,
            Self::G17G => agx_power_recovery::G17PmConfigEvidenceTarget::G17G,
            Self::G17S => agx_power_recovery::G17PmConfigEvidenceTarget::G17S,
        }
    }

    const fn asc_target(self) -> g17_asc_bringup::G17AscEvidenceTarget {
        match self {
            Self::G17P => g17_asc_bringup::G17AscEvidenceTarget::G17P,
            Self::G17G => g17_asc_bringup::G17AscEvidenceTarget::G17G,
            Self::G17S => g17_asc_bringup::G17AscEvidenceTarget::G17S,
        }
    }
}

/// Static target identity was not an exact pinned G17 power witness.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17PowerPreflightError {
    UnsupportedGeneration,
    UnsupportedVariant,
    UnexpectedUscGeneration,
    UnexpectedHalGeneration,
    UnexpectedFirmwareRoleTopology,
    UnexpectedSubmissionTransport,
    UnexpectedUatWidth,
    MissingT8140Topology,
    UnexpectedT8140Topology,
    BootSequence(g17_boot::BootError),
    ColdBootIncomplete,
    UnprovedPowerTransition(agx_power_recovery::PowerTransitionError),
}

/// A18 Pro performance-state handling selected only after exact D93AP
/// topology admission.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum T8140PerformanceStateStrategy {
    /// D93AP has `perf-state-count = 0`, sixteen zero records, and no
    /// `perf-states-sram`; do not run the legacy OPP synthesis path.
    NoLegacyOppSynthesis,
}

/// A18 Pro ASC launch handling selected only after exact D93AP admission.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum T8140AscLaunchStrategy {
    /// Start the independent GFX ASC, then GFX1 ASC, require both starts, and
    /// only then enter the combined-started wait.
    DualAscGfxThenGfx1ThenCombinedWait,
}

/// Operational strategy carried by an admitted A18 Pro preflight.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct T8140G17PPlatformStrategy {
    pub(crate) performance_states: T8140PerformanceStateStrategy,
    pub(crate) asc_launch: T8140AscLaunchStrategy,
    pub(crate) asc_providers: [agx_power_recovery::T8140G17PAscProvider; 2],
}

const T8140_G17P_PLATFORM_STRATEGY: T8140G17PPlatformStrategy = T8140G17PPlatformStrategy {
    performance_states: T8140PerformanceStateStrategy::NoLegacyOppSynthesis,
    asc_launch: T8140AscLaunchStrategy::DualAscGfxThenGfx1ThenCombinedWait,
    asc_providers: agx_power_recovery::D93AP_T8140_G17P_ASC_PROVIDERS,
};

/// Live evidence which is still required before Linux may execute G17 power.
pub(crate) mod missing_live {
    pub(crate) const FIRMWARE_LOAD_POWER_ORDER: u16 = 1 << 0;
    pub(crate) const FIRMWARE_LOAD_CACHE_MAINTENANCE: u16 = 1 << 1;
    pub(crate) const POWER_CALLBACK_ENDPOINT_CAUSALITY: u16 = 1 << 2;
    pub(crate) const INTERRUPT_ACK_ROUTE: u16 = 1 << 3;
    pub(crate) const COLD_BOOT_TO_SUBMISSION_CAUSALITY: u16 = 1 << 4;
    pub(crate) const GFX1_PIO_LINUX_OWNERSHIP: u16 = 1 << 5;
    pub(crate) const GFX1_POWER_SEQUENCE: u16 = 1 << 6;
    pub(crate) const RECOVERY_HANDSHAKE: u16 = 1 << 7;
    pub(crate) const COLD_BOOT_OBSERVATION: u16 = 1 << 8;
    pub(crate) const PMGR_ASC_GATE_ORDER: u16 = 1 << 9;
    pub(crate) const ASC_CONTROL_APERTURE_OWNERSHIP: u16 = 1 << 10;
    pub(crate) const IORVBAR_DMA_VISIBILITY: u16 = 1 << 11;
    pub(crate) const ASC_WRAPPER_MMIO_ENABLE: u16 = 1 << 12;
    pub(crate) const ASC_TEARDOWN_OWNER: u16 = 1 << 13;
}

const REQUIRED_LIVE_EVIDENCE: u16 = missing_live::FIRMWARE_LOAD_POWER_ORDER
    | missing_live::FIRMWARE_LOAD_CACHE_MAINTENANCE
    | missing_live::POWER_CALLBACK_ENDPOINT_CAUSALITY
    | missing_live::INTERRUPT_ACK_ROUTE
    | missing_live::COLD_BOOT_TO_SUBMISSION_CAUSALITY
    | missing_live::GFX1_PIO_LINUX_OWNERSHIP
    | missing_live::GFX1_POWER_SEQUENCE
    | missing_live::RECOVERY_HANDSHAKE
    | missing_live::COLD_BOOT_OBSERVATION
    | missing_live::PMGR_ASC_GATE_ORDER
    | missing_live::ASC_CONTROL_APERTURE_OWNERSHIP
    | missing_live::IORVBAR_DMA_VISIBILITY
    | missing_live::ASC_WRAPPER_MMIO_ENABLE
    | missing_live::ASC_TEARDOWN_OWNER;

/// Opaque result of the production power execution gate.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct MissingLiveEvidence(u16);

impl MissingLiveEvidence {
    pub(crate) const fn bits(self) -> u16 {
        self.0
    }

    pub(crate) const fn contains(self, evidence: u16) -> bool {
        self.0 & evidence != 0
    }
}

fn target_for_identity(
    identity: &identity::GpuIdentity,
) -> Result<G17PowerTarget, G17PowerPreflightError> {
    use identity::{GpuGen, GpuVariant};

    if identity.gpu_gen != GpuGen::G17 {
        return Err(G17PowerPreflightError::UnsupportedGeneration);
    }

    match identity.gpu_variant {
        GpuVariant::P => Ok(G17PowerTarget::G17P),
        GpuVariant::G => Ok(G17PowerTarget::G17G),
        GpuVariant::S => Ok(G17PowerTarget::G17S),
        // G17C uses the G17X host family, but a target-matched G17C firmware
        // and live power witness are not pinned. G17A is likewise absent.
        GpuVariant::C | GpuVariant::D | GpuVariant::A => {
            Err(G17PowerPreflightError::UnsupportedVariant)
        }
    }
}

fn validate_identity(
    identity: &identity::GpuIdentity,
) -> Result<G17PowerTarget, G17PowerPreflightError> {
    use identity::{FirmwareRoleTopology, GpuHalGeneration, SubmissionTransport};

    let target = target_for_identity(identity)?;
    if identity.usc_generation != 3 {
        return Err(G17PowerPreflightError::UnexpectedUscGeneration);
    }

    let (hal, submission) = match target {
        G17PowerTarget::G17P => (
            GpuHalGeneration::Hal200,
            SubmissionTransport::ClassicAndSksm,
        ),
        G17PowerTarget::G17G | G17PowerTarget::G17S => {
            (GpuHalGeneration::Hal300, SubmissionTransport::Sksm)
        }
    };
    if identity.gpu_hal_generation != hal {
        return Err(G17PowerPreflightError::UnexpectedHalGeneration);
    }
    if identity.firmware_roles != FirmwareRoleTopology::Dual {
        return Err(G17PowerPreflightError::UnexpectedFirmwareRoleTopology);
    }
    if identity.submission_transport != submission {
        return Err(G17PowerPreflightError::UnexpectedSubmissionTransport);
    }
    if identity.uat_input_address_bits != 42 {
        return Err(G17PowerPreflightError::UnexpectedUatWidth);
    }

    Ok(target)
}

fn missing_static_live_evidence() -> u16 {
    let mut missing = 0;
    let gaps = g17_boot::G17X_FIRMWARE_START_GAPS;

    if !gaps.load_firmware_mmio_and_power_order_proven {
        missing |= missing_live::FIRMWARE_LOAD_POWER_ORDER;
    }
    if !gaps.load_firmware_cache_maintenance_proven {
        missing |= missing_live::FIRMWARE_LOAD_CACHE_MAINTENANCE;
    }
    if !gaps.power_callback_to_endpoint_causality_proven {
        missing |= missing_live::POWER_CALLBACK_ENDPOINT_CAUSALITY;
    }
    if !gaps.interrupt_and_ack_route_proven {
        missing |= missing_live::INTERRUPT_ACK_ROUTE;
    }
    if !gaps.both_cold_boot_flags_to_submission_causality_proven {
        missing |= missing_live::COLD_BOOT_TO_SUBMISSION_CAUSALITY;
    }
    if !agx_power_recovery::G17_HOST_PIO_LINUX_MMIO_OWNERSHIP_PROVEN {
        missing |= missing_live::GFX1_PIO_LINUX_OWNERSHIP;
    }
    if !agx_power_recovery::EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE {
        missing |= missing_live::GFX1_POWER_SEQUENCE;
    }
    if !agx_power_recovery::EXECUTABLE_G17_RECOVERY_HANDSHAKE_AVAILABLE {
        missing |= missing_live::RECOVERY_HANDSHAKE;
    }
    let asc = g17_asc_bringup::G17_ASC_LIVE_GAPS;
    if !asc.pmgr_set_power_state_gate_order_owned {
        missing |= missing_live::PMGR_ASC_GATE_ORDER;
    }
    if !asc.asc_control_aperture_owned {
        missing |= missing_live::ASC_CONTROL_APERTURE_OWNERSHIP;
    }
    if !asc.iorvbar_aperture_and_dma_visibility_owned {
        missing |= missing_live::IORVBAR_DMA_VISIBILITY;
    }
    if !asc.wrapper_mmio_enable_source_proven {
        missing |= missing_live::ASC_WRAPPER_MMIO_ENABLE;
    }
    if !asc.stop_cpu_teardown_owner_wired {
        missing |= missing_live::ASC_TEARDOWN_OWNER;
    }

    (REQUIRED_LIVE_EVIDENCE & !missing_live::COLD_BOOT_OBSERVATION) & missing
}

/// Pure, target-bound observer for a future G17 dual-role cold boot.
///
/// Methods record already-observed host facts. They do not cause a role kick,
/// send a message, write a register, or admit submission.
#[derive(Debug)]
pub(crate) struct G17PowerPreflight {
    target: G17PowerTarget,
    t8140_strategy: Option<T8140G17PPlatformStrategy>,
    boot: g17_boot::DualRoleBoot,
}

impl G17PowerPreflight {
    /// Bind a preflight to one exact decoded target and all independent axes.
    ///
    /// G17P additionally requires the topology observed by the platform
    /// decoder. Supplying the expected constant without observing the device
    /// is not admission; production callers must pass their decoded value.
    pub(crate) fn new(
        identity: &identity::GpuIdentity,
        observed_t8140_topology: Option<&agx_power_recovery::T8140G17PAdtTopology>,
    ) -> Result<Self, G17PowerPreflightError> {
        let target = validate_identity(identity)?;
        let decoded = observed_t8140_topology
            .map(agx_power_recovery::decode_t8140_g17p_asc_providers);
        let t8140_strategy = Self::admit_t8140_strategy(target, decoded)?;

        Ok(Self {
            target,
            t8140_strategy,
            boot: g17_boot::DualRoleBoot::new(),
        })
    }

    /// Bind a preflight from what a *Linux* platform decoder can observe.
    ///
    /// This is the production T8140 entry point: the Linux device tree does
    /// not carry the full ADT node, but its GPU node's two `mboxes` expose
    /// exactly the dual ASCWrap v6 provider topology. The observation is
    /// bus-translated and then compared against the pinned D93AP providers;
    /// only an exact match admits the A18 Pro platform strategy. Supplying
    /// the expected constant without observing the device remains impossible
    /// from this path — the caller passes raw `reg`/interrupt values it read
    /// from the live tree.
    pub(crate) fn new_with_linux_dt(
        identity: &identity::GpuIdentity,
        observed: Option<&agx_power_recovery::T8140G17PLinuxDtTopology>,
    ) -> Result<Self, G17PowerPreflightError> {
        let target = validate_identity(identity)?;
        let decoded =
            observed.map(agx_power_recovery::decode_t8140_g17p_asc_providers_from_linux_dt);
        let t8140_strategy = Self::admit_t8140_strategy(target, decoded)?;

        Ok(Self {
            target,
            t8140_strategy,
            boot: g17_boot::DualRoleBoot::new(),
        })
    }

    /// Admit the T8140/G17P platform strategy from decoded providers.
    ///
    /// `decoded` is `None` when no topology observation was supplied at all,
    /// `Some(None)` when an observation was supplied but did not decode, and
    /// `Some(Some(providers))` when it decoded. Only providers exactly equal
    /// to the pinned D93AP expectation admit the strategy; every other
    /// combination fails closed.
    fn admit_t8140_strategy(
        target: G17PowerTarget,
        decoded: Option<Option<[agx_power_recovery::T8140G17PAscProvider; 2]>>,
    ) -> Result<Option<T8140G17PPlatformStrategy>, G17PowerPreflightError> {
        match (target, decoded) {
            (G17PowerTarget::G17P, Some(Some(providers)))
                if providers == T8140_G17P_PLATFORM_STRATEGY.asc_providers =>
            {
                Ok(Some(T8140_G17P_PLATFORM_STRATEGY))
            }
            (G17PowerTarget::G17P, Some(_)) => {
                Err(G17PowerPreflightError::UnexpectedT8140Topology)
            }
            (G17PowerTarget::G17P, None) => Err(G17PowerPreflightError::MissingT8140Topology),
            (_, Some(_)) => Err(G17PowerPreflightError::UnexpectedT8140Topology),
            (_, None) => Ok(None),
        }
    }

    pub(crate) const fn target(&self) -> G17PowerTarget {
        self.target
    }

    /// Present only when exact D93AP topology admission selected the A18 Pro
    /// no-legacy-OPP and dual-ASC launch paths.
    pub(crate) const fn t8140_strategy(&self) -> Option<T8140G17PPlatformStrategy> {
        self.t8140_strategy
    }

    pub(crate) const fn boot_layout(&self) -> g17_boot::G17HostBootLayout {
        g17_boot::g17_host_boot_layout(self.target.boot_target())
    }

    pub(crate) const fn boot_text_evidence(&self) -> g17_boot::G17HostBootTextEvidence {
        g17_boot::g17_host_boot_text_evidence(self.target.boot_target())
    }

    pub(crate) const fn asc_text_evidence(&self) -> g17_asc_bringup::G17AscTextEvidence {
        g17_asc_bringup::g17_asc_text_evidence(self.target.asc_target())
    }

    pub(crate) const fn opaque_pm_config(&self) -> &'static [u64; 6] {
        agx_power_recovery::g17_pm_config_for_evidence_target(self.target.pm_target())
    }

    pub(crate) fn mark_role_root_ready(
        &mut self,
        role: g17_boot::FirmwareRole,
    ) -> Result<(), G17PowerPreflightError> {
        self.boot
            .mark_initdata_root_ready(role)
            .map_err(G17PowerPreflightError::BootSequence)
    }

    pub(crate) fn record_role_kick(
        &mut self,
        role: g17_boot::FirmwareRole,
        succeeded: bool,
    ) -> Result<(), G17PowerPreflightError> {
        self.boot
            .record_kick(role, succeeded)
            .map_err(G17PowerPreflightError::BootSequence)
    }

    pub(crate) fn notify_role_started(&mut self, role: g17_boot::FirmwareRole) {
        self.boot.notify_started(role);
    }

    pub(crate) fn notify_role_cold_boot_complete(&mut self, role: g17_boot::FirmwareRole) {
        self.boot.notify_cold_boot_complete(role);
    }

    /// Whether all three independent host-observed cold-boot gates closed.
    pub(crate) fn cold_boot_observed(&self) -> bool {
        self.boot.handoff_ready()
    }

    /// Validate a requested edge against the pinned GFX1 transition grammar.
    ///
    /// This becomes available only after the complete pure cold-boot
    /// observation. Success proves only the edge shape. Callers must still
    /// pass [`Self::execution_gate`] before any eventual hardware action.
    pub(crate) fn validate_power_transition_shape(
        &self,
        old: u8,
        new: u8,
    ) -> Result<(), G17PowerPreflightError> {
        if !self.boot.handoff_ready() {
            return Err(G17PowerPreflightError::ColdBootIncomplete);
        }
        match agx_power_recovery::validate_g17_gfx1_power_transition(
            self.target.gfx1_target(),
            old,
            new,
        ) {
            Ok(()) => Ok(()),
            Err(e) => Err(G17PowerPreflightError::UnprovedPowerTransition(e)),
        }
    }

    /// Final production power gate. It remains separate from cold-boot state.
    pub(crate) fn execution_gate(&self) -> Result<(), MissingLiveEvidence> {
        let mut missing = missing_static_live_evidence();
        if !self.boot.handoff_ready() {
            missing |= missing_live::COLD_BOOT_OBSERVATION;
        }
        if missing == 0 {
            Ok(())
        } else {
            Err(MissingLiveEvidence(missing))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agx_power_recovery::{
        PowerTransitionError, G17_HAL200_PM_CONFIG_OPAQUE, G17_HAL300_PM_CONFIG_OPAQUE,
        GFX1_POWER_INTERMEDIATE_21, GFX1_POWER_NAP, GFX1_POWER_SLEEP,
    };
    use g17_boot::{BootError, FirmwareRole};
    use identity::{
        decode_gpu_identity, FirmwareRoleTopology, GpuGen, GpuHalGeneration, GpuIdentity,
        GpuVariant, SubmissionTransport,
    };

    fn id(family: u8, variant: u8) -> GpuIdentity {
        decode_gpu_identity(family, variant, 1).expect("identity must decode")
    }

    fn new_preflight(
        identity: &GpuIdentity,
    ) -> Result<G17PowerPreflight, G17PowerPreflightError> {
        let topology = if identity.gpu_gen == GpuGen::G17 && identity.gpu_variant == GpuVariant::P {
            Some(&agx_power_recovery::D93AP_T8140_G17P_ADT_TOPOLOGY)
        } else {
            None
        };
        G17PowerPreflight::new(identity, topology)
    }

    fn complete_cold_boot(preflight: &mut G17PowerPreflight) {
        preflight.mark_role_root_ready(FirmwareRole::Gfx).unwrap();
        preflight.mark_role_root_ready(FirmwareRole::Gfx1).unwrap();
        preflight.record_role_kick(FirmwareRole::Gfx, true).unwrap();
        preflight
            .record_role_kick(FirmwareRole::Gfx1, true)
            .unwrap();
        preflight.notify_role_started(FirmwareRole::Gfx);
        preflight.notify_role_cold_boot_complete(FirmwareRole::Gfx);
        preflight.notify_role_cold_boot_complete(FirmwareRole::Gfx1);
        assert!(preflight.cold_boot_observed());
    }

    #[test]
    fn exact_p_g_s_identities_select_distinct_targets() {
        for (identity, target, layout, pm) in [
            (
                id(0x0a, 0),
                G17PowerTarget::G17P,
                (0x1980, 0x1a6d, 0x2a0, 0x3d0, 0xb78, 0xca8),
                &G17_HAL200_PM_CONFIG_OPAQUE,
            ),
            (
                id(0x0b, 2),
                G17PowerTarget::G17G,
                (0x19c8, 0x1ab5, 0x2a8, 0x3e0, 0xb80, 0xcb0),
                &G17_HAL300_PM_CONFIG_OPAQUE,
            ),
            (
                id(0x0b, 3),
                G17PowerTarget::G17S,
                (0x1980, 0x1a6d, 0x2a0, 0x3d0, 0xb78, 0xca8),
                &G17_HAL300_PM_CONFIG_OPAQUE,
            ),
        ] {
            let preflight = new_preflight(&identity).unwrap();
            assert_eq!(preflight.target(), target);
            let actual = preflight.boot_layout();
            assert_eq!(
                (
                    actual.role_record_base,
                    actual.combined_started,
                    actual.accelerator,
                    actual.event_source,
                    actual.cold_boot_role0,
                    actual.cold_boot_role1,
                ),
                layout
            );
            assert_eq!(preflight.opaque_pm_config(), pm);
        }
        assert_eq!(G17_POWER_TARGETS.len(), 3);
    }

    #[test]
    fn g17p_requires_exact_t8140_topology_and_selects_operational_strategy() {
        let identity = id(0x0a, 0);
        let exact = agx_power_recovery::D93AP_T8140_G17P_ADT_TOPOLOGY;

        assert_eq!(
            G17PowerPreflight::new(&identity, None).unwrap_err(),
            G17PowerPreflightError::MissingT8140Topology
        );

        let mut collapsed_dual_asc = exact;
        collapsed_dual_asc.gfx1_asc_regs = exact.gfx_asc_regs;
        assert_eq!(
            G17PowerPreflight::new(&identity, Some(&collapsed_dual_asc)).unwrap_err(),
            G17PowerPreflightError::UnexpectedT8140Topology
        );

        let mut invented_legacy_opp = exact;
        invented_legacy_opp.perf_state_count = 2;
        assert_eq!(
            G17PowerPreflight::new(&identity, Some(&invented_legacy_opp)).unwrap_err(),
            G17PowerPreflightError::UnexpectedT8140Topology
        );

        let preflight = G17PowerPreflight::new(&identity, Some(&exact)).unwrap();
        assert_eq!(
            preflight.t8140_strategy(),
            Some(T8140G17PPlatformStrategy {
                performance_states: T8140PerformanceStateStrategy::NoLegacyOppSynthesis,
                asc_launch: T8140AscLaunchStrategy::DualAscGfxThenGfx1ThenCombinedWait,
                asc_providers: agx_power_recovery::D93AP_T8140_G17P_ASC_PROVIDERS,
            })
        );

        for identity in [id(0x0b, 2), id(0x0b, 3)] {
            assert_eq!(
                G17PowerPreflight::new(&identity, Some(&exact)).unwrap_err(),
                G17PowerPreflightError::UnexpectedT8140Topology
            );
            assert_eq!(new_preflight(&identity).unwrap().t8140_strategy(), None);
        }
    }

    fn j700_linux_dt_topology() -> agx_power_recovery::T8140G17PLinuxDtTopology {
        agx_power_recovery::T8140G17PLinuxDtTopology {
            gfx: agx_power_recovery::T8140G17PLinuxDtAscObservation {
                role_is_gfx1: false,
                wrapper_cpu_base: 0x4_8260_0000,
                wrapper_size: 0x0008_8000,
                iorvbar_cpu_base: 0x4_8205_0000,
                iorvbar_size: 8,
                send_empty_irq: 993,
                recv_not_empty_irq: 996,
            },
            gfx1: agx_power_recovery::T8140G17PLinuxDtAscObservation {
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
    fn linux_dt_observation_admits_g17p_exactly() {
        let identity = id(0x0a, 0);
        let exact = j700_linux_dt_topology();

        // No observation means missing topology. Never substitute the constant.
        assert_eq!(
            G17PowerPreflight::new_with_linux_dt(&identity, None).unwrap_err(),
            G17PowerPreflightError::MissingT8140Topology
        );

        // The exact J700 observation selects the same strategy as the exact
        // D93AP ADT observation.
        let preflight = G17PowerPreflight::new_with_linux_dt(&identity, Some(&exact)).unwrap();
        assert_eq!(
            preflight.t8140_strategy(),
            new_preflight(&identity).unwrap().t8140_strategy()
        );

        // A perturbed observation is refused, not "close enough".
        let mut wrong_irq = exact;
        wrong_irq.gfx.send_empty_irq = 994;
        assert_eq!(
            G17PowerPreflight::new_with_linux_dt(&identity, Some(&wrong_irq)).unwrap_err(),
            G17PowerPreflightError::UnexpectedT8140Topology
        );

        let mut collapsed = exact;
        collapsed.gfx1.wrapper_cpu_base = exact.gfx.wrapper_cpu_base;
        assert_eq!(
            G17PowerPreflight::new_with_linux_dt(&identity, Some(&collapsed)).unwrap_err(),
            G17PowerPreflightError::UnexpectedT8140Topology
        );

        // An observation that cannot even be bus-translated is refused.
        let mut untranslatable = exact;
        untranslatable.gfx.wrapper_cpu_base = 0x1000;
        assert_eq!(
            G17PowerPreflight::new_with_linux_dt(&identity, Some(&untranslatable)).unwrap_err(),
            G17PowerPreflightError::UnexpectedT8140Topology
        );

        // Non-G17P targets never take a T8140 observation.
        for other in [id(0x0b, 2), id(0x0b, 3)] {
            assert_eq!(
                G17PowerPreflight::new_with_linux_dt(&other, Some(&exact)).unwrap_err(),
                G17PowerPreflightError::UnexpectedT8140Topology
            );
            assert_eq!(
                G17PowerPreflight::new_with_linux_dt(&other, None)
                    .unwrap()
                    .t8140_strategy(),
                None
            );
        }
    }

    #[test]
    fn text_evidence_remains_target_specific() {
        let p = new_preflight(&id(0x0a, 0)).unwrap();
        let g = new_preflight(&id(0x0b, 2)).unwrap();
        let s = new_preflight(&id(0x0b, 3)).unwrap();
        assert_eq!(
            p.boot_text_evidence().boot_firmware_va,
            0xffff_fe00_08a4_4ce0
        );
        assert_eq!(
            g.boot_text_evidence().boot_firmware_va,
            0xffff_fe00_08b7_666c
        );
        assert_eq!(
            s.boot_text_evidence().boot_firmware_va,
            0xffff_fe00_08b3_9754
        );
        assert_eq!(
            p.asc_text_evidence().a7iop_start_cpu_with_options,
            0xffff_fe00_08ab_c6d0
        );
        assert_eq!(
            g.asc_text_evidence().a7iop_start_cpu_with_options,
            0xffff_fe00_08be_fd20
        );
        assert_eq!(
            s.asc_text_evidence().a7iop_start_cpu_with_options,
            0xffff_fe00_08bb_6a50
        );
        assert_ne!(p.boot_text_evidence(), g.boot_text_evidence());
        assert_ne!(g.boot_text_evidence(), s.boot_text_evidence());
        assert_ne!(p.asc_text_evidence(), g.asc_text_evidence());
        assert_ne!(g.asc_text_evidence(), s.asc_text_evidence());
    }

    #[test]
    fn every_independent_identity_axis_fails_closed() {
        let base = id(0x0a, 0);
        for (changed, error) in [
            (
                GpuIdentity {
                    gpu_gen: GpuGen::G16,
                    ..base
                },
                G17PowerPreflightError::UnsupportedGeneration,
            ),
            (
                GpuIdentity {
                    gpu_variant: GpuVariant::A,
                    ..base
                },
                G17PowerPreflightError::UnsupportedVariant,
            ),
            (
                GpuIdentity {
                    usc_generation: 2,
                    ..base
                },
                G17PowerPreflightError::UnexpectedUscGeneration,
            ),
            (
                GpuIdentity {
                    gpu_hal_generation: GpuHalGeneration::Hal300,
                    ..base
                },
                G17PowerPreflightError::UnexpectedHalGeneration,
            ),
            (
                GpuIdentity {
                    firmware_roles: FirmwareRoleTopology::Single,
                    ..base
                },
                G17PowerPreflightError::UnexpectedFirmwareRoleTopology,
            ),
            (
                GpuIdentity {
                    submission_transport: SubmissionTransport::Sksm,
                    ..base
                },
                G17PowerPreflightError::UnexpectedSubmissionTransport,
            ),
            (
                GpuIdentity {
                    uat_input_address_bits: 39,
                    ..base
                },
                G17PowerPreflightError::UnexpectedUatWidth,
            ),
        ] {
            assert_eq!(new_preflight(&changed).unwrap_err(), error);
        }
    }

    #[test]
    fn unpinned_g17_variants_never_alias_a_power_target() {
        for identity in [id(0x0a, 5), id(0x0b, 4)] {
            assert_eq!(
                new_preflight(&identity).unwrap_err(),
                G17PowerPreflightError::UnsupportedVariant
            );
        }
        assert_eq!(
            new_preflight(&id(0x07, 2)).unwrap_err(),
            G17PowerPreflightError::UnsupportedGeneration
        );
    }

    #[test]
    fn cold_boot_observer_enforces_root_and_kick_order() {
        let mut preflight = new_preflight(&id(0x0b, 3)).unwrap();
        preflight.mark_role_root_ready(FirmwareRole::Gfx).unwrap();
        assert_eq!(
            preflight.record_role_kick(FirmwareRole::Gfx, true),
            Err(G17PowerPreflightError::BootSequence(
                BootError::InitdataRootsIncomplete
            ))
        );
        preflight.notify_role_started(FirmwareRole::Gfx);
        preflight.notify_role_cold_boot_complete(FirmwareRole::Gfx);
        preflight.notify_role_cold_boot_complete(FirmwareRole::Gfx1);
        assert!(!preflight.cold_boot_observed());

        let mut wrong_order = new_preflight(&id(0x0b, 2)).unwrap();
        wrong_order.mark_role_root_ready(FirmwareRole::Gfx).unwrap();
        wrong_order
            .mark_role_root_ready(FirmwareRole::Gfx1)
            .unwrap();
        assert_eq!(
            wrong_order.record_role_kick(FirmwareRole::Gfx1, true),
            Err(G17PowerPreflightError::BootSequence(
                BootError::WrongKickOrder
            ))
        );
        assert!(!wrong_order.cold_boot_observed());
    }

    #[test]
    fn power_edges_require_complete_cold_boot_and_exact_bridge() {
        for identity in [id(0x0a, 0), id(0x0b, 2), id(0x0b, 3)] {
            let mut preflight = new_preflight(&identity).unwrap();
            assert_eq!(
                preflight
                    .validate_power_transition_shape(GFX1_POWER_SLEEP, GFX1_POWER_INTERMEDIATE_21),
                Err(G17PowerPreflightError::ColdBootIncomplete)
            );
            complete_cold_boot(&mut preflight);

            for (old, new) in [
                (GFX1_POWER_SLEEP, GFX1_POWER_INTERMEDIATE_21),
                (GFX1_POWER_NAP, GFX1_POWER_INTERMEDIATE_21),
                (GFX1_POWER_INTERMEDIATE_21, GFX1_POWER_SLEEP),
                (GFX1_POWER_INTERMEDIATE_21, GFX1_POWER_NAP),
            ] {
                assert!(preflight.validate_power_transition_shape(old, new).is_ok());
            }
            assert_eq!(
                preflight.validate_power_transition_shape(GFX1_POWER_SLEEP, GFX1_POWER_NAP),
                Err(G17PowerPreflightError::UnprovedPowerTransition(
                    PowerTransitionError::DirectSleepNapIsFatal
                ))
            );
            assert_eq!(
                preflight.validate_power_transition_shape(GFX1_POWER_SLEEP, GFX1_POWER_SLEEP),
                Err(G17PowerPreflightError::UnprovedPowerTransition(
                    PowerTransitionError::UnprovenTransition
                ))
            );
        }
    }

    #[test]
    fn complete_observation_never_opens_execution_or_submission() {
        for identity in [id(0x0a, 0), id(0x0b, 2), id(0x0b, 3)] {
            let mut preflight = new_preflight(&identity).unwrap();
            let initial = preflight.execution_gate().unwrap_err();
            assert_eq!(
                initial.bits(),
                REQUIRED_LIVE_EVIDENCE
                    & !missing_live::FIRMWARE_LOAD_CACHE_MAINTENANCE
                    & !missing_live::ASC_WRAPPER_MMIO_ENABLE
                    & !missing_live::ASC_CONTROL_APERTURE_OWNERSHIP
                    & !missing_live::ASC_TEARDOWN_OWNER
            );
            assert!(initial.contains(missing_live::COLD_BOOT_OBSERVATION));
            complete_cold_boot(&mut preflight);
            let complete = preflight.execution_gate().unwrap_err();
            assert_eq!(
                complete.bits(),
                REQUIRED_LIVE_EVIDENCE
                    & !missing_live::FIRMWARE_LOAD_CACHE_MAINTENANCE
                    & !missing_live::COLD_BOOT_OBSERVATION
                    & !missing_live::ASC_WRAPPER_MMIO_ENABLE
                    & !missing_live::ASC_CONTROL_APERTURE_OWNERSHIP
                    & !missing_live::ASC_TEARDOWN_OWNER
            );
            assert!(!complete.contains(missing_live::COLD_BOOT_OBSERVATION));
            for bit in [
                missing_live::FIRMWARE_LOAD_POWER_ORDER,
                missing_live::POWER_CALLBACK_ENDPOINT_CAUSALITY,
                missing_live::INTERRUPT_ACK_ROUTE,
                missing_live::COLD_BOOT_TO_SUBMISSION_CAUSALITY,
                missing_live::GFX1_PIO_LINUX_OWNERSHIP,
                missing_live::GFX1_POWER_SEQUENCE,
                missing_live::RECOVERY_HANDSHAKE,
                missing_live::PMGR_ASC_GATE_ORDER,
                missing_live::IORVBAR_DMA_VISIBILITY,
            ] {
                assert!(complete.contains(bit));
            }
            assert!(!complete.contains(missing_live::FIRMWARE_LOAD_CACHE_MAINTENANCE));
            assert!(!complete.contains(missing_live::ASC_WRAPPER_MMIO_ENABLE));
            assert!(!complete.contains(missing_live::ASC_CONTROL_APERTURE_OWNERSHIP));
            assert!(!complete.contains(missing_live::ASC_TEARDOWN_OWNER));
        }
        assert!(!g17_boot::EXECUTABLE_G17X_FIRMWARE_START_HANDSHAKE_AVAILABLE);
        assert!(!agx_power_recovery::EXECUTABLE_GFX1_POWER_SEQUENCE_AVAILABLE);
        assert!(!agx_power_recovery::EXECUTABLE_G17_RECOVERY_HANDSHAKE_AVAILABLE);
        assert!(!g17_asc_bringup::EXECUTABLE_G17_ASC_BRINGUP_AVAILABLE);
    }
}
