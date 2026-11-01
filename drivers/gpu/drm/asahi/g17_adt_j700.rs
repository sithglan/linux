// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! Device-derived T8140 (A18 Pro) GPU node constants, board **J700AP**.
//!
//! Source: the *live* `IODeviceTree` `sgx@70000000` node of a Mac17,5
//! (Apple A18 Pro, macOS 26.5.1 build 25F80, kernel `RELEASE_ARM64_T8140`),
//! read with `ioreg -lw0 -p IODeviceTree -n sgx` on 2026-08-26. The node's
//! `compatible` is `"gpu,t8140"`, which is exactly the OF entry
//! [`crate::driver`] matches for this part.
//!
//! # Why this is separate from `D93AP_T8140_G17P_ADT_TOPOLOGY`
//!
//! [`crate::agx_power_recovery`] grounds the same SoC from **D93AP** (a phone
//! board) out of a *static* ADT, where the perf-state records are placeholders:
//! `perf_state_count: 0`, `gpu_num_perf_states: 2`,
//! `perf_state_records_all_zero: true`, `perf_states_sram_present: false`.
//!
//! J700AP is a different board *and* a live (post-iBoot/PMGR population) tree,
//! and reports a fully populated table instead: `perf-state-count = 16`,
//! `gpu-num-perf-states = 15`, sixteen non-zero (freq, voltage) records, plus a
//! second `perf-states-sram` rail table. Both observations are correct for what
//! they describe; neither supersedes the other. Do not merge them.
//!
//! # Status
//!
//! These are *observations of Apple's device tree*,*not* a claim that the
//! firmware accepts them, and nothing here has been used in a live boot. They
//! are the concrete per-board inputs a first J700 bring-up needs, and they are
//! validated only for internal consistency ([`validate_j700_gpu_node`]).

#![allow(dead_code)]

/// A GPU performance state: core frequency and its rail voltage.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct J700PerfState {
    pub(crate) freq_hz: u32,
    pub(crate) volt_mv: u32,
}

/// A physical address range from an ADT `reg`-style property.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct J700Range {
    pub(crate) base: u64,
    pub(crate) size: u64,
}

/// `perf-state-count` on this board (records in each table).
pub(crate) const J700_PERF_STATE_COUNT: u32 = 16;
/// `gpu-num-perf-states`: the active states, i.e. every record but the idle
/// entry at index 0.
pub(crate) const J700_GPU_NUM_PERF_STATES: u32 = 15;
/// `perf-state-table-count`.
pub(crate) const J700_PERF_STATE_TABLE_COUNT: u32 = 1;

/// `perf-states`: the GPU core rail. Index 0 is the idle state (0 Hz).
pub(crate) const J700_PERF_STATES: [J700PerfState; J700_PERF_STATE_COUNT as usize] = [
    J700PerfState { freq_hz: 0, volt_mv: 125 },
    J700PerfState { freq_hz: 338_000_000, volt_mv: 605 },
    J700PerfState { freq_hz: 492_000_000, volt_mv: 630 },
    J700PerfState { freq_hz: 618_000_000, volt_mv: 650 },
    J700PerfState { freq_hz: 796_000_000, volt_mv: 695 },
    J700PerfState { freq_hz: 928_000_000, volt_mv: 745 },
    J700PerfState { freq_hz: 952_000_000, volt_mv: 800 },
    J700PerfState { freq_hz: 1_056_000_000, volt_mv: 800 },
    J700PerfState { freq_hz: 1_053_000_000, volt_mv: 845 },
    J700PerfState { freq_hz: 1_170_000_000, volt_mv: 845 },
    J700PerfState { freq_hz: 1_152_000_000, volt_mv: 885 },
    J700PerfState { freq_hz: 1_278_000_000, volt_mv: 885 },
    J700PerfState { freq_hz: 1_204_000_000, volt_mv: 905 },
    J700PerfState { freq_hz: 1_338_000_000, volt_mv: 905 },
    J700PerfState { freq_hz: 1_326_000_000, volt_mv: 975 },
    J700PerfState { freq_hz: 1_470_000_000, volt_mv: 975 },
];

/// `perf-states-sram`: same frequency ladder, SRAM rail voltages. The low
/// states are floored at 765 mV rather than tracking the core rail down.
pub(crate) const J700_PERF_STATES_SRAM: [J700PerfState; J700_PERF_STATE_COUNT as usize] = [
    J700PerfState { freq_hz: 0, volt_mv: 765 },
    J700PerfState { freq_hz: 338_000_000, volt_mv: 765 },
    J700PerfState { freq_hz: 492_000_000, volt_mv: 765 },
    J700PerfState { freq_hz: 618_000_000, volt_mv: 765 },
    J700PerfState { freq_hz: 796_000_000, volt_mv: 765 },
    J700PerfState { freq_hz: 928_000_000, volt_mv: 765 },
    J700PerfState { freq_hz: 952_000_000, volt_mv: 800 },
    J700PerfState { freq_hz: 1_056_000_000, volt_mv: 800 },
    J700PerfState { freq_hz: 1_053_000_000, volt_mv: 845 },
    J700PerfState { freq_hz: 1_170_000_000, volt_mv: 845 },
    J700PerfState { freq_hz: 1_152_000_000, volt_mv: 885 },
    J700PerfState { freq_hz: 1_278_000_000, volt_mv: 885 },
    J700PerfState { freq_hz: 1_204_000_000, volt_mv: 905 },
    J700PerfState { freq_hz: 1_338_000_000, volt_mv: 905 },
    J700PerfState { freq_hz: 1_326_000_000, volt_mv: 975 },
    J700PerfState { freq_hz: 1_470_000_000, volt_mv: 975 },
];

/// `gfx-handoff-base` / `gfx-handoff-size`.
///
/// This is the AP<->firmware UAT handoff region — the PPL Dekker
/// `magic_ap`/`magic_fw` rendezvous that [`crate::mmu`] blocks on. That module
/// records that the on-disk M5 firmware proves the region *exists* but "does
/// not expose its address"; for this board the device tree does, so a J700
/// bring-up no longer has to discover it.
pub(crate) const J700_GFX_HANDOFF: J700Range = J700Range {
    base: 0x0000_0101_fff3_0000,
    size: 0x4000,
};

/// `gfx-shared-region-base` / size.
pub(crate) const J700_GFX_SHARED_REGION: J700Range = J700Range {
    base: 0x0000_0101_fff3_8000,
    size: 0x8_0000,
};

/// `gfx-shared-l2-region-base` / size.
pub(crate) const J700_GFX_SHARED_L2_REGION: J700Range = J700Range {
    base: 0x0000_0101_fff3_4000,
    size: 0x4000,
};

/// `gfx-data-base` / size — the GFX role's firmware data region.
pub(crate) const J700_GFX_DATA: J700Range = J700Range {
    base: 0x0000_0100_01d7_c000,
    size: 0x13_4000,
};

/// `gfx1-data-base` / size — the **GFX1** (scheduler role) data region. Its
/// presence is the device tree's own statement of the dual-role topology.
pub(crate) const J700_GFX1_DATA: J700Range = J700Range {
    base: 0x0000_0100_01eb_0000,
    size: 0x12_c000,
};

/// `gpu-region-base` / size.
pub(crate) const J700_GPU_REGION: J700Range = J700Range {
    base: 0x0000_0101_fffb_8000,
    size: 0x4000,
};

/// `rtkit-private-vm-region-base` / size.
pub(crate) const J700_RTKIT_PRIVATE_VM: J700Range = J700Range {
    base: 0xffff_fc00_0000_0000,
    size: 0x20_0000_0000,
};

/// `power-gates` and `clock-gates` are both `[0xeb, 0xec]`: two gates, one per
/// ASC role, independently corroborating the dual-ASC topology.
pub(crate) const J700_POWER_GATES: [u32; 2] = [0xeb, 0xec];
pub(crate) const J700_CLOCK_GATES: [u32; 2] = [0xeb, 0xec];

/// `interrupts` on the sgx node.
pub(crate) const J700_SGX_INTERRUPTS: [u32; 8] =
    [0x3cf, 0x3d0, 0x3d1, 0x3d2, 0x3e9, 0x3eb, 0x3de, 0x3e0];

/// `clock-ids`.
pub(crate) const J700_CLOCK_ID: u32 = 0x15f;
/// `gpu-perf-tgt-utilization`.
pub(crate) const J700_PERF_TARGET_UTILIZATION: u32 = 92;
/// `gpu-power-sample-period`.
pub(crate) const J700_POWER_SAMPLE_PERIOD: u32 = 16;
/// `ttbat-phys-addr-base`.
pub(crate) const J700_TTBAT_PHYS_ADDR_BASE: u32 = 0x0407_ffee;

/// The perf-state clock column in MHz, in the order the init-data
/// hardware-data perf-state tables expect (see [`crate::g17_initdata`]; the
/// tables themselves are not yet encoded, see
/// [`crate::g17_initdata::PERF_TABLE_VALUES_ENCODED`]).
pub(crate) const fn perf_state_clocks_mhz() -> [u32; J700_PERF_STATE_COUNT as usize] {
    let mut out = [0u32; J700_PERF_STATE_COUNT as usize];
    let mut i = 0;
    while i < J700_PERF_STATES.len() {
        out[i] = J700_PERF_STATES[i].freq_hz / 1_000_000;
        i += 1;
    }
    out
}

/// Internal-consistency check over the observed node. This validates the shape
/// of what was read; it cannot attest that the firmware accepts these values.
pub(crate) const fn validate_j700_gpu_node() -> bool {
    if J700_GPU_NUM_PERF_STATES + 1 != J700_PERF_STATE_COUNT {
        return false;
    }
    if J700_PERF_STATES[0].freq_hz != 0 {
        return false;
    }
    // The two rails must describe the same frequency ladder.
    let mut i = 0;
    while i < J700_PERF_STATES.len() {
        if J700_PERF_STATES[i].freq_hz != J700_PERF_STATES_SRAM[i].freq_hz {
            return false;
        }
        if J700_PERF_STATES[i].volt_mv == 0 || J700_PERF_STATES_SRAM[i].volt_mv == 0 {
            return false;
        }
        i += 1;
    }
    // The GFX and GFX1 data regions are distinct and must not overlap.
    let gfx_end = J700_GFX_DATA.base + J700_GFX_DATA.size;
    let gfx1_end = J700_GFX1_DATA.base + J700_GFX1_DATA.size;
    if J700_GFX_DATA.base >= gfx1_end || J700_GFX1_DATA.base >= gfx_end {
        // disjoint, as expected
    } else {
        return false;
    }
    if J700_POWER_GATES[0] == J700_POWER_GATES[1] {
        return false;
    }
    J700_GFX_HANDOFF.size != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_j700_gpu_node_is_self_consistent() {
        assert!(validate_j700_gpu_node());
    }

    #[test]
    fn perf_state_ladder_matches_the_observed_table() {
        assert_eq!(J700_PERF_STATE_COUNT, 16);
        assert_eq!(J700_GPU_NUM_PERF_STATES, 15);
        assert_eq!(J700_PERF_STATE_TABLE_COUNT, 1);
        // Idle floor and turbo ceiling as observed on hardware.
        assert_eq!(J700_PERF_STATES[0].freq_hz, 0);
        assert_eq!(J700_PERF_STATES[0].volt_mv, 125);
        assert_eq!(J700_PERF_STATES[15].freq_hz, 1_470_000_000);
        assert_eq!(J700_PERF_STATES[15].volt_mv, 975);
        // The SRAM rail floors the low states instead of tracking the core.
        assert_eq!(J700_PERF_STATES_SRAM[0].volt_mv, 765);
        assert_eq!(J700_PERF_STATES_SRAM[5].volt_mv, 765);
        // Both rails converge from the 952 MHz state upward.
        let mut i = 6;
        while i < J700_PERF_STATES.len() {
            assert_eq!(J700_PERF_STATES[i].volt_mv, J700_PERF_STATES_SRAM[i].volt_mv);
            i += 1;
        }
    }

    #[test]
    fn clock_column_is_megahertz_in_table_order() {
        let mhz = perf_state_clocks_mhz();
        assert_eq!(mhz[0], 0);
        assert_eq!(mhz[1], 338);
        assert_eq!(mhz[15], 1470);
        assert_eq!(mhz.len(), J700_PERF_STATE_COUNT as usize);
    }

    #[test]
    fn dual_role_regions_and_gates_are_distinct() {
        assert_ne!(J700_GFX_DATA.base, J700_GFX1_DATA.base);
        assert_eq!(J700_POWER_GATES, [0xeb, 0xec]);
        assert_eq!(J700_CLOCK_GATES, J700_POWER_GATES);
        assert_eq!(J700_SGX_INTERRUPTS.len(), 8);
    }

    #[test]
    fn handoff_region_is_the_uat_rendezvous_and_is_page_sized() {
        assert_eq!(J700_GFX_HANDOFF.base, 0x0000_0101_fff3_0000);
        assert_eq!(J700_GFX_HANDOFF.size, 0x4000);
        // The handoff sits immediately below the shared L2 region.
        assert_eq!(
            J700_GFX_HANDOFF.base + J700_GFX_HANDOFF.size,
            J700_GFX_SHARED_L2_REGION.base
        );
    }
}
