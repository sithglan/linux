// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Pure policy for the opt-in native DVFS-input parity diagnostic
//! (`g17p_dvfs_parity`, default 0 = unchanged v68 behaviour).
//!
//! This file has no kernel dependencies so the exact source can be compiled
//! into a std test harness (`neo-dvfs-parity-v83/test-dvfs-policy.py`).
//!
//! Native protocol being reproduced (static RE, see
//! `work/neo-gpu-throughput-20260929/dvfs-inputs-20260929/DVFS-INPUTS.md`):
//!
//! * RegionC+0xe4 is the CLPC deadline control effort (16.16, <= 0x10000).
//!   Its only runtime producer on macOS is AppleCLPC through
//!   `AGXAccelerator::callPlatformFunction("SetCLPCDeadlineControlEffort")`
//!   -> `AGXArmFirmware::updateCLPCDeadlineInterfaceCE(u32)` ->
//!   `setCLPCDeadlineControlEffort` (kext 0x8893e4c). GFX1 samples it live
//!   (fw1 014960/01496c and 028c0c) and turns it into a DVFS *floor vote*
//!   (unit 9, 01397c..013a04 / 029260..02935c); ceilings still apply. It is
//!   not read by the primary's idle descent, so it cannot keep the GPU on.
//! * The CLPC decision rule itself is NOT decoded. This policy is a
//!   host-side approximation tied only to real submitted work: effort is
//!   raised on submission and cleared after `hold` of host-observed idleness.
//! * RegionC+0x7c/+0x80 is the native `updateSubmitInProgress` bracket.
//! * The 0x0087_0000_0000_0010 note is native `notifyFirmware(4, 1, 0)`,
//!   sent inside `updateSubmitInProgress(true)` only when the context
//!   priority is not 2 and GFX1 stats+0x66c is nonzero (kext 88a7f40..88a7f60).

/// bit0: publish a work-tied CLPC deadline control effort at RegionC+0xe4.
pub(crate) const PARITY_CE: u32 = 1 << 0;
/// bit1: native submit-in-progress bracket order (+0x7c, then +0x80).
pub(crate) const PARITY_BRACKET: u32 = 1 << 1;
/// bit2: native-gated 0x87 note inside the bracket (requires bit1).
pub(crate) const PARITY_KICK: u32 = 1 << 2;
pub(crate) const PARITY_MASK_ALL: u32 = PARITY_CE | PARITY_BRACKET | PARITY_KICK;

/// 1.0 in the kext's 16.16 effort encoding; larger values are rejected by
/// `updateCLPCDeadlineInterfaceCE(u32)` (`cmp w1, #0x10, lsl #12; b.ls`).
pub(crate) const CE_ONE: u32 = 0x1_0000;

/// GFX1 stats object (kext `fw+0xcd8`, 0x6a8 bytes, GFX1 `[0x1738c8]`,
/// secondary root +0xb8) as an offset in the primary private state view.
pub(crate) const GFX1_STATS_OFFSET: usize = 0xe780;
pub(crate) const GFX1_STATS_SIZE: usize = 0x6a8;
/// GFX1 writes `stats+0x66c = ctx+0x40 > ctx+0xb80` (fw1 029e14..029e28);
/// the kext reads it as the 0x87 gate (88a7f58..88a7f60).
pub(crate) const GFX1_STATS_KICK_GATE: usize = 0x66c;
/// Native `eAGXContextPriorityType` observed for ordinary Metal queues.
pub(crate) const NATIVE_DEFAULT_PRIORITY: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) mask: u32,
    pub(crate) hold_ns: u64,
    pub(crate) ramp_ns: u64,
    pub(crate) ce_max: u32,
}

pub(crate) const DEFAULT_HOLD_US: u64 = 16_000;
pub(crate) const MIN_HOLD_US: u64 = 100;
pub(crate) const MAX_HOLD_US: u64 = 1_000_000;
pub(crate) const MAX_RAMP_US: u64 = 5_000_000;

pub(crate) const DEFAULT_CONFIG: Config = Config {
    // v84: native CLPC deadline-effort parity (bit0) is the production
    // default (v83 hardware-qualified). Bracket/kick (bits 1/2) stay opt-in.
    mask: PARITY_CE,
    hold_ns: DEFAULT_HOLD_US * 1000,
    ramp_ns: 0,
    ce_max: CE_ONE,
};

fn parse_number(text: &str) -> Option<u64> {
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse::<u64>().ok(),
    }
}

/// Parse `MASK [HOLD_US [RAMP_US [CE_MAX]]]`. Omitted fields keep defaults.
pub(crate) fn parse(text: &str) -> Option<Config> {
    let mut fields = text.split_ascii_whitespace();
    let mask = parse_number(fields.next()?)?;
    if mask > u64::from(PARITY_MASK_ALL) {
        return None;
    }
    let mut config = DEFAULT_CONFIG;
    config.mask = mask as u32;
    if config.mask & PARITY_KICK != 0 && config.mask & PARITY_BRACKET == 0 {
        // Native only sends the note from inside updateSubmitInProgress(true).
        return None;
    }
    if let Some(hold) = fields.next() {
        let hold = parse_number(hold)?;
        if !(MIN_HOLD_US..=MAX_HOLD_US).contains(&hold) {
            return None;
        }
        config.hold_ns = hold * 1000;
    }
    if let Some(ramp) = fields.next() {
        let ramp = parse_number(ramp)?;
        if ramp > MAX_RAMP_US {
            return None;
        }
        config.ramp_ns = ramp * 1000;
    }
    if let Some(max) = fields.next() {
        let max = parse_number(max)?;
        if max == 0 || max > u64::from(CE_ONE) {
            return None;
        }
        config.ce_max = max as u32;
    }
    if fields.next().is_some() {
        return None;
    }
    Some(config)
}

/// Effort after `elapsed` ns of an uninterrupted submission stream.
pub(crate) fn ce_target(elapsed_ns: u64, config: &Config) -> u32 {
    if config.ramp_ns == 0 || elapsed_ns >= config.ramp_ns {
        return config.ce_max;
    }
    // u32 * u64 below 2^32 * 5e9 < 2^64.
    (u64::from(config.ce_max) * elapsed_ns / config.ramp_ns) as u32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdleAction {
    /// Nothing to do (not streaming, or work still in flight: the next
    /// completion service re-evaluates).
    None,
    /// Hold expired with no host-tracked work: publish effort 0.
    Clear,
    /// Idle but inside the hold window: re-check after this many ns.
    Recheck(u64),
}

/// Work-tied effort state. `streaming` is true from the first submission
/// until `hold_ns` of continuous host-observed idleness has elapsed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CePolicy {
    streaming: bool,
    stream_start_ns: u64,
    idle_since_ns: u64,
}

impl CePolicy {
    pub(crate) const fn new() -> Self {
        Self { streaming: false, stream_start_ns: 0, idle_since_ns: 0 }
    }

    pub(crate) fn streaming(&self) -> bool {
        self.streaming
    }

    /// A real submission is being published. Returns the effort that should
    /// be live in RegionC+0xe4 for it.
    pub(crate) fn on_submit(&mut self, now_ns: u64, config: &Config) -> u32 {
        if !self.streaming {
            self.streaming = true;
            self.stream_start_ns = now_ns;
        }
        self.idle_since_ns = 0;
        ce_target(now_ns.saturating_sub(self.stream_start_ns), config)
    }

    /// Completion service or the delayed re-check. `in_flight` is whether
    /// the host still tracks submitted, unretired work.
    pub(crate) fn on_idle_check(&mut self, now_ns: u64, in_flight: bool, config: &Config) -> IdleAction {
        if !self.streaming {
            return IdleAction::None;
        }
        if in_flight {
            self.idle_since_ns = 0;
            return IdleAction::None;
        }
        if self.idle_since_ns == 0 {
            self.idle_since_ns = now_ns.max(1);
        }
        let idle = now_ns.saturating_sub(self.idle_since_ns);
        if idle >= config.hold_ns {
            *self = Self::new();
            return IdleAction::Clear;
        }
        IdleAction::Recheck(config.hold_ns - idle)
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }
}

/// Native kick gate (kext 88a7f40..88a7f60): not priority 2 and GFX1 has
/// published a nonzero stats+0x66c. The additional IOGPU predicate at
/// 88a7f4c (`bl` outside the kext on `[acc+0x140]`) is not decoded.
pub(crate) fn kick_gate(stats_66c: u32, priority: u32) -> bool {
    stats_66c != 0 && priority != 2
}

/// Round a re-check delay up to whole milliseconds (at least 1).
pub(crate) fn recheck_ms(delay_ns: u64) -> u32 {
    let ms = delay_ns.div_ceil(1_000_000).max(1);
    ms.min(u64::from(u32::MAX)) as u32
}
