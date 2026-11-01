// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! G17P live-boot lifecycle boundary.
//!
//! T8140 has two independent ASCWrap v6 providers. The provider owns the
//! wrapper and IOP VBAR mappings; DRM must never map an `asc1` alias or write
//! `CPU_RUN` directly. A previous stage did exactly that and then freed RTKit
//! transports while both CPUs could still be running.
//!
//! The current path keeps each hardware owner explicit:
//!
//! 1. [`crate::driver`] admits the exact live DT topology before any target
//!    effect and passes an unforgeable [`T8140LiveTopologyAdmission`] token.
//! 2. This module resolves both mailbox providers and verifies their roles and
//!    exact resources through the provider-owned lifecycle surface.
//! 3. Both providers report a matching CPU start and stop implementation.
//! 4. Both RTKit receivers are armed before CPU start. GFX completes its
//!    handshake before GFX1 starts.
//! 5. The retained runtime drops the registered queue, GFX1/GFX transports,
//!    provider CPUs, and manager resources in strict reverse order.

#![allow(dead_code)]

#[path = "m5_live_boot.rs"]
pub(crate) mod m5;

use core::{
    ffi::{c_int, c_uint},
    mem::MaybeUninit,
    sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering},
};

use kernel::macros::vtable;
use kernel::new_condvar;
use kernel::{
    bindings,
    device::Core,
    error::{from_err_ptr, to_result},
    io::{Io, Mmio, MmioRaw},
    iosys_map::IoSysMapRef,
    platform,
    prelude::*,
    soc::apple::rtkit,
    sync::{Arc, CondVar, CondVarTimeoutResult},
    time::{delay::fsleep, Delta},
};
#[cfg(CONFIG_DEV_COREDUMP)]
use kernel::{
    devcoredump, new_mutex,
    sync::Mutex,
    time::{msecs_to_jiffies, Instant, Monotonic},
};

use crate::{
    driver::{AsahiDevice, T8140LiveTopologyAdmission},
    g17_completion, g17_initdata, g17_lifecycle, g17_manager, g17_resources, g17_rtkit,
    g17_submission, g17_uapi, gem, hw, identity, mmu, module_parameters, regs,
};
use kernel::sync::aref::ARef;

/// Ordered boundary stages.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum BootStage {
    ExactTopology,
    ProviderOwnership,
    ReversibleLifecycle,
    StartCpus,
    BootRtkit,
    PublishInitdata,
    InitdataAcknowledged,
    DeviceControlPublished,
    RegisterComputeQueue,
}

const INITDATA_ACK_TYPE: u16 = 0x09;
const INITDATA_ACK_POLLS: usize = 200;
const CONTROL_COUNTER_POLLS: usize = 100;
const FIRMWARE_RECOVERY_POLLS: usize = 500;
const G17P_CRASHLOG_MAX_BYTES: usize = 256 * 1024;
/// 1ms samples of `sgx+0xe01000 & 0xf` after the submit power record. Nothing
/// acknowledges the device-control power lever, so this is an observation
/// window, not a handshake: the firmware's own core-power sequencer
/// (`0x20a70`) budgets `0xe10` timebase ticks for its `sgx+0x2_11d0` poll.
const G17P_POWER_WIRE_POLLS: usize = 20;
/// Separate budget for the GPU cores actually reaching the powered state.
///
/// Record retirement is a firmware acknowledgement and lands in a couple of
/// milliseconds; the powered witness is driven by the core power sequencer,
/// which is slower and slower still while another client is rendering. Sharing
/// one 20ms budget meant a wake whose record had demonstrably retired
/// (counters == target) was reported as a timeout because the sequencer had
/// not finished yet -- measured on G17P with a compositor and a browser both
/// submitting, where the very next record observed powered=true 1ms later.
const G17P_POWER_WIRE_POWERED_POLLS: usize = 250;
const G17P_RTKIT_UAT_DVA_MASK: u64 = (1u64 << 40) - 1;

/// One role's iBoot-placed RTKit image, from the live J700 ADT.
///
/// `/arm-io/gfx-asc` and `/arm-io/gfx1-asc` each carry a `segment-ranges`
/// property of 32-byte records `{u64 phys, u64 iova, u64 remap, u32 size,
/// u32 flags}`. On T8140 there are two segments per role, `__TEXT;__DATA`,
/// and `remap == phys`. Everything here is below the start of System RAM, so
/// Linux never maps it and cannot corrupt it -- but the firmware writes its
/// own `__DATA` the whole time it runs, and nothing puts it back.
#[derive(Debug, Copy, Clone)]
struct G17PFirmwareImage {
    text_phys: u64,
    data_phys: u64,
    data_size: usize,
}

const G17P_GFX_FIRMWARE_IMAGE: G17PFirmwareImage = G17PFirmwareImage {
    text_phys: 0x1_0000_974000,
    data_phys: 0x1_0001_d7c000,
    data_size: 0x13_4000,
};

const G17P_GFX1_FIRMWARE_IMAGE: G17PFirmwareImage = G17PFirmwareImage {
    text_phys: 0x1_0000_9c0000,
    data_phys: 0x1_0001_eb0000,
    data_size: 0x12_c000,
};

/// Pristine `__DATA` for both roles, taken before either processor ever ran.
#[cfg(CONFIG_DEV_COREDUMP)]
struct G17PFirmwareDataSnapshot {
    gfx: KVVec<u8>,
    gfx1: KVVec<u8>,
}

/// Snapshot both roles' `__DATA` while it is still exactly as iBoot left it.
///
/// Refuses unless each role's live IORVBAR names precisely the `__TEXT` base
/// this geometry expects, with its valid bit set. That reset vector is the one
/// on-machine witness that the ADT-derived addresses describe THIS board, and
/// it is read from the provider's own mapping, so a wrong table can never send
/// a write somewhere it does not belong.
#[cfg(CONFIG_DEV_COREDUMP)]
fn snapshot_g17p_firmware_data(pdev: &platform::Device) -> Option<G17PFirmwareDataSnapshot> {
    let images = [G17P_GFX_FIRMWARE_IMAGE, G17P_GFX1_FIRMWARE_IMAGE];
    for (index, image) in images.iter().enumerate() {
        let provider = G17PMailboxProvider::get(pdev.as_ref(), index).ok()?;
        let observed = provider.asc_snapshot().ok()?;
        let expected = image.text_phys | 1;
        if observed.iorvbar != expected {
            dev_warn!(
                pdev.as_ref(),
                "G17PMARK fw-snapshot role={} REFUSED: iorvbar={:#x} expected={:#x}; the pinned carveout geometry does not describe this board\n",
                index,
                observed.iorvbar,
                expected
            );
            return None;
        }
    }
    // The invalidating read, for the same reason the drift count uses it: the
    // baseline has to come from DRAM, not from whatever the AP happens to be
    // holding for these physical addresses after iBoot placed the image.
    let gfx = crate::pgtable::UatPageTable::copy_live_firmware_physical_range(
        G17P_GFX_FIRMWARE_IMAGE.data_phys,
        G17P_GFX_FIRMWARE_IMAGE.data_size,
    )
    .ok()?;
    let gfx1 = crate::pgtable::UatPageTable::copy_live_firmware_physical_range(
        G17P_GFX1_FIRMWARE_IMAGE.data_phys,
        G17P_GFX1_FIRMWARE_IMAGE.data_size,
    )
    .ok()?;
    dev_info!(
        pdev.as_ref(),
        "G17PMARK fw-snapshot armed gfx __DATA {:#x}+{:#x} gfx1 __DATA {:#x}+{:#x}; both reset vectors match the pinned __TEXT bases\n",
        G17P_GFX_FIRMWARE_IMAGE.data_phys,
        G17P_GFX_FIRMWARE_IMAGE.data_size,
        G17P_GFX1_FIRMWARE_IMAGE.data_phys,
        G17P_GFX1_FIRMWARE_IMAGE.data_size,
    );
    Some(G17PFirmwareDataSnapshot { gfx, gfx1 })
}

/// How many bytes of a role's live `__DATA` differ from its pristine copy.
///
/// Read through the cache-invalidating path, not the plain one. The probe-time
/// snapshot pulled all of `__DATA` into the AP's PIPT caches as clean lines,
/// and the firmware writes its globals over a path that does not reliably
/// invalidate them, so a plain write-back read here can hand back the pristine
/// bytes this is trying to compare against and report drift=0 for a segment
/// the firmware has been scribbling on for minutes. Reporting zero drift is
/// the one wrong answer that would retire the whole hypothesis.
#[cfg(CONFIG_DEV_COREDUMP)]
fn count_g17p_firmware_data_drift(image: &G17PFirmwareImage, pristine: &[u8]) -> Result<usize> {
    let live = crate::pgtable::UatPageTable::copy_live_firmware_physical_range(
        image.data_phys,
        image.data_size,
    )?;
    Ok(live
        .iter()
        .zip(pristine.iter())
        .filter(|(a, b)| a != b)
        .count())
}

/// Wait for both ASC wrappers to report idle after their CPUs were stopped.
///
/// `AppleASCWrapV6::stopCPU` clears RUN and then waits about 10 ms for
/// `IDLE_STATUS & 3` before anything downstream gates the power domain. The
/// Linux provider clears RUN and returns immediately, so a gate that follows
/// can land while the block still has AXI traffic in flight -- which is what a
/// GFX PMGR register latched BUSY with PS_ACTUAL=0, deaf to every later target
/// write, looks like. Logs what each role actually reached either way, so the
/// wait is a measurement and not just a delay.
fn wait_for_g17p_asc_idle(pdev: &platform::Device, checkpoint: &'static str) {
    const BUDGET_US: u64 = 10_000;
    const STEP_US: u64 = 50;
    for index in 0..2usize {
        let Ok(provider) = G17PMailboxProvider::get(pdev.as_ref(), index) else {
            continue;
        };
        let mut waited = 0u64;
        let mut observed = 0u32;
        let mut idle = false;
        loop {
            match provider.asc_snapshot() {
                Ok(state) => {
                    observed = state.idle_status;
                    // AppleASCWrapV6::_isIdle is `(IDLE_STATUS & 3) != 0`.
                    // Not `== 3`: bit 0 and bit 1 are separate idle witnesses
                    // and either one alone means idle.
                    if state.idle_status & 3 != 0 {
                        idle = true;
                        break;
                    }
                }
                Err(_) => break,
            }
            if waited >= BUDGET_US {
                break;
            }
            fsleep(Delta::from_micros(STEP_US as i64));
            waited += STEP_US;
        }
        dev_info!(
            pdev.as_ref(),
            "G17PMARK asc-idle role={} checkpoint={} idle={} waited_us={} idle_word={:#x}\n",
            index,
            checkpoint,
            idle,
            waited,
            observed
        );
    }
}

// Runtime-PM entry points the Rust `kernel` crate does not wrap. These are
// `EXPORT_SYMBOL_GPL` in drivers/base/power/runtime.c.
extern "C" {
    fn devm_pm_runtime_enable(dev: *mut bindings::device) -> c_int;
    fn __pm_runtime_set_status(dev: *mut bindings::device, status: c_uint) -> c_int;
    fn __pm_runtime_suspend(dev: *mut bindings::device, rpmflags: c_int) -> c_int;
    fn __pm_runtime_resume(dev: *mut bindings::device, rpmflags: c_int) -> c_int;
    fn pm_runtime_barrier(dev: *mut bindings::device) -> c_int;
}

/// `RPM_GET_PUT` from `include/linux/pm_runtime.h`.
const RPM_GET_PUT: c_int = 0x04;
/// `RPM_ACTIVE` from `enum rpm_status`.
const RPM_ACTIVE: c_uint = 0;

/// Module-local, read-only view of one existing ASCWrap mailbox provider.
///
/// The C provider owns the mapping and all lifecycle transitions. This view
/// neither maps an alias nor changes callback ownership; it only borrows the
/// provider's established mailbox-register pointer for two control reads.
struct G17PMailboxProvider {
    mbox: *mut bindings::apple_mbox,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum G17PAscwrapV6Role {
    Gfx,
    Gfx1,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct G17PAscwrapV6Lifecycle {
    role: G17PAscwrapV6Role,
    wrapper: (u64, u64),
    iop_vbar: (u64, u64),
    missing: u32,
}

impl G17PMailboxProvider {
    fn cpu_state(&self) -> Result<(u32, u32)> {
        let mut control = 0;
        let mut status = 0;
        // SAFETY: the provider remains linked and owns the wrapper mapping;
        // both output pointers refer to writable stack storage.
        to_result(unsafe {
            bindings::apple_mbox_ascwrap_v6_get_cpu_state(
                self.mbox, &mut control, &mut status)
        })?;
        Ok((control, status))
    }

    fn get(dev: &kernel::device::Device, index: usize) -> Result<Self> {
        // SAFETY: `dev` is live and apple_mbox_get() establishes a device link
        // to keep the resolved provider alive for this consumer.
        let mbox = unsafe {
            from_err_ptr(bindings::apple_mbox_get(
                dev.as_raw(),
                index.try_into()?,
            ))?
        };
        Ok(Self { mbox })
    }

    fn lifecycle(&self) -> Result<G17PAscwrapV6Lifecycle> {
        let mut raw = MaybeUninit::<bindings::apple_mbox_ascwrap_v6_lifecycle>::uninit();
        // SAFETY: `self.mbox` is the live provider returned by
        // apple_mbox_get() and `raw` points to writable storage.
        to_result(unsafe {
            bindings::apple_mbox_ascwrap_v6_get_lifecycle(self.mbox, raw.as_mut_ptr())
        })?;
        // SAFETY: the successful C call initialized every field.
        let raw = unsafe { raw.assume_init() };
        let role = match raw.role as u32 {
            1 => G17PAscwrapV6Role::Gfx,
            2 => G17PAscwrapV6Role::Gfx1,
            _ => return Err(EINVAL),
        };
        Ok(G17PAscwrapV6Lifecycle {
            role,
            wrapper: (raw.wrapper_start, raw.wrapper_size),
            iop_vbar: (raw.iop_vbar_start, raw.iop_vbar_size),
            missing: raw.missing,
        })
    }

    fn require_safe_cpu_lifecycle(&self) -> Result {
        // SAFETY: this is a capability check on the live provider pointer.
        to_result(unsafe {
            bindings::apple_mbox_ascwrap_v6_require_safe_cpu_lifecycle(self.mbox)
        })
    }

    fn start_cpu(&self) -> Result {
        // SAFETY: lifecycle serialization and MMIO ownership remain in C.
        to_result(unsafe { bindings::apple_mbox_ascwrap_v6_start_cpu(self.mbox) })
    }

    fn stop_cpu(&self) -> Result {
        // SAFETY: lifecycle serialization and MMIO ownership remain in C.
        to_result(unsafe { bindings::apple_mbox_ascwrap_v6_stop_cpu(self.mbox) })
    }

    /// The registers that decide whether a restarted ASC will re-run its
    /// RTKit handshake.
    ///
    /// `apple_rtkit_boot()` is passive: it waits for the IOP to volunteer a
    /// HELLO and never pokes it. A rebuild therefore depends entirely on the
    /// ASC actually restarting from its reset vector when `CPU_RUN` is set
    /// again. Nothing in the driver has ever looked at whether it does, so
    /// read the deciding state rather than infer it from the timeout.
    /// The provider's own `struct device`, which is what carries the runtime-PM
    /// reference that keeps the shared `gfx` power domain up.
    fn provider_device(&self) -> *mut bindings::device {
        // SAFETY: the device link established by `get()` keeps the provider
        // and therefore this pointer valid for this consumer's lifetime.
        unsafe { (*self.mbox).dev }
    }

    /// A second handle on the same provider, for diagnostics that must read
    /// ASC state while the lifecycle owner holds the real one.
    ///
    /// `get()` established a device link that keeps the provider alive for the
    /// consumer's whole bound lifetime, and this type owns nothing else, so an
    /// alias cannot outlive what it points at.
    fn diagnostic_alias(&self) -> Self {
        Self { mbox: self.mbox }
    }

    fn asc_snapshot(&self) -> Result<G17PAscSnapshot> {
        /// ASCWrap v6 CPU control/status pair, from the provider's own
        /// `APPLE_ASCWRAP_V6_CPU_CONTROL`.
        const CPU_CONTROL: usize = 0x44;
        const CPU_STATUS: usize = 0x48;
        const CPU_WINDOW: usize = CPU_STATUS + core::mem::size_of::<u32>();
        /// The mailbox block starts at wrapper + 0x8000; its first word is the
        /// wrapper idle status `_isIdle` reads, and the two FIFO control words
        /// follow at the standard ASC offsets.
        const IDLE_STATUS: usize = 0x0;
        const A2I_CONTROL: usize = 0x110;
        const I2A_CONTROL: usize = 0x114;
        const MBOX_WINDOW: usize = I2A_CONTROL + core::mem::size_of::<u32>();
        /// The IOP reset vector lives in its own eight-byte window.
        const VBAR_WINDOW: usize = 0x8;

        // SAFETY: the provider owns all three mappings and the device link
        // established by apple_mbox_get() keeps them valid for this consumer.
        let (wrapper, regs, vbar) = unsafe {
            (
                (*self.mbox).wrapper_regs as usize,
                (*self.mbox).regs as usize,
                (*self.mbox).iop_vbar_regs as usize,
            )
        };
        if wrapper == 0 || regs == 0 || vbar == 0 {
            return Err(ENODEV);
        }
        let cpu_raw = MmioRaw::<kernel::io::Region<CPU_WINDOW>>::new_region(wrapper, CPU_WINDOW)?;
        let mbox_raw = MmioRaw::<kernel::io::Region<MBOX_WINDOW>>::new_region(regs, MBOX_WINDOW)?;
        let vbar_raw = MmioRaw::<kernel::io::Region<VBAR_WINDOW>>::new_region(vbar, VBAR_WINDOW)?;
        // SAFETY: each window covers exactly the relaxed reads issued below.
        let cpu = unsafe { Mmio::from_raw(cpu_raw) };
        // SAFETY: as above.
        let mailbox = unsafe { Mmio::from_raw(mbox_raw) };
        // SAFETY: as above.
        let iop_vbar = unsafe { Mmio::from_raw(vbar_raw) };
        Ok(G17PAscSnapshot {
            cpu_control: cpu.relaxed().read32(CPU_CONTROL),
            cpu_status: cpu.relaxed().read32(CPU_STATUS),
            idle_status: mailbox.relaxed().read32(IDLE_STATUS),
            iorvbar: iop_vbar.relaxed().try_read64(0)?,
            a2i_control: mailbox.relaxed().read32(A2I_CONTROL),
            i2a_control: mailbox.relaxed().read32(I2A_CONTROL),
        })
    }

    fn a2i_control(&self) -> Result<u32> {
        const A2I_CONTROL: usize = 0x110;
        const CONTROL_WINDOW_SIZE: usize = A2I_CONTROL + core::mem::size_of::<u32>();

        // SAFETY: the provider owns the live ASC mailbox mapping. The device
        // link established by apple_mbox_get() keeps it valid, and this view
        // is bounded to the standard read-only A2I control register.
        let raw = MmioRaw::<kernel::io::Region<CONTROL_WINDOW_SIZE>>::new_region(
            unsafe { (*self.mbox).regs as usize },
            CONTROL_WINDOW_SIZE,
        )?;
        // SAFETY: `raw` covers the complete 32-bit control register read.
        Ok(unsafe { Mmio::from_raw(raw) }
            .relaxed()
            .read32(A2I_CONTROL))
    }

}

// SAFETY: the provider mapping is device-link protected and the reader issues
// only relaxed MMIO reads. Mailbox mutation remains synchronized in C.
unsafe impl Send for G17PMailboxProvider {}
// SAFETY: as above; concurrent snapshots do not change either FIFO.
unsafe impl Sync for G17PMailboxProvider {}

/// One read of the ASC state a restart depends on.
#[derive(Debug, Copy, Clone, Default)]
struct G17PAscSnapshot {
    cpu_control: u32,
    cpu_status: u32,
    idle_status: u32,
    iorvbar: u64,
    a2i_control: u32,
    i2a_control: u32,
}

/// Report the deciding ASC state at one named point of a start or restart.
///
/// `run` is `CPU_CONTROL` bit 4, the only bit this driver has ever written.
/// `iorvbar` is the reset vector with its valid bit in bit 0: a zero here
/// after a restart means the ASC has nowhere to boot from, which no amount of
/// waiting for HELLO can fix.
fn log_g17p_asc_state(
    dev: &kernel::device::Device,
    provider: &G17PMailboxProvider,
    role: &'static str,
    checkpoint: &'static str,
) {
    match provider.asc_snapshot() {
        Ok(state) => dev_info!(
            dev,
            "G17PMARK asc-state role={} checkpoint={} cpu_control={:#x} run={} cpu_status={:#x} idle_word={:#x} iorvbar={:#x} a2i={:#x} i2a={:#x}\n",
            role,
            checkpoint,
            state.cpu_control,
            (state.cpu_control >> 4) & 1,
            state.cpu_status,
            state.idle_status,
            state.iorvbar,
            state.a2i_control,
            state.i2a_control,
        ),
        Err(error) => dev_warn!(
            dev,
            "G17PMARK asc-state role={} checkpoint={} unavailable ({:?})\n",
            role,
            checkpoint,
            error
        ),
    }
}

fn log_g17p_a2i_control(
    dev: &kernel::device::Device,
    mailbox: &G17PMailboxProvider,
    checkpoint: &'static str,
) {
    match mailbox.a2i_control() {
        Ok(control) => dev_info!(
            dev,
            "G17P A2I checkpoint={} raw={:#x} count={} empty={} full={} read={} write={} enabled={}\n",
            checkpoint,
            control,
            (control >> 20) & 0xf,
            (control >> 17) & 1,
            (control >> 16) & 1,
            (control >> 12) & 0xf,
            (control >> 8) & 0xf,
            control & 1,
        ),
        Err(error) => dev_err!(
            dev,
            "G17P A2I checkpoint={} read failed ({:?})\n",
            checkpoint,
            error,
        ),
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct G17PWorkScanObservation {
    first: u32,
    last: u32,
    observed_or: u32,
    transitions: u32,
    samples: u32,
    failures: u32,
}

impl G17PWorkScanObservation {
    const fn new() -> Self {
        Self {
            first: 0,
            last: 0,
            observed_or: 0,
            transitions: 0,
            samples: 0,
            failures: 0,
        }
    }

    fn observe(&mut self, value: u32) {
        if self.samples == 0 {
            self.first = value;
        } else if self.last != value {
            self.transitions = self.transitions.saturating_add(1);
        }
        self.last = value;
        self.observed_or |= value;
        self.samples = self.samples.saturating_add(1);
    }

    fn observe_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }
}

/// Accumulated KTrace drain outcome for one role across a submit poll loop.
#[derive(Debug, Copy, Clone, Default)]
struct G17PKtraceTotals {
    consumed: u32,
    printed: u32,
    skipped: u32,
    lost_records: u64,
    lost_windows: u32,
    saw_add_kicks: bool,
    last_pause_mask: u64,
    last_producer: u32,
}

impl G17PKtraceTotals {
    fn accumulate(&mut self, stats: &g17_resources::G17PKtraceDrainStats) {
        self.consumed = self.consumed.saturating_add(stats.consumed);
        self.printed = self.printed.saturating_add(stats.printed);
        self.skipped = self.skipped.saturating_add(stats.skipped);
        self.lost_records = self.lost_records.saturating_add(stats.lost_records);
        self.lost_windows = self.lost_windows.saturating_add(stats.lost_windows);
        self.saw_add_kicks |= stats.saw_add_kicks;
        if stats.last_pause_mask != 0 {
            self.last_pause_mask = stats.last_pause_mask;
        }
        self.last_producer = stats.producer;
    }
}

/// The KTrace drain is deliberately gated on the same parameter that arms the
/// class mask: arming without a consumer makes the firmware busy-wait once and
/// then silently drop records, so "armed but undrained" must be impossible.
fn g17p_ktrace_enabled() -> bool {
    module_parameters::g17p_fw_trace.value() != 0
}

fn g17p_ktrace_role_name(role: g17_initdata::InstanceRole) -> &'static str {
    match role {
        g17_initdata::InstanceRole::Primary => "primary",
        g17_initdata::InstanceRole::Secondary => "secondary",
    }
}

/// Checkpoint probe: dump the firmware log ring. DRAM only, like the KTrace
/// drain -- neither touches the sgx window, so both are safe on a path where
/// the GPU cores may never have powered.
fn dump_g17p_firmware_log(
    dev: &kernel::device::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    checkpoint: &'static str,
) {
    if module_parameters::g17p_fw_log.value() == 0 {
        return;
    }
    match manager.dump_firmware_log(60) {
        Ok(count) => dev_info!(
            dev,
            "G17P fwlog checkpoint={} records={}\n",
            checkpoint,
            count
        ),
        Err(error) => dev_warn!(
            dev,
            "G17P fwlog checkpoint={} dump failed ({:?})\n",
            checkpoint,
            error
        ),
    }
}

/// Checkpoint probe: verify the published channel-14 pointer pair and drain
/// whatever the firmware has already traced. Note this consumes what it reads.
fn probe_g17p_ktrace(
    dev: &kernel::device::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    checkpoint: &'static str,
) {
    if !g17p_ktrace_enabled() {
        return;
    }
    let verbose = module_parameters::g17p_fw_ktrace_verbose.value() != 0;
    let budget = module_parameters::g17p_fw_ktrace_budget.value();
    // The bounded binary sink replaces ordinary per-submission reporting,
    // including this caller's summaries, not just the ring's record prints.
    // Keep draining both roles and retain error/boot/fault reports.
    let quiet = cfg!(g17p_tsq_ktrace_window)
        && matches!(checkpoint, "render-pre-doorbell" | "render-post-wait");
    for role in [
        g17_initdata::InstanceRole::Primary,
        g17_initdata::InstanceRole::Secondary,
    ] {
        let role_name = g17p_ktrace_role_name(role);
        match manager.check_ktrace_ring_pointers(role) {
            Ok(_) if quiet => {}
            Ok((state, ring)) => dev_info!(
                dev,
                "G17P ktrace pointers checkpoint={} role={} state={:#x} ring={:#x}\n",
                checkpoint,
                role_name,
                state,
                ring
            ),
            Err(error) => dev_warn!(
                dev,
                "G17P ktrace pointers checkpoint={} role={} check failed ({:?})\n",
                checkpoint,
                role_name,
                error
            ),
        }
        match manager.drain_ktrace_ring(role, verbose, budget) {
            Ok(_) if quiet => {}
            Ok(stats) => dev_info!(
                dev,
                "G17P ktrace armed check checkpoint={} role={} producer-snapshot={} consumed-count={} consumer-before={} consumer-after={} printed={} skipped={} lost={}/{} addkicks={} pause-mask={:#x}\n",
                checkpoint,
                role_name,
                stats.producer,
                stats.consumed,
                stats.consumer_before,
                (stats.consumer_before + stats.consumed) % g17_completion::G17P_KTRACE_CAPACITY,
                stats.printed,
                stats.skipped,
                stats.lost_records,
                stats.lost_windows,
                stats.saw_add_kicks,
                stats.last_pause_mask
            ),
            Err(error) => dev_warn!(
                dev,
                "G17P ktrace armed check checkpoint={} role={} failed ({:?})\n",
                checkpoint,
                role_name,
                error
            ),
        }
    }
}

/// Checkpoint probe: read the firmware EVENT ring (channel-table entry 13).
///
/// Read-only unless `g17p_report_drain` is 2. This ring is where the firmware
/// asks the host to grow the tiling parameter buffer (event type 7), and the
/// G17P path has never read it, so a request would sit in DRAM unanswered
/// while the tiler waited. DRAM only -- no sgx MMIO, so it is safe at both
/// checkpoints including the timeout, where the cores have powered back down.
fn probe_g17p_reports_with_mode(
    dev: &kernel::device::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    checkpoint: &'static str,
    advance: bool,
    budget: u32,
) {
    for role in [
        g17_initdata::InstanceRole::Primary,
        g17_initdata::InstanceRole::Secondary,
    ] {
        let role_name = g17p_ktrace_role_name(role);
        match manager.check_report_ring_pointers(role) {
            Ok((state, ring)) => dev_info!(
                dev,
                "G17P report pointers checkpoint={} role={} state={:#x} ring={:#x}\n",
                checkpoint,
                role_name,
                state,
                ring
            ),
            Err(error) => dev_warn!(
                dev,
                "G17P report pointers checkpoint={} role={} check failed ({:?})\n",
                checkpoint,
                role_name,
                error
            ),
        }
        match manager.drain_report_ring(role, advance, budget) {
            Ok(stats) => dev_info!(
                dev,
                "G17P report ring checkpoint={} role={} producer={} consumer={} pending={} read={} printed={} undecoded={} grow-requests={} restart-requests={} history-records={} history-grow-requests={} history-restart-requests={} out-of-range={} advanced={} fwlog-write={}\n",
                checkpoint,
                role_name,
                stats.producer,
                stats.consumer_before,
                stats.producer.wrapping_sub(stats.consumer_before) & 0xff,
                stats.consumed,
                stats.printed,
                stats.skipped,
                stats.grow_requests,
                stats.restart_requests,
                stats.history_records,
                stats.history_grow_requests,
                stats.history_restart_requests,
                stats.cursor_out_of_range,
                advance,
                stats.fwlog_write
            ),
            Err(error) => dev_warn!(
                dev,
                "G17P report ring checkpoint={} role={} failed ({:?})\n",
                checkpoint,
                role_name,
                error
            ),
        }
    }
}

fn probe_g17p_reports(
    dev: &kernel::device::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    checkpoint: &'static str,
) {
    let mode = crate::g17_config::REPORT_DRAIN;
    if mode == 0 {
        return;
    }
    probe_g17p_reports_with_mode(
        dev,
        manager,
        checkpoint,
        mode >= 2,
        crate::g17_config::REPORT_DRAIN_BUDGET,
    );
}

/// Recovery-only report-ring history. Unlike the checkpoint diagnostic this
/// never runs on a successful submission and never advances either consumer.
fn probe_g17p_recovery_report_history(
    dev: &kernel::device::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
) {
    if module_parameters::g17p_recovery_snapshot.value() == 0 {
        return;
    }
    probe_g17p_reports_with_mode(dev, manager, "recovery-pre-ack-history", false, 8);
}

fn log_g17p_status_a(
    dev: &kernel::device::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    checkpoint: &'static str,
) {
    if !crate::util::g17p_verbose() {
        return;
    }
    match manager.status_a_snapshot() {
        Ok(snapshot) => dev_info!(
            dev,
            "G17P Status-A checkpoint={} scan-address={:#x} scan-active={} runtime-address={:#x} raw={:#x} active={} scheduler-constructed={}\n",
            checkpoint,
            snapshot.scan_address,
            snapshot.scan_active,
            snapshot.address,
            snapshot.raw,
            snapshot.active,
            snapshot.scheduler_constructed,
        ),
        Err(error) => dev_err!(
            dev,
            "G17P Status-A checkpoint={} read failed ({:?})\n",
            checkpoint,
            error,
        ),
    }
}

fn log_g17p_firmware_recovery_handshake(
    dev: &kernel::device::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    checkpoint: &'static str,
) {
    match manager.firmware_recovery_handshake_snapshot() {
        Ok(snapshot) => dev_info!(
            dev,
            "G17P firmware-recovery handshake checkpoint={} status-b={:#x} epoch-address={:#x} epoch={} state-address={:#x} state={} callback-known-unregistered={} waiting-first-response={} waiting-final-clear={} power-callback-count-address={:#x} power-callback-count={}\n",
            checkpoint,
            snapshot.status_b,
            snapshot.epoch_address,
            snapshot.epoch,
            snapshot.state_address,
            snapshot.state,
            snapshot.callback_known_unregistered(),
            snapshot.waiting_for_first_response(),
            snapshot.waiting_for_final_clear(),
            snapshot.power_callback_count_address,
            snapshot.power_callback_count,
        ),
        Err(error) => dev_err!(
            dev,
            "G17P firmware-recovery handshake checkpoint={} read failed ({:?})\n",
            checkpoint,
            error,
        ),
    }
}

/// Log the firmware's own fault report next to the MMU fault bank.
///
/// `main_config + 0x26c` is bundle view 3, so the report is in host memory we
/// already own; the firmware names the blamed kick queue there, that queue's
/// data master, and the recovery state. Nothing else in the driver reads it.
fn log_g17p_fault_report(
    dev: &kernel::device::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    checkpoint: &'static str,
) {
    // This report is validated host-owned WC memory, not the diagnostic MMIO
    // fault bank. Keep it available with register probing disabled, especially
    // before recovery acknowledgement clears the firmware's blame fields.
    let mut blamed_slot: Option<usize> = None;
    let mut blamed_data_master: Option<u32> = None;
    match manager.firmware_fault_report_snapshot() {
        Ok(snapshot) => dev_info!(
            dev,
            "G17P fault report checkpoint={} report={:#x} state={} host-requested={} reason={} sources={} blamed-qid={}/{} ksm-slot={}/{} data-master={}/{}\n",
            checkpoint,
            snapshot.report,
            snapshot.state,
            snapshot.host_requested,
            snapshot.reason,
            snapshot.source_count,
            snapshot.qid_present,
            snapshot.qid,
            snapshot.slot_present,
            snapshot.slot,
            snapshot.data_master_present,
            snapshot.data_master,
        ),
        Err(error) => dev_warn!(
            dev,
            "G17P fault report checkpoint={} read failed ({:?})\n",
            checkpoint,
            error,
        ),
    }
    if let Ok(snapshot) = manager.firmware_fault_report_snapshot() {
        if snapshot.slot_present != 0 {
            blamed_slot = Some(snapshot.slot as usize);
        }
        if snapshot.data_master_present != 0 {
            blamed_data_master = Some(snapshot.data_master);
        }
    }
    // Keep this independent of the report read: failure of either bounded
    // host-memory diagnostic must not suppress the other or touch MMIO.
    //
    // Slot 0 is NOT the blamed slot. `FUN_5450` blames the first set bit of
    // the DM fault mask, and two live faults blamed slots 1 and 0. Reading
    // only slot 0 printed an all-zero record for both and said nothing about
    // either. Scan the whole array the view can hold, name every record the
    // firmware actually wrote, and always print the blamed one.
    let blamed = blamed_slot.filter(|slot| *slot < g17_resources::DM1_WATCHDOG_SLOT_COUNT);
    let mut populated = 0u32;
    let mut failures = 0u32;
    for slot in 0..g17_resources::DM1_WATCHDOG_SLOT_COUNT {
        match manager.firmware_dm1_slot_snapshot(slot) {
            Ok(snapshot) => {
                if snapshot.is_blank() {
                    continue;
                }
                populated |= 1u32 << slot;
                let blame_tag = match blamed == Some(slot) {
                    true => " BLAMED",
                    false => "",
                };
                dev_info!(
                    dev,
                    "G17P DM1 monitor checkpoint={} slot={}{} address={:#x} raw={:08x?} monitor-state={} monitor-timestamp={:#x}\n",
                    checkpoint,
                    slot,
                    blame_tag,
                    snapshot.address,
                    snapshot.raw,
                    snapshot.state(),
                    snapshot.timestamp(),
                );
            }
            Err(_) => failures = failures.saturating_add(1),
        }
    }
    // A blamed slot the firmware left blank is itself the result: it says the
    // progress monitor never observed that slot, so the blame came from the
    // fault mask alone. Print it explicitly rather than leaving it implied.
    if let Some(slot) = blamed {
        if populated & (1u32 << slot) == 0 {
            dev_info!(
                dev,
                "G17P DM1 monitor checkpoint={} slot={} BLAMED but blank: firmware wrote no progress record for it\n",
                checkpoint,
                slot,
            );
        }
    }
    // The record array offset 0xa40 is DM1's. A blame naming any other data
    // master is not described by these records at all, so say so instead of
    // letting DM1 rows be read as evidence about it.
    let dm_note = match blamed_data_master {
        Some(1) | None => "",
        Some(_) => " (records above are DM1's; the blamed data master is not DM1)",
    };
    dev_info!(
        dev,
        "G17P DM1 monitor checkpoint={} scanned={} populated-mask={:#x} unreadable={} blamed-slot={:?} blamed-data-master={:?}{}\n",
        checkpoint,
        g17_resources::DM1_WATCHDOG_SLOT_COUNT,
        populated,
        failures,
        blamed_slot,
        blamed_data_master,
        dm_note,
    );
}

/// A narrowly scoped, already-halted slot discriminator. Failure to observe
/// it must never prevent the host acknowledgement that resumes recovery.
fn log_g17p_halted_slot(
    dev: &kernel::device::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    registers: Option<&regs::Resources>,
    expected_epoch: u64,
) {
    if module_parameters::g17p_recovery_snapshot.value() == 0 {
        return;
    }
    let Some(registers) = registers else { return };
    let selected = match (
        manager.firmware_recovery_handshake_snapshot(),
        manager.firmware_fault_report_snapshot(),
    ) {
        (Ok(handshake), Ok(report)) => {
            let gate = g17_completion::G17PHaltedSlotGate {
                handshake_state: handshake.state,
                epoch: handshake.epoch,
                expected_epoch,
                report_state: report.state,
                host_requested: report.host_requested,
                slot_present: report.slot_present,
                slot: report.slot,
                data_master_present: report.data_master_present,
                data_master: report.data_master,
            };
            if gate.allows_generic_sample() {
                Some(gate)
            } else {
                None
            }
        }
        _ => None,
    };
    let Some(gate) = selected else { return };
    log_g17p_halted_slot_sample(dev, manager, registers, expected_epoch,
        gate.slot, gate.data_master);
    if gate.data_master != 0 { return; }
    // The firmware waits for our ACK while the runtime lock is held. Keep
    // the host-owned monitor bounds and the hardware's 16-slot bounds separate.
    for slot in 0..g17_resources::DM1_WATCHDOG_SLOT_COUNT.min(16) {
        let Ok(monitor) = manager.firmware_dm1_slot_snapshot(slot) else { continue };
        if !gate.allows_dm1_peer_sample(slot as u32, monitor.state()) { continue; }
        // Never carry a halt authorization into a different recovery epoch.
        let Ok(handshake) = manager.firmware_recovery_handshake_snapshot() else { break };
        if handshake.state != 1 || handshake.epoch != expected_epoch { break; }
        dev_info!(dev,
            "G17P halted secondary DM1 observation: slot={} monitor-timestamp={:#x} (not firmware blame)\n",
            slot, monitor.timestamp());
        log_g17p_halted_slot_sample(dev, manager, registers, expected_epoch, slot as u32, 1);
    }
}

fn log_g17p_halted_slot_sample(
    dev: &kernel::device::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    registers: &regs::Resources,
    expected_epoch: u64,
    slot: u32,
    data_master: u32,
) {
    match registers.g17p_halted_slot_snapshot(slot, data_master) {
        Ok(Some(snapshot)) => {
            let stable = snapshot.state_before == snapshot.state_after;
            if data_master == 0 && stable {
                manager.log_halted_ta_callback(slot);
            }
            dev_info!(
                dev,
                "G17P halted slot={} epoch={} data-master={} gpc={:#x} c020-before={:#018x} c078={:#018x} c020-after={:#018x} stable={} progress-selector={} selector={} key-qid={} key-stamp={:#x}\n",
                slot,
                expected_epoch,
                data_master,
                snapshot.gpc_state,
                snapshot.state_before,
                snapshot.key,
                snapshot.state_after,
                stable,
                g17_completion::g17p_slot_has_progress_selector(snapshot.state_before),
                (snapshot.state_before >> 24) & 3,
                (snapshot.key >> 40) & 0x7f,
                snapshot.key & 0xff_ffff_ffff,
            );
            if let Some(selector) =
                g17_completion::g17p_dm1_monitor_selector_index(snapshot.state_before)
            {
                let (count, names) = if data_master == 0 {
                    (3, "1678/1c0e0/a088")
                } else { (8, "1668/13020/15150/15388/15390/15510/15518/a068") };
                dev_info!(dev,
                    "G17P halted DM{} selected-progress epoch={} selector-index={} c020-stable={} count={}/{} errno={} regs={} raw={:016x?}\n",
                    data_master, expected_epoch, selector, stable,
                    snapshot.progress_count, count, snapshot.progress_errno, names,
                    &snapshot.progress[..snapshot.progress_count]);
            }
        }
        Ok(None) => dev_info!(dev, "G17P halted slot={}: GPC off; no low-SGX access\n", slot),
        Err(error) => dev_warn!(dev, "G17P halted slot={}: read failed ({:?}); continuing recovery\n", slot, error),
    }
}

/// Preserve the fault selected by B1's reason-3 classifier before ACK. This
/// does not use Status-B's copied slot mask as a GMMU-fault predicate.
fn log_g17p_fault_address_candidates(
    dev: &kernel::device::Device,
    manager: &g17_manager::G17PManagerConstruction,
    async_compute_queues: &KVec<G17PAsyncComputeQueue>,
    label: &'static str,
    address_word: u64,
    vm_slot: u8,
) {
    if manager.log_fault_address_candidates(label, address_word, vm_slot) {
        return;
    }

    let matching_context = async_compute_queues
        .iter()
        .find_map(|queue| {
            let context = queue.context.as_deref()?;
            (queue.inflight.is_some() && context.context_id() == u32::from(vm_slot))
                .then_some((context, "active-compute"))
        })
        .or_else(|| {
            async_compute_queues.iter().find_map(|queue| {
                let context = queue.context.as_deref()?;
                (context.context_id() == u32::from(vm_slot))
                    .then_some((context, "retained-compute"))
            })
        });
    if let Some((context, owner)) = matching_context {
        manager.log_compute_fault_address_candidates(
            label,
            address_word,
            vm_slot,
            context,
            owner,
        );
        return;
    }

    dev_info!(
        dev,
        "G17P fault address[{}]: no retained render or compute owner for descriptor context {}\n",
        label,
        vm_slot,
    );
}

fn log_g17p_halted_gmmu(
    dev: &kernel::device::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    async_compute_queues: &KVec<G17PAsyncComputeQueue>,
    registers: Option<&regs::Resources>,
    expected_epoch: u64,
) {
    let Some(registers) = registers else { return };
    let allowed = match (
        manager.firmware_recovery_handshake_snapshot(),
        manager.firmware_fault_report_snapshot(),
    ) {
        (Ok(handshake), Ok(report)) => g17_completion::g17p_halted_gmmu_sample_allowed(
            handshake.state, handshake.epoch, expected_epoch,
            report.state, report.host_requested, report.reason,
        ),
        _ => false,
    };
    if !allowed {
        return;
    }
    match registers.g17p_halted_gmmu_snapshot() {
        Ok(Some(snapshot)) => {
            let stable = snapshot.info_before == snapshot.info_after;
            if let Some(address_word) = snapshot.address_word {
                let address = address_word << 6;
                dev_info!(
                    dev,
                    "G17P halted GMMU reason3 epoch={} gpc={:#x} info-before={:#018x} addr-word={:#018x} va={:#x} info-after={:#018x} stable={} reason={} read={} level={} unit={:#x} vm-slot={}\n",
                    expected_epoch, snapshot.gpc_state, snapshot.info_before,
                    address_word, address, snapshot.info_after, stable,
                    (snapshot.info_before >> 1) & 7,
                    (snapshot.info_before >> 4) & 1,
                    (snapshot.info_before >> 7) & 3,
                    (snapshot.info_before >> 9) & 0xff,
                    (snapshot.info_before >> 17) & 0x3f,
                );
                log_g17p_fault_address_candidates(
                    dev,
                    manager,
                    async_compute_queues,
                    "halted-reason3",
                    address_word,
                    ((snapshot.info_before >> 17) & 0x3f) as u8,
                );
            } else {
                dev_info!(
                    dev,
                    "G17P halted GMMU reason3 epoch={} gpc={:#x} info-before={:#018x} info-after={:#018x} stable={} no-valid-fault; address not read\n",
                    expected_epoch, snapshot.gpc_state,
                    snapshot.info_before, snapshot.info_after, stable,
                );
            }
        }
        Ok(None) => dev_info!(dev, "G17P halted GMMU reason3: GPC off; no low-SGX access\n"),
        Err(error) => dev_warn!(dev, "G17P halted GMMU reason3: read failed ({:?}); continuing recovery\n", error),
    }
}

fn validate_primary_bootstrap_uma_completion(
    pdev: &platform::Device,
    record: &g17_resources::G17PPrimaryFirmwareEventRecord,
) -> Result<g17_manager::G17PBootstrapUmaCompletion> {
    let uma = g17_completion::decode_g17p_firmware_uma_grow_event(&record.raw)
        .map_err(|_| EIO)?;
    if uma.stamp_index != 1
        || g17_submission::g17p_umadesc_offset(uma.pool_index).is_none()
        || uma.shared_control == 0
        || uma.operand_table == 0
        || uma.cookie <= uma.operand_table
        || uma.result != g17_submission::G17P_USC_FREELIST_GROW_DESCRIPTOR_BYTES
    {
        dev_err!(
            pdev.as_ref(),
            "G17P UMA grow: rejected bootstrap completion stamp={} pool={} shared={:#x} operand={:#x} cookie={:#x} result={:#x}\n",
            uma.stamp_index,
            uma.pool_index,
            uma.shared_control,
            uma.operand_table,
            uma.cookie,
            uma.result
        );
        return Err(EIO);
    }
    Ok(g17_manager::G17PBootstrapUmaCompletion {
        stamp: uma.stamp_index,
        hardware_buffer_id: uma.pool_index,
        shared_control: uma.shared_control,
        operand_table: uma.operand_table,
        cookie: uma.cookie,
        count: uma.result,
    })
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct G17PPrimaryFirmwareEventBatch {
    recovery_generation: Option<u64>,
    recovery_records: u32,
    uma_release: Option<g17_manager::G17PBootstrapUmaRelease>,
    blamed_stamp: Option<u8>,
    unknown_records: u32,
    completion_records: u32,
    completion_masks: [u64; 2],
    blamed_recovery_records: u32,
    /// Type-4 records naming a generation the host has already serviced.
    /// Counted and skipped, never a drain failure -- see the type-4 arm.
    stale_recovery_records: u32,
    notification_count: u32,
    control_backpressured: bool,
}

/// Drain the complete primary event-ring batch in native cursor order. Multiple
/// type-4 records for the same host generation are one scheduler restart
/// request; the accepted generation advances only when that coalesced request
/// is serviced.
fn drain_primary_firmware_event_batch(
    pdev: &platform::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    state: &G17PRoleRtkitState,
    announce_device_control: &mut impl FnMut() -> Result,
) -> Result<G17PPrimaryFirmwareEventBatch> {
    let notification_count = state
        .firmware_event_notifications
        .swap(0, Ordering::AcqRel);
    let mut batch = G17PPrimaryFirmwareEventBatch {
        recovery_generation: None,
        recovery_records: 0,
        uma_release: None,
        blamed_stamp: None,
        unknown_records: 0,
        completion_records: 0,
        completion_masks: [0; 2],
        blamed_recovery_records: 0,
        stale_recovery_records: 0,
        notification_count,
        control_backpressured: false,
    };
    let role = match state.processor {
        G17PProcessor::Gfx => g17_initdata::InstanceRole::Primary,
        G17PProcessor::Gfx1 => g17_initdata::InstanceRole::Secondary,
    };
    for _ in 0..g17_completion::G17P_FIRMWARE_EVENT_CAPACITY {
        let Some(record) = manager.drain_firmware_event(role)? else { break; };
        let event_type = u32::from_le_bytes(record.raw[0..4].try_into().map_err(|_| EIO)?);
        match event_type {
            g17_completion::G17P_FIRMWARE_EVENT_RECOVERY_TYPE => {
                let recovery =
                    g17_completion::decode_g17p_firmware_recovery_event(&record.raw)
                        .map_err(|_| EIO)?;
                dev_info!(
                    pdev.as_ref(),
                    "G17P event worker: type4 generation={} cursor={}->{} producer={}\n",
                    recovery.generation,
                    record.consumer_before,
                    record.consumer_published,
                    record.producer_snapshot
                );
                let accepted = manager.accepted_firmware_recovery_generation();
                if recovery.generation < accepted {
                    // A restart the host has ALREADY serviced. Both RTKit roles
                    // see one firmware request, so the second ring's copy is
                    // routinely read after `accept_empty_primary_firmware_recovery`
                    // has advanced the host generation past it. Failing the
                    // drain here was fatal twice over: the caller turns any
                    // error into `crashed`, and aborting the batch strands every
                    // later record in the ring -- the exact stall the
                    // stamp-blamed arm above and the `unknown` arm below were
                    // both already fixed for. Count it, skip the bookkeeping,
                    // keep draining. There is no restart to service and both
                    // command gates were released by the real one, so the
                    // type-4 `break` does not apply either.
                    batch.stale_recovery_records =
                        batch.stale_recovery_records.saturating_add(1);
                    if batch.stale_recovery_records <= 4 {
                        dev_warn!(
                            pdev.as_ref(),
                            "G17P event worker: type4 generation={} is behind the accepted host generation {}; already serviced, skipping\n",
                            recovery.generation,
                            accepted
                        );
                    }
                    continue;
                }
                if recovery.stamp_index != -1 {
                    // Attribute a stamp only after accepting this recovery
                    // generation. A delayed copy from the other RTKit role may
                    // carry a numeric QID that has since changed logical owners;
                    // treating that stale record as current blame poisons the
                    // replacement queue's lifetime decision.
                    let stamp = u8::try_from(recovery.stamp_index).map_err(|_| EIO)?;
                    if stamp >= 128 {
                        return Err(EIO);
                    }
                    dev_err!(
                        pdev.as_ref(),
                        "G17P recovery: firmware requests stamp-blamed recovery generation={} stamp={}; servicing\n",
                        recovery.generation,
                        recovery.stamp_index
                    );
                    batch.blamed_recovery_records =
                        batch.blamed_recovery_records.saturating_add(1);
                    batch.blamed_stamp = Some(stamp);
                    // v96: a paused pool allocator (unanswered event 6) stalls
                    // every render using it. Record grow state before recovery
                    // settles, drops stale requests or returns pairs.
                    manager.log_render_grow_state("stamp-blamed-recovery");
                    // v97d: did the host-side 3D stall watch see this coming?
                    if crate::g17_recovery_path::enabled(crate::g17_recovery_path::STALL_WATCH) {
                        let open = crate::g17_recovery_path::note_recovery_for_stalls();
                        dev_err!(pdev.as_ref(),
                            "G17P 3D stall watch at recovery: open-stalls(pair,stage,3d-completed)={:x?}\n",
                            &open.pairs[..open.count]);
                    }
                } else {
                    manager.log_render_grow_state("recovery");
                }
                if let Err(error) =
                    manager.validate_empty_primary_firmware_recovery(recovery.generation)
                {
                    // Ahead of the accepted generation: the host and the
                    // firmware genuinely disagree about how many restarts have
                    // been served, which is not something to drain past.
                    dev_err!(
                        pdev.as_ref(),
                        "G17P event worker: type4 generation={} is AHEAD of the accepted host generation {} ({:?})\n",
                        recovery.generation,
                        accepted,
                        error
                    );
                    return Err(error);
                }
                if batch
                    .recovery_generation
                    .is_some_and(|generation| generation != recovery.generation)
                {
                    dev_err!(
                        pdev.as_ref(),
                        "G17P recovery: one IRQ batch contains different generations {:?} and {}\n",
                        batch.recovery_generation,
                        recovery.generation
                    );
                    return Err(EIO);
                }
                batch.recovery_generation = Some(recovery.generation);
                batch.recovery_records = batch.recovery_records.checked_add(1).ok_or(EOVERFLOW)?;
                // Type4 schedules restartWorkQueue on a separate gate turn.
                // Do not consume the following ring record until that turn
                // has fully returned and released both native command gates.
                break;
            }
            g17_completion::G17P_FIRMWARE_EVENT_TVB_GROW_TYPE => {
                let request = match g17_completion::G17PFirmwareTvbGrowEvent::decode(&record.raw) {
                    Ok(request) => request,
                    Err(error) => {
                        dev_warn!(pdev.as_ref(), "G17P TVB grow: malformed request ({:?})\n", error);
                        // Native asserts here. Stop event service through the
                        // caller's existing failure path; a malformed request
                        // cannot safely identify an allocation or its reply.
                        return Err(EIO);
                    }
                };
                // v96 handshake step 1: firmware event-ring arrival (raw).
                let arrivals = GROW_ARRIVALS.fetch_add(1, Ordering::Relaxed) + 1;
                if crate::g17_recovery_path::log_budget(arrivals, 64, 256) {
                    dev_info!(pdev.as_ref(),
                        "G17P TVB grow event #{}: cursor={}->{} producer={} raw={:02x?}\n",
                        arrivals, record.consumer_before, record.consumer_published,
                        record.producer_snapshot, &record.raw[..0x1c.min(record.raw.len())]);
                }
                match manager.queue_render_tvb_grow(request)? {
                    g17_manager::G17PTvbGrowDisposition::IgnoredStale => {}
                    g17_manager::G17PTvbGrowDisposition::PreparationDeferred => {}
                    g17_manager::G17PTvbGrowDisposition::Published => {
                        announce_device_control()?;
                    }
                    g17_manager::G17PTvbGrowDisposition::Deferred => {
                        batch.control_backpressured = true;
                        break;
                    }
                }
            }
            g17_completion::G17P_FIRMWARE_EVENT_UMA_GROW_TYPE => {
                let completion = validate_primary_bootstrap_uma_completion(pdev, &record)?;
                manager
                    .match_render_usc_freelist_completion(completion)
                    .map_err(|error| {
                        dev_err!(
                            pdev.as_ref(),
                            "G17P UMA grow: type13 has no matching live FList owner ({:?})\n",
                            error
                        );
                        error
                    })?;
                dev_info!(
                    pdev.as_ref(),
                    "G17P UMA grow: matched retained FList owner stamp={} ID={} cookie={:#x} count={:#x}; retaining TA/3D references until paired completion\n",
                    completion.stamp,
                    completion.hardware_buffer_id,
                    completion.cookie,
                    completion.count,
                );
            }
            g17_completion::G17P_FIRMWARE_EVENT_COMPLETION_TYPE => {
                // Type 1 is published exactly once per executed submission, but
                // only its first 0x18 bytes are real: everything beyond that is
                // the 8-byte repeating poison "RTKSTACK". An earlier reading of
                // uuid/context/stamp at +0x28..+0x40 was KTrace spill into the
                // same pages -- with KTrace disabled those offsets are pure
                // poison, which is how the misread was caught. Log the real
                // prefix only; do NOT synthesise a completion from bytes whose
                // encoding is not yet established.
                let qword = |offset: usize| {
                    u64::from_le_bytes(record.raw[offset..offset + 8].try_into().unwrap_or([0; 8]))
                };
                if crate::util::g17p_verbose() {
                    dev_info!(
                        pdev.as_ref(),
                        "G17P completion record: head={:#x} {:#x} {:#x} cursor={}->{} producer={}\n",
                        qword(0x00),
                        qword(0x08),
                        qword(0x10),
                        record.consumer_before,
                        record.consumer_published,
                        record.producer_snapshot
                    );
                }
                batch.completion_records = batch.completion_records.saturating_add(1);
                let event = g17_completion::decode_g17p_firmware_stamp_event(&record.raw)
                    .map_err(|_| EIO)?;
                batch.completion_masks[0] |= event.masks[0];
                batch.completion_masks[1] |= event.masks[1];
            }
            g17_completion::G17P_FIRMWARE_EVENT_PROCESS_EXIT_TYPE => {
                // The answer to a process-exit kill (device control 0x29).
                // Record the cookie; the teardown turn that follows this
                // drain matches it to its queue (`release_exit_killed_compute`).
                // Never fatal: an unreadable record only leaves its queue
                // retained, exactly as without the kill.
                let cookie = g17_completion::decode_g17p_process_exit_event(&record.raw).unwrap_or(0);
                let noted = crate::g17_exit_kill::note_ack(cookie);
                dev_info!(
                    pdev.as_ref(),
                    "G17P event worker: type12 process-exit complete cookie={} cursor={}->{} producer={}{}\n",
                    cookie,
                    record.consumer_before,
                    record.consumer_published,
                    record.producer_snapshot,
                    if noted { "" } else { " (no free acknowledgement slot; its queue stays retained)" },
                );
            }
            unknown => {
                // Only recovery (type 4) and UMA-grow (type 13) were ever
                // reachable while the firmware never executed work. Now that a
                // kick actually runs, the firmware publishes further record
                // kinds here; rejecting them with EIO aborted the whole drain
                // and left the submission unretired. Log the record and keep
                // draining -- `drain_primary_firmware_event` has already
                // published the consumer cursor for it.
                batch.unknown_records = batch.unknown_records.saturating_add(1);
                if batch.unknown_records <= 4 {
                    dev_info!(
                        pdev.as_ref(),
                        "G17P event worker: unhandled firmware event type={} cursor={}->{} producer={} raw={:02x?}\n",
                        unknown,
                        record.consumer_before,
                        record.consumer_published,
                        record.producer_snapshot,
                        &record.raw[..32.min(record.raw.len())]
                    );
                }
            }
        }
    }
    if batch.unknown_records != 0 {
        dev_info!(
            pdev.as_ref(),
            "G17P event worker: {} unhandled firmware event record(s) this batch\n",
            batch.unknown_records
        );
    }
    if batch.recovery_records != 0 || batch.stale_recovery_records != 0 {
        dev_info!(
            pdev.as_ref(),
            "G17P event worker: notifications={} type4={} stale-type4={} generation={:?}\n",
            batch.notification_count,
            batch.recovery_records,
            batch.stale_recovery_records,
            batch.recovery_generation,
        );
    }
    Ok(batch)
}

/// Service one coalesced firmware-initiated recovery before Linux has
/// published user work. Native increments its host generation once for the
/// scheduled restart, even if the event-ring batch contained duplicates.
fn complete_primary_real_recovery(
    pdev: &platform::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    primary_queue: Option<&mut g17_manager::G17PSksmQueue>,
    async_compute_queues: &mut KVec<G17PAsyncComputeQueue>,
    registers: Option<&regs::Resources>,
    state: &G17PRoleRtkitState,
    generation: u64,
    duplicate_records: u32,
) -> Result {
    let handshake = manager.firmware_recovery_handshake_snapshot()?;
    if handshake.state != 1 {
        dev_err!(
            pdev.as_ref(),
            "G17P recovery: type-4 generation={} arrived at shared epoch={} state={}, expected state 1\n",
            generation,
            handshake.epoch,
            handshake.state
        );
        return Err(EIO);
    }
    manager.validate_empty_primary_firmware_recovery(generation)?;
    // Recovery isolation classifies against THIS recovery's table only.
    crate::g17_recovery_isolation::clear_entries();
    crate::g17_recovery_isolation::clear_sources();
    // The scheduler-empty precondition is UNSATISFIABLE for a render-blamed
    // recovery. The firmware raises the restart request during the render
    // wait, so `render_storage`/`render_tracker` are populated by construction
    // and this returns EBUSY every time -- measured, after the inline drain
    // started reaching it. The guard was written when only compute could
    // trigger a recovery, and for compute the scheduler genuinely was empty.
    //
    // The precedent for relaxing it is a few lines below, in this same
    // function: the non-empty recovery-info table used to abort here too,
    // until it was understood that "refusing to clear is strictly worse than
    // clearing" -- refusing leaves the firmware pinned waiting for the host
    // forever. The same argument holds exactly: never answering leaves KSM
    // halted, which is the ETIMEDOUT.
    //
    // What the guard actually protects is the generation-0 SKSM timestamp
    // wipe, which would erase a live render's progress markers. So scope the
    // refusal to that wipe rather than to the whole handshake: acknowledge and
    // clear regardless, skip the wipe while render state is live.
    let scheduler_active = manager.render_scheduler_active();
    if scheduler_active {
        if crate::g17_config::RECOVERY_ALLOW_ACTIVE_SCHEDULER == 0 {
            manager.validate_empty_primary_firmware_recovery_scheduler()?;
        }
        dev_warn!(
            pdev.as_ref(),
            "G17P recovery: servicing generation={} with a LIVE render scheduler; the generation-0 SKSM timestamp wipe is skipped so the render's progress markers survive\n",
            generation
        );
    }
    dev_info!(
        pdev.as_ref(),
        "G17P recovery: servicing generation={} coalesced-records={} shared-epoch={} state={}\n",
        generation,
        duplicate_records,
        handshake.epoch,
        handshake.state
    );
    // The firmware's own fault report is bundle view 3 -- HOST memory we
    // already own -- and it is complete before the firmware writes 1 into
    // status_b[0x4900], i.e. before this ack. Reading it here costs nothing
    // and names the recovery's reason: 3 is the GMMU page fault (b1 0x5f1c),
    // 2 and 4 are the other two arms. Every prior sample was taken at
    // pre-0x0a/post-0x10, both strictly BEFORE the fault existed, which is
    // why every report so far read reason=0.
    //
    // The faulting VA is not copied into any host-DRAM recovery structure.
    // macOS reads it from the requestor-selected 0xd8c0/0xd8c8 window while
    // firmware is halted at state 1, before the host's 1 -> 2 acknowledge.
    // Match that exact window below, guarded by the always-on GPC state.
    log_g17p_fault_report(pdev.as_ref(), manager, "recovery-blamed-cause");
    // Compute-only tails need a drain here too: the last render checkpoint
    // may precede the very kicks that faulted. Capture before acknowledgement
    // lets firmware mutate the halted state. This is a no-op unless tracing
    // was explicitly enabled before boot; it changes no scheduling policy.
    probe_g17p_ktrace(pdev.as_ref(), manager, "fault-before-ack");
    probe_g17p_recovery_report_history(pdev.as_ref(), manager);
    log_g17p_halted_slot(pdev.as_ref(), manager, registers, handshake.epoch);
    log_g17p_halted_gmmu(
        pdev.as_ref(),
        manager,
        async_compute_queues,
        registers,
        handshake.epoch,
    );
    let cause = manager.primary_firmware_recovery_cause_snapshot()?;
    // Recovery isolation bit5: sample every fault-mask slot while firmware is
    // still halted (keys and DM1 progress records are historical DRAM).
    crate::g17_recovery_isolation::clear_sources();
    if crate::g17_recovery_isolation::enabled(crate::g17_recovery_isolation::ATTRIBUTION) {
        let mut keys = [0u64; crate::g17_recovery_isolation::MAX_SOURCES];
        let mut monitors = [0u32; crate::g17_recovery_isolation::MAX_SOURCES];
        for slot in 0..crate::g17_recovery_isolation::MAX_SOURCES {
            if (cause.slot_mask as u64) & (1u64 << slot) == 0 { continue; }
            keys[slot] = g17_manager::G17PManagerConstruction::firmware_ksm_slot_key(slot).unwrap_or(0);
            monitors[slot] = manager.firmware_dm1_slot_snapshot(slot)
                .map(|snapshot| snapshot.state()).unwrap_or(0);
        }
        let reason = manager.firmware_fault_report_snapshot().map(|report| report.reason).unwrap_or(0);
        crate::g17_recovery_isolation::store_sources(reason, cause.slot_mask as u32, &keys, &monitors);
    }
    let slot_mask_population = cause.slot_mask.count_ones();
    let lowest_blamed_slot = match cause.slot_mask {
        0 => None,
        mask => Some(mask.trailing_zeros()),
    };
    dev_info!(
        pdev.as_ref(),
        "G17P recovery cause: zero0={:#x} detected-source-count={} zero1={:#x} command-word={:#x} raw48cc={:#x} raw48d0={:#x} dm-slot-mask={:#x} (slots={} lowest={:?}) host-recovery={}\n",
        cause.zero_0,
        cause.detected_source_count,
        cause.zero_1,
        cause.command_word,
        cause.raw_48cc,
        cause.raw_48d0,
        cause.slot_mask,
        slot_mask_population,
        lowest_blamed_slot,
        cause.host_recovery
    );
    // Firmware is halted at recovery state 1, so this is the last point at
    // which the queue state that caused the watchdog can be sampled without
    // racing firmware or losing it to the acknowledgement below. Keep this on
    // the recovery path: none of these reads belong in normal submission.
    for queue in async_compute_queues.iter_mut().filter(|queue| queue.inflight.is_some()) {
        for active in queue.inflight.iter() {
            let packet = &active.packet;
            let command = packet.command();
            let timestamps = packet.completion()
                .and_then(|completion| completion.halted_timestamp_snapshot());
            dev_err!(
                pdev.as_ref(),
                "G17P recovery active-compute: owner={:?} qid={} submission={} context={} vm={} root={:#x} command-images={} input-fences={} usc={:#x} stream=[{:#x},{:#x}) terminator={:#x} samplers={} attachments={} add3={} scs={:#x} timestamps={:x?}\n",
                queue.owner,
                queue.queue.queue_id(),
                packet.submission_id(),
                packet.context().context_id(),
                packet.context().vm_id(),
                packet.context().root(),
                packet.command_images().len(),
                packet.input_fences().len(),
                command.usc_exec_base,
                command.control_stream_base,
                command.control_stream_end,
                command.control_stream_terminator,
                command.sampler_count,
                command.attachments.as_slice().len(),
                command.g17p_add3_buffers.is_some(),
                command.scs_layout,
                timestamps,
            );
            let mut stream = [0u8; 512];
            let length = command.control_stream_end
                .saturating_sub(command.control_stream_base)
                .min(stream.len() as u64) as usize;
            match packet.context().vm().read_bytes(
                command.control_stream_base, &mut stream[..length],
            ) {
                Ok(()) => {
                    for (index, bytes) in stream[..length].chunks(16).enumerate() {
                        dev_err!(
                            pdev.as_ref(),
                            "G17P recovery CDM: qid={} offset={:#x} bytes={:02x?}\n",
                            queue.queue.queue_id(), index * 16, bytes,
                        );
                    }
                }
                Err(error) => dev_err!(
                    pdev.as_ref(),
                    "G17P recovery CDM: qid={} read failed {:?}\n",
                    queue.queue.queue_id(), error,
                ),
            }
        }
        manager.log_async_compute_timeout(&mut queue.queue);
    }
    // `detected-source-count` must equal the mask population and the blamed
    // slot must be its lowest set bit. When they disagree the mask is not the
    // whole story for this fault, which is worth one line to know.
    if slot_mask_population != cause.detected_source_count {
        dev_warn!(
            pdev.as_ref(),
            "G17P recovery cause: source count {} does not match dm-slot-mask population {} ({:#x})\n",
            cause.detected_source_count,
            slot_mask_population,
            cause.slot_mask,
        );
    }
    if cause.slot_mask != 0 && module_parameters::g17p_fault_report.value() != 0 {
        let mut faulting_address = None;
        if let Some(registers) = registers {
            match registers.g17p_render_fault_irq_snapshot() {
                Ok(Some(snapshot)) => {
                    if snapshot.fault_info & 1 != 0 {
                        faulting_address = Some((
                            snapshot.fault_addr_word,
                            ((snapshot.fault_info >> 17) & 0x3f) as u8,
                        ));
                    }
                    dev_info!(
                    pdev.as_ref(),
                    "G17P fault IRQ @recovery-state-1: gpc-state={:#x} info={:#018x} addr-word={:#018x} gate={:#018x} sub-status={:#018x} irq-status={:#018x} bit35={}\n",
                    snapshot.gpc_state,
                    snapshot.fault_info,
                    snapshot.fault_addr_word,
                    snapshot.requestor_gate,
                    snapshot.sub_status,
                    snapshot.irq_status,
                    (snapshot.irq_status >> 35) & 1,
                    )
                }
                Ok(None) => dev_warn!(
                    pdev.as_ref(),
                    "G17P fault IRQ @recovery-state-1: skipped because gpc-state=0\n"
                ),
                Err(error) => dev_warn!(
                    pdev.as_ref(),
                    "G17P fault IRQ @recovery-state-1: snapshot failed ({:?})\n",
                    error,
                ),
            }
            if let Err(error) =
                registers.g17p_log_fault_requestors_pre_ack("recovery-state-1")
            {
                dev_warn!(
                    pdev.as_ref(),
                    "G17P recovery: pre-ack fault-requestor sweep failed ({:?})\n",
                    error
                );
            }
        }
        if let Some((addr_word, vm_slot)) = faulting_address {
            log_g17p_fault_address_candidates(
                pdev.as_ref(),
                manager,
                async_compute_queues,
                "recovery-state-1",
                addr_word,
                vm_slot,
            );
        }
    }
    if scheduler_active {
        manager.log_render_recovery_pre_ack("recovery-state-1");
    }
    if cause.host_recovery != 0 {
        dev_err!(
            pdev.as_ref(),
            "G17P recovery: event-backed path has host-recovery flag {}\n",
            cause.host_recovery
        );
        return Err(EIO);
    }
    let accepted_generation = manager.accept_empty_primary_firmware_recovery(generation)?;
    dev_info!(
        pdev.as_ref(),
        "G17P recovery: accepted host generation {} before empty scheduler recovery\n",
        accepted_generation
    );

    // Only the first recovery of a session may wipe the SKSM last-submitted
    // timestamps. Doing it on every recovery erases the queue's progress and
    // makes the firmware immediately request another one: observed on J700 as
    // 105 identical fault-free recoveries (cause word 0x3, no fault, no
    // detected source) inside a single 625ms submit window.
    if generation == 0 && !scheduler_active {
        if let Err(error) = manager.dump_primary_recovery_cause_block() {
            dev_warn!(pdev.as_ref(), "G17P recovery: cause-block dump failed ({:?})\n", error);
        }
        manager.reset_empty_sksm_last_submitted_hw_timestamps()?;
        dev_info!(
            pdev.as_ref(),
            "G17P recovery: published 128 invalid/zero last-submitted HW timestamps\n"
        );
    }
    // Full binary observation while the validated state-1 halt still holds.
    // The archive owns copies; exposing it does not expose mutable GPU memory.
    for slot in 0..g17_resources::G17PRenderQueuePair::SLOT_COUNT {
        if manager.render_slot_allocated(slot) {
            finish_g17p_render_trace(pdev.as_ref(), manager, slot, crate::g17_trace_capture::Phase::RecoveryBeforeAck);
        }
    }
    // Native restartWorkQueue sets every work queue's recovery target while
    // firmware is still halted, before resuming it. After the read-only
    // captures above, so they keep observing the faulted state.
    if module_parameters::g17p_recovery_target_restart.value() != 0 {
        publish_g17p_recovery_targets(pdev, manager, primary_queue, async_compute_queues, generation);
    }
    manager.acknowledge_primary_firmware_recovery(1, 2)?;
    let mut post_state = 2;
    for _ in 0..FIRMWARE_RECOVERY_POLLS {
        post_state = manager.firmware_recovery_handshake_snapshot()?.state;
        if post_state == 3 {
            break;
        }
        if state.crashed.load(Ordering::Acquire) {
            return Err(EIO);
        }
        fsleep(Delta::from_millis(1));
    }
    if post_state != 3 {
        dev_err!(
            pdev.as_ref(),
            "G17P recovery: firmware stopped at state {}, expected post-recovery state 3\n",
            post_state
        );
        return Err(ETIMEDOUT);
    }
    let recovery_info = manager.primary_firmware_recovery_info_snapshot()?;
    // Recovery isolation: keep the decoded table for classification after the
    // final clear (which may reset it). A read failure leaves it empty, and an
    // empty table only removes the guilty-bit source, never adds innocence.
    if crate::g17_recovery_isolation::enabled(crate::g17_recovery_isolation::INNOCENT) {
        let mut raw = [(0u32, 0u32); crate::g17_recovery_isolation::MAX_ENTRIES];
        match manager.primary_recovery_info_entries(&mut raw) {
            Ok(used) => crate::g17_recovery_isolation::store_entries(&raw[..used]),
            Err(error) => dev_warn!(pdev.as_ref(),
                "G17P recovery isolation: recovery-info read failed ({:?}); guilty set from the blamed stamp only\n",
                error),
        }
    }
    dev_info!(
        pdev.as_ref(),
        "G17P recovery info: emitted={} valid={} first-valid-index={:?} first-valid-flags={:#x}\n",
        recovery_info.emitted_entry_count,
        recovery_info.valid_entry_count,
        recovery_info.first_valid_index,
        recovery_info.first_valid_flags
    );
    // A non-empty recovery-info table used to abort here, back when only the
    // empty prepublication case was understood. That left the firmware pinned
    // at state 3 ("waiting for final clear") forever, because the final
    // acknowledge below is what clears it. On J700 the firmware emits exactly
    // one valid entry on the first real submission, with no fault recorded
    // (fault-status 0, host-recovery 0), so refusing to clear is strictly
    // worse than clearing: log the table and complete the handshake.
    if recovery_info.emitted_entry_count != 0 || recovery_info.valid_entry_count != 0 {
        dev_warn!(
            pdev.as_ref(),
            "G17P recovery: completing final clear with a non-empty recovery-info table (emitted={} valid={} first-index={:?} first-flags={:#x})\n",
            recovery_info.emitted_entry_count,
            recovery_info.valid_entry_count,
            recovery_info.first_valid_index,
            recovery_info.first_valid_flags
        );
        // The generation-0 dump above runs BEFORE the firmware reaches state
        // 3, so it always caught this table empty. Re-dump it now that the
        // firmware has published: each entry is 8 bytes and the driver's
        // snapshot only decodes the first word, so the second word of entry 0
        // -- the half that plausibly names what was blamed -- has never been
        // looked at. Host memory only; no MMIO.
        if let Err(error) = manager.dump_primary_recovery_cause_block() {
            dev_warn!(
                pdev.as_ref(),
                "G17P recovery: populated cause-block dump failed ({:?})\n",
                error
            );
        }
    }
    manager.acknowledge_primary_firmware_recovery(3, 0)?;
    let completed = manager.firmware_recovery_handshake_snapshot()?;
    dev_info!(
        pdev.as_ref(),
        "G17P recovery: final clear acknowledged; state={} epoch={}\n",
        completed.state,
        completed.epoch
    );
    if completed.state != 0 {
        return Err(EIO);
    }
    Ok(())
}

/// `AGXAccelerator::restartWorkQueue` ("GPU Hang - Channel Restart") copies
/// each live work queue's uncached producer to its recovery target before the
/// restarted firmware replays outstanding work: TA and 3D channels of every
/// AGX3DWorkQueue and the channel of every AGXCLWorkQueue. Here: every render
/// pair carrying a graph, the primary CL queue and every async CL queue. See
/// `g17p_publish_recovery_target` for the firmware consumer.
///
/// Native skips the walk for restart reasons 2, 4, 5 and 6 (byte +8 of its
/// accelerator+0x18dc0 restart record); the firmware source of that byte is
/// not decoded, so Linux applies it on every serviced type-4 recovery. A
/// failed copy is reported and leaves that channel's previous target.
fn publish_g17p_recovery_targets(
    pdev: &platform::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    primary_queue: Option<&mut g17_manager::G17PSksmQueue>,
    async_compute_queues: &mut KVec<G17PAsyncComputeQueue>,
    generation: u64,
) {
    let (render, render_failure) = manager.publish_render_recovery_targets();
    let mut compute = 0u32;
    let mut compute_failure = None;
    for queue in Iterator::chain(primary_queue.into_iter(),
        async_compute_queues.iter_mut().map(|queue| &mut queue.queue))
    {
        match queue.publish_recovery_target() {
            Ok(_) => compute += 1,
            Err(error) => {
                if compute_failure.is_none() { compute_failure = Some((queue.queue_id(), error)); }
            }
        }
    }
    // Native orders the copies before the stamp table and the resume with
    // `dmb ish`; the acknowledge that follows is the resume.
    fence(Ordering::SeqCst);
    dev_info!(
        pdev.as_ref(),
        "G17P recovery: generation={} recovery targets set to producers for {} render pair(s) and {} CL queue(s) before resume (native restartWorkQueue)\n",
        generation,
        render,
        compute,
    );
    if let Some((slot, error)) = render_failure {
        dev_warn!(pdev.as_ref(),
            "G17P recovery: render pair slot {} recovery target not published ({:?})\n", slot, error);
    }
    if let Some((qid, error)) = compute_failure {
        dev_warn!(pdev.as_ref(),
            "G17P recovery: CL qid {} recovery target not published ({:?})\n", qid, error);
    }
}

static INLINE_RECOVERIES_SERVICED: AtomicU32 = AtomicU32::new(0);

/// Offer only a sealed first-render archive through the established coredump
/// mechanism. Existing coredumps can occupy the device slot; the host must
/// verify the G17TRC01 magic rather than assume every devcd contains this data.
fn finish_g17p_render_trace(
    dev: &kernel::device::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    render_slot: u8,
    phase: crate::g17_trace_capture::Phase,
) {
    #[cfg(CONFIG_DEV_COREDUMP)]
    match manager.finish_render_trace(render_slot, phase) {
        Ok(Some(capture)) => {
            let size = capture.len();
            devcoredump::dev_coredump(dev, kernel::module::this_module::<crate::LocalModule>(), capture, GFP_KERNEL,
                msecs_to_jiffies(60 * 60 * 1000));
            dev_info!(dev, "G17P binary capture: sealed {} bytes phase={:?}; offered to devcoredump (G17TRC01)\n", size, phase);
        }
        Ok(None) => {}
        Err(error) => dev_warn!(dev, "G17P binary capture finalization failed: {:?}\n", error),
    }
}

fn settle_primary_empty_recovery(
    pdev: &platform::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    state: &G17PRoleRtkitState,
    announce_device_control: &mut impl FnMut() -> Result,
) -> Result<(Option<g17_manager::G17PBootstrapUmaRelease>, [u64; 2])> {
    let mut uma_release = None;
    let mut completion_masks = [0u64; 2];
    for _ in 0..g17_completion::G17P_FIRMWARE_EVENT_CAPACITY {
        let batch = drain_primary_firmware_event_batch(pdev, manager, state, announce_device_control)?;
        completion_masks[0] |= batch.completion_masks[0];
        completion_masks[1] |= batch.completion_masks[1];
        if let Some(release) = batch.uma_release {
            if uma_release.replace(release).is_some() {
                return Err(EIO);
            }
        }
        if let Some(generation) = batch.recovery_generation {
            // Coalesce rather than refuse: one firmware restart request can be
            // observed more than once (a second drain pass here, or the gfx1
            // ring in the same worker pass), and the caller turns the bare
            // EBUSY into a permanently crashed session.
            manager.queue_or_coalesce_primary_recovery(
                generation,
                batch.recovery_records,
                batch.blamed_stamp,
            )?;
            return Ok((uma_release, completion_masks));
        }
        if batch.control_backpressured {
            return Ok((uma_release, completion_masks));
        }
        // v87 STATE_WORD: states 0 and 1 need only the state word; any other
        // state takes the full snapshot for the (unchanged) error report.
        let state = if crate::g17_host_path::enabled(crate::g17_host_path::STATE_WORD) {
            manager.firmware_recovery_state()?
        } else {
            manager.firmware_recovery_handshake_snapshot()?.state
        };
        if state == 0 {
            return Ok((uma_release, completion_masks));
        }
        if state == 1 {
            // Firmware publishes type4 before state 1, but the producer can
            // race this snapshot. Reload without running recovery inline.
            continue;
        }
        let handshake = manager.firmware_recovery_handshake_snapshot()?;
        dev_err!(
            pdev.as_ref(),
            "G17P event worker: shared epoch={} state={} has no schedulable type-4 record\n",
            handshake.epoch,
            handshake.state
        );
        return Err(EIO);
    }
    Err(EIO)
}

fn log_g17p_scheduler_gate(
    dev: &kernel::device::Device,
    registers: &regs::Resources,
    checkpoint: &'static str,
) {
    match registers.g17p_scheduler_gate_registers() {
        Ok(snapshot) => dev_info!(
            dev,
            "G17P scheduler gate checkpoint={} fender={:#x} armed={} gpc-state={:#x} host-irq-enable={:#x} host-irq-status={:#x}\n",
            checkpoint,
            snapshot.fender_dynamic_gating,
            snapshot.fender_wake_armed(),
            snapshot.host_irq_summary,
            snapshot.host_irq_enable,
            snapshot.host_irq_status,
        ),
        Err(error) => dev_err!(
            dev,
            "G17P scheduler gate checkpoint={} read failed ({:?})\n",
            checkpoint,
            error,
        ),
    }
}

const fn g17p_rtkit_uat_address(dva: u64) -> u64 {
    dva & G17P_RTKIT_UAT_DVA_MASK
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u32)]
enum G17PProcessor {
    Gfx = 0,
    Gfx1 = 1,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct G17PFirmwareCrashBuffer {
    dva: u64,
    size: usize,
}

impl G17PProcessor {
    const fn name(self) -> &'static str {
        match self {
            Self::Gfx => "GFX",
            Self::Gfx1 => "GFX1",
        }
    }

    const fn firmware_crash_buffer(self) -> G17PFirmwareCrashBuffer {
        // Both endpoint-1 buffer requests are stable across the retained
        // G17P captures. Message 0x0010810001e1c000 decodes to the primary
        // DVA below and size 0x8000; GFX1 advertises the second DVA with the
        // same size.
        match self {
            Self::Gfx => G17PFirmwareCrashBuffer {
                dva: 0x100_01e1_c000,
                size: 0x8000,
            },
            Self::Gfx1 => G17PFirmwareCrashBuffer {
                dva: 0x100_01f5_0000,
                size: 0x8000,
            },
        }
    }
}

const fn bounded_crashlog_size(size: usize) -> usize {
    if size < G17P_CRASHLOG_MAX_BYTES {
        size
    } else {
        G17P_CRASHLOG_MAX_BYTES
    }
}

const _: () = {
    assert!(bounded_crashlog_size(0) == 0);
    assert!(bounded_crashlog_size(4096) == 4096);
    assert!(bounded_crashlog_size(G17P_CRASHLOG_MAX_BYTES + 1) == G17P_CRASHLOG_MAX_BYTES);
    let gfx = G17PProcessor::Gfx.firmware_crash_buffer();
    let gfx1 = G17PProcessor::Gfx1.firmware_crash_buffer();
    assert!(gfx.dva == 0x100_01e1_c000 && gfx1.dva == 0x100_01f5_0000);
    assert!(gfx.dva == crate::g17_adt_j700::J700_GFX_DATA.base + 0xa_0000);
    assert!(gfx1.dva == crate::g17_adt_j700::J700_GFX1_DATA.base + 0xa_0000);
    assert!(g17p_rtkit_uat_address(gfx.dva) == 0x01e1_c000);
    assert!(g17p_rtkit_uat_address(gfx1.dva) == 0x01f5_0000);
    assert!(gfx.size == 0x8000 && gfx1.size == 0x8000);
    assert!(gfx.dva & (mmu::UAT_PGSZ as u64 - 1) == 0);
    assert!(gfx1.dva & (mmu::UAT_PGSZ as u64 - 1) == 0);
    assert!(gfx.dva + gfx.size as u64 <= 1u64 << 42);
    assert!(gfx1.dva + gfx1.size as u64 <= 1u64 << 42);
};

/// Load-time boundary for short, independent hardware experiments.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum G17PTestStage {
    Control = 0,
    Qid4 = 1,
    ComputeUapi = 2,
}

impl G17PTestStage {
    fn from_parameter(value: u32) -> Result<Self> {
        match value {
            0 => Ok(Self::Control),
            1 => Ok(Self::Qid4),
            2 => Ok(Self::ComputeUapi),
            _ => Err(EINVAL),
        }
    }

    const fn configures_qid4(self) -> bool {
        !matches!(self, Self::Control)
    }

    /// Only the explicit QID4 diagnostic prepares a queue without a submit.
    /// Normal render/compute UAPI sessions allocate it on first compute use.
    const fn prepares_qid4_at_startup(self) -> bool {
        matches!(self, Self::Qid4)
    }

    const fn enables_compute_uapi(self) -> bool {
        matches!(self, Self::ComputeUapi)
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Qid4 => "QID4",
            Self::ComputeUapi => "compute UAPI",
        }
    }
}

/// Per-role callback state retained until the corresponding RTKit client is
/// dropped. The callback only publishes atomics; probe owns every mutation of
/// manager and queue state.
struct G17PRoleRtkitState {
    dev: ARef<AsahiDevice>,
    processor: G17PProcessor,
    firmware_contexts: mmu::FirmwareContextTableReader,
    #[cfg(CONFIG_DEV_COREDUMP)]
    crash_capture: Arc<Mutex<G17PCrashCaptureState>>,
    acked: AtomicBool,
    crashed: AtomicBool,
    runtime_events: AtomicU32,
    firmware_event_notifications: AtomicU32,
    firmware_event_notifications_total: AtomicU32,
    /// CNTPCT sampled by the RTKit callback that counted this role's most
    /// recent firmware-event notification. Written only while the timing
    /// buffer is armed; a render completion record carries both roles'
    /// samples so a capture can place notification ingress on the GPU's
    /// own timestamp clock.
    firmware_event_ingress_counter: AtomicU64,
    event_worker_armed: AtomicBool,
    submission_phase: AtomicU32,
}

impl G17PRoleRtkitState {
    fn new(
        dev: ARef<AsahiDevice>,
        processor: G17PProcessor,
        firmware_contexts: mmu::FirmwareContextTableReader,
        #[cfg(CONFIG_DEV_COREDUMP)] crash_capture: Arc<Mutex<G17PCrashCaptureState>>,
    ) -> Self {
        Self {
            dev,
            processor,
            firmware_contexts,
            #[cfg(CONFIG_DEV_COREDUMP)]
            crash_capture,
            acked: AtomicBool::new(false),
            crashed: AtomicBool::new(false),
            runtime_events: AtomicU32::new(0),
            firmware_event_notifications: AtomicU32::new(0),
            firmware_event_notifications_total: AtomicU32::new(0),
            firmware_event_ingress_counter: AtomicU64::new(0),
            event_worker_armed: AtomicBool::new(false),
            submission_phase: AtomicU32::new(g17_manager::G17P_SUBMIT_PHASE_IDLE),
        }
    }

    /// Count one firmware-event notification and emit its budgeted ingress
    /// diagnostic. Shared by the hard-IRQ early callback and the workqueue
    /// callback so every notification is counted exactly once, whichever
    /// path delivered it. Only atomics, a module-parameter read and a
    /// `dev_info!` (printk formats into a stack buffer and takes no sleeping
    /// lock), so it is valid in hard-IRQ context.
    ///
    /// The pending count is swapped to zero on every drain; using it as the
    /// log budget would print every IRQ during a serviced storm, so the
    /// diagnostic is budgeted over the role lifetime total instead.
    fn note_firmware_event_notification(&self) {
        if crate::g17_timing_buffer::enabled() || crate::g17_e2e::armed() {
            self.firmware_event_ingress_counter
                .store(crate::g17_timing_buffer::arch_counter(), Ordering::Relaxed);
        }
        let count = self
            .firmware_event_notifications
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let total = self
            .firmware_event_notifications_total
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        if g17p_notification_log_due(total) {
            if crate::util::g17p_verbose() {
                dev_info!(
                    self.dev.as_ref(),
                    "G17P firmware-event notification role={} pending={} total={} submit-phase={}\n",
                    self.processor.name(),
                    count,
                    total,
                    self.submission_phase.load(Ordering::Acquire)
                );
            }
        }
    }
}

#[cfg(CONFIG_DEV_COREDUMP)]
struct G17PCrashRecord {
    processor: G17PProcessor,
    original_size: usize,
    crashlog: KVVec<u8>,
}

#[cfg(CONFIG_DEV_COREDUMP)]
struct G17PCrashCaptureState {
    records: [Option<G17PCrashRecord>; 2],
}

#[cfg(CONFIG_DEV_COREDUMP)]
fn new_g17p_crash_capture() -> Result<Arc<Mutex<G17PCrashCaptureState>>> {
    Arc::pin_init(
        new_mutex!(
            G17PCrashCaptureState {
                records: [None, None],
            },
            "G17PCrashCaptureState"
        ),
        GFP_KERNEL,
    )
}

#[cfg(CONFIG_DEV_COREDUMP)]
fn crash_qword(raw: &[u8], index: usize) -> u64 {
    let offset = index * 8;
    u64::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
        raw[offset + 4],
        raw[offset + 5],
        raw[offset + 6],
        raw[offset + 7],
    ])
}

#[cfg(CONFIG_DEV_COREDUMP)]
fn crash_u32(raw: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(raw.get(offset..offset + 4)?.try_into().ok()?))
}

#[cfg(CONFIG_DEV_COREDUMP)]
fn crash_u64(raw: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(raw.get(offset..offset + 8)?.try_into().ok()?))
}

#[cfg(CONFIG_DEV_COREDUMP)]
fn crash_fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    ((a as u32) << 24) | ((b as u32) << 16) | ((c as u32) << 8) | d as u32
}

#[cfg(CONFIG_DEV_COREDUMP)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct G17PCrashRegisters {
    pc: u64,
    far: u64,
    esr: u64,
    sp: u64,
    psr: u64,
}

#[cfg(CONFIG_DEV_COREDUMP)]
fn g17p_crash_registers(raw: &[u8]) -> Option<G17PCrashRegisters> {
    let total = usize::try_from(crash_u32(raw, 8)?).ok()?.min(raw.len());
    let mut offset = 0x20usize;
    for _ in 0..64 {
        let kind = crash_u32(raw, offset)?;
        let size = usize::try_from(crash_u32(raw, offset + 12)?).ok()?;
        if kind == crash_fourcc(b'C', b'L', b'H', b'E') {
            return None;
        }
        if size < 16 || offset.checked_add(size)? > total {
            return None;
        }
        if kind == crash_fourcc(b'C', b'r', b'g', b'8') {
            let body = offset + 16;
            if size < 16 + 0x350 {
                return None;
            }
            return Some(G17PCrashRegisters {
                sp: crash_u64(raw, body + 0x100)?,
                pc: crash_u64(raw, body + 0x108)?,
                psr: crash_u64(raw, body + 0x110)?,
                far: crash_u64(raw, body + 0x330)?,
                esr: crash_u64(raw, body + 0x340)?,
            });
        }
        offset += size;
    }
    None
}

#[cfg(CONFIG_DEV_COREDUMP)]
fn log_g17p_crash_sections(state: &G17PRoleRtkitState, raw: &[u8]) {
    let Some(header) = crash_u32(raw, 0) else {
        return;
    };
    let Some(total) = crash_u32(raw, 8).and_then(|value| usize::try_from(value).ok()) else {
        return;
    };
    if header != crash_fourcc(b'C', b'L', b'H', b'E') || total > raw.len() {
        dev_err!(
            state.dev.as_ref(),
            "G17P {} crashlog header invalid: fourcc={:#x} size={:#x}/{:#x}\n",
            state.processor.name(),
            header,
            total,
            raw.len()
        );
        return;
    }
    if let Some(regs) = g17p_crash_registers(raw) {
        dev_err!(
            state.dev.as_ref(),
            "G17P {} firmware exception: pc={:#x} far={:#x} esr={:#x} sp={:#x} psr={:#x}\n",
            state.processor.name(),
            regs.pc,
            regs.far,
            regs.esr,
            regs.sp,
            regs.psr
        );
    }

    let mut offset = 0x20usize;
    for _ in 0..64 {
        let Some(kind) = crash_u32(raw, offset) else {
            break;
        };
        let Some(size) = crash_u32(raw, offset + 12)
            .and_then(|value| usize::try_from(value).ok())
        else {
            break;
        };
        if kind == crash_fourcc(b'C', b'L', b'H', b'E') {
            break;
        }
        let Some(end) = offset.checked_add(size) else {
            break;
        };
        if size < 16 || end > total {
            break;
        }
        if kind == crash_fourcc(b'C', b's', b't', b'r') && size >= 20 {
            let body = offset + 16;
            let id = crash_u32(raw, body).unwrap_or(0);
            let bytes = &raw[body + 4..end];
            let length = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
            let length = length.min(192);
            if let Ok(message) = core::str::from_utf8(&bytes[..length]) {
                dev_err!(
                    state.dev.as_ref(),
                    "G17P {} firmware message {}: {}\n",
                    state.processor.name(),
                    id,
                    message
                );
            }
        }
        offset = end;
    }
}

#[cfg(CONFIG_DEV_COREDUMP)]
fn log_crash_buffer_observation(
    state: &G17PRoleRtkitState,
    source: &str,
    raw: &[u8],
    sentinel: u8,
) {
    let sentinel_qword = u64::from_le_bytes([sentinel; 8]);
    let mut first_changed = None;
    let mut first_nonzero = None;
    for index in 0..raw.len() / 8 {
        let value = crash_qword(raw, index);
        if first_changed.is_none() && value != sentinel_qword {
            first_changed = Some((index * 8, value));
        }
        if first_nonzero.is_none() && value != 0 {
            first_nonzero = Some((index * 8, value));
        }
        if first_changed.is_some() && first_nonzero.is_some() {
            break;
        }
    }
    let changed = first_changed.unwrap_or((raw.len(), sentinel_qword));
    let nonzero = first_nonzero.unwrap_or((raw.len(), 0));
    dev_info!(
        state.dev.as_ref(),
        "G17P {} crashlog {} sentinel {:#x}: changed={} off={:#x} value={:#x}; nonzero={} off={:#x} value={:#x}\n",
        state.processor.name(),
        source,
        sentinel_qword,
        first_changed.is_some(),
        changed.0,
        changed.1,
        first_nonzero.is_some(),
        nonzero.0,
        nonzero.1
    );
}

#[cfg(CONFIG_DEV_COREDUMP)]
fn publish_g17p_crashlogs(
    dev: &kernel::device::Device,
    records: &[G17PCrashRecord],
) -> Result {
    let mut crashdump = crate::crashdump::CrashDumpBuilder::new(KVVec::new())?;
    for record in records {
        crashdump.add_rtkit_crash_info(
            record.processor as u32,
            record.processor.firmware_crash_buffer().dva,
            record.original_size,
            record.crashlog.len(),
        )?;
        crashdump.add_crashlog(&record.crashlog)?;
    }
    let crashdump = KBox::new(crashdump.finalize()?, GFP_KERNEL)?;
    devcoredump::dev_coredump(
        dev,
        kernel::module::this_module::<crate::LocalModule>(),
        crashdump,
        GFP_KERNEL,
        msecs_to_jiffies(60 * 60 * 1000),
    );
    Ok(())
}

#[cfg(CONFIG_DEV_COREDUMP)]
fn retain_g17p_crashlog(
    state: &G17PRoleRtkitState,
    original_size: usize,
    crashlog: KVVec<u8>,
) -> Result {
    let record = G17PCrashRecord {
        processor: state.processor,
        original_size,
        crashlog,
    };
    publish_g17p_crashlogs(state.dev.as_ref(), core::slice::from_ref(&record))?;
    let mut capture = state.crash_capture.lock();
    capture.records[state.processor as usize] = Some(record);
    dev_info!(
        state.dev.as_ref(),
        "G17P {} crashlog published and retained\n",
        state.processor.name()
    );
    Ok(())
}

#[cfg(CONFIG_DEV_COREDUMP)]
fn retain_g17p_crashlog_slice(
    state: &G17PRoleRtkitState,
    original_size: usize,
    crashlog: &[u8],
) -> Result {
    log_g17p_crash_sections(state, crashlog);
    let mut retained = KVVec::new();
    retained.extend_from_slice(crashlog, GFP_KERNEL)?;
    retain_g17p_crashlog(state, original_size, retained)
}

#[cfg(CONFIG_DEV_COREDUMP)]
fn capture_g17p_firmware_crash_dva(state: &G17PRoleRtkitState) -> Result {
    let buffer = state.processor.firmware_crash_buffer();
    let captured_size = bounded_crashlog_size(buffer.size);
    let physical = crate::pgtable::UatPageTable::copy_firmware_physical_range(
        buffer.dva,
        captured_size,
    )?;
    log_crash_buffer_observation(state, "firmware-physical", &physical, 0);
    log_g17p_crash_sections(state, &physical);
    dev_info!(
        state.dev.as_ref(),
        "G17P {} crashlog copied from firmware data PA {:#x} ({} bytes)\n",
        state.processor.name(),
        buffer.dva,
        physical.len()
    );
    retain_g17p_crashlog(state, buffer.size, physical)
}

/// Active G17P boot does not ask Rust to back system-endpoint buffers. The
/// generic RTKit core uses its coherent-DMA fallback when these callbacks are
/// absent.
struct G17PNoRtkitBuffer;

impl rtkit::Buffer for G17PNoRtkitBuffer {
    fn iova(&self) -> Result<usize> {
        Err(ENOTSUPP)
    }

    fn buf(&mut self) -> Result<IoSysMapRef<'_, u8>> {
        Err(ENOTSUPP)
    }
}

/// Queue the event worker to signal deferred completions unless a worker
/// wrapper already owns the drain. Cheap and idempotent (`queue_work` on a
/// queued item is a no-op), so callers may kick per completion. Takes the
/// fields it needs rather than `&self` so the compute retirement loop can
/// call it while it holds disjoint borrows of the runtime.
fn kick_deferred_completion_drain_from(
    deferred: &KVec<crate::g17_drm::G17PDeferredCompletion>,
    state: Option<&Arc<G17PRoleRtkitState>>,
    drain_owned: bool,
) {
    if drain_owned || deferred.is_empty() {
        return;
    }
    if let Some(state) = state {
        if state.event_worker_armed.load(Ordering::Acquire) {
            crate::driver::queue_g17p_firmware_event_worker(state.dev.clone());
        }
    }
}

/// Bound ingress diagnostics independently of the pending service counter.
const fn g17p_notification_log_due(total: u32) -> bool {
    total != 0 && (total <= 4 || total.is_power_of_two())
}

/// Every G17P mailbox send goes through here so the opt-in message log
/// (`g17p_diag_knobs` bit10) can record it; otherwise identical to
/// `send_message`.
trait G17PLoggedSend {
    fn send_message_logged(self, endpoint: u8, message: u64) -> Result;
}

impl G17PLoggedSend for Pin<&mut rtkit::RtKit<G17PRtkitOps>> {
    fn send_message_logged(self, endpoint: u8, message: u64) -> Result {
        crate::g17_timing_buffer::msglog(
            crate::g17_timing_buffer::MSG_SEND,
            endpoint as u64,
            message,
            core::ptr::from_ref::<rtkit::RtKit<G17PRtkitOps>>(&*self) as usize as u64,
        );
        if crate::g17_e2e::option(crate::g17_e2e::OPT_MAILBOX) {
            crate::g17_e2e::record(crate::g17_e2e::K_SEND, 0, 0, [endpoint as u64, message,
                core::ptr::from_ref::<rtkit::RtKit<G17PRtkitOps>>(&*self) as usize as u64, 0, 0]);
        }
        let result = self.send_message(endpoint, message);
        // v93r g17p_recovery_path bit1: time of the last device-control
        // announcement actually handed to the mailbox.
        if result.is_ok() && endpoint == g17_rtkit::ENDPOINT_GFX_INTERRUPTS
            && message == g17_rtkit::encode_primary_device_control()
        {
            crate::g17_recovery_path::note_control_announce();
        }
        result
    }
}

struct G17PRtkitOps;

#[vtable]
impl rtkit::Operations for G17PRtkitOps {
    type Data = Arc<G17PRoleRtkitState>;
    type Buffer = G17PNoRtkitBuffer;

    fn recv_message(
        data: <Self::Data as kernel::types::ForeignOwnable>::Borrowed<'_>,
        endpoint: u8,
        message: u64,
    ) {
        crate::g17_timing_buffer::msglog(
            crate::g17_timing_buffer::MSG_RECV,
            endpoint as u64 | (data.processor as u64) << 8,
            message,
            0,
        );
        if endpoint == g17_rtkit::ENDPOINT_GFX_MESSAGES
            && (message >> 48) as u16 == INITDATA_ACK_TYPE
        {
            data.acked.store(true, Ordering::Release);
        } else if g17_completion::decode_g17p_firmware_event_notification(endpoint, message).is_ok()
        {
            // A notification reaches this workqueue callback only when
            // `recv_message_early` declined it because the event worker was
            // not armed at IRQ time. Count it here exactly once (the early
            // path counts the ones it consumes) and honour an arming that
            // happened in between. Either callback only records ingress; the
            // serialized runtime owner drains and dispatches the 0x48-byte
            // ring, so neither mutates its consumer or queue state.
            data.note_firmware_event_notification();
            // Queue on a notification from either role. The firmware raises
            // these on GFX1 for a compute submission targeting target1, and the
            // worker reads a single fixed primary-side resource, so gating the
            // queue on the primary role dropped the first submission's records.
            //
            // The worker DOES drain channel 13: `read_primary_firmware_event`
            // walks it through `PRIMARY_FIRMWARE_EVENT_CHANNEL_INDEX = 13` at
            // the 0x48 stride. `drain_report_ring` is a read-only WITNESS of
            // that same ring, to show what the worker has or has not consumed;
            // it is not a second consumer and must never advance the cursor.
            if data.event_worker_armed.load(Ordering::Acquire) {
                crate::driver::queue_g17p_firmware_event_worker(data.dev.clone());
            }
        } else if endpoint == g17_rtkit::ENDPOINT_GFX_INTERRUPTS {
            data.runtime_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Hard-IRQ context: the mailbox rx IRQ calls `apple_rtkit_rx`, which
    /// offers every application-endpoint message here first and, on `true`,
    /// neither allocates its `GFP_ATOMIC` message record nor queues it on the
    /// RTKit ordered workqueue (`rust/kernel/soc/apple/rtkit.rs`
    /// `Operations::recv_message_early`; `drivers/soc/apple/rtkit.c`
    /// `apple_rtkit_rx`).
    fn recv_message_early(
        data: <Self::Data as kernel::types::ForeignOwnable>::Borrowed<'_>,
        endpoint: u8,
        message: u64,
    ) -> bool {
        crate::g17_timing_buffer::msglog(
            crate::g17_timing_buffer::MSG_RECV_EARLY,
            endpoint as u64 | (data.processor as u64) << 8,
            message,
            0,
        );
        if crate::g17_e2e::option(crate::g17_e2e::OPT_MAILBOX) {
            crate::g17_e2e::record(crate::g17_e2e::K_IRQ, 0, 0,
                [endpoint as u64 | (data.processor as u64) << 8, message, 0, 0, 0]);
        }
        match (endpoint, (message >> 48) as u16) {
            (g17_rtkit::ENDPOINT_GFX_MESSAGES, INITDATA_ACK_TYPE) => {
                data.acked.store(true, Ordering::Release);
                true
            }
            _ => {
                // The firmware-event notification is the completion wake.
                // Taking it here removes one of the two workqueue hops between
                // the firmware's doorbell and the event worker: the worker is
                // queued straight from the IRQ (`queue_work` on the embedded
                // work item is IRQ-safe, and an already-queued item is a
                // no-op). Only an armed worker may consume it; before arming,
                // the workqueue callback still counts the notification, so
                // nothing is lost or counted twice.
                if data.event_worker_armed.load(Ordering::Acquire)
                    && g17_completion::decode_g17p_firmware_event_notification(endpoint, message)
                        .is_ok()
                {
                    data.note_firmware_event_notification();
                    crate::driver::queue_g17p_firmware_event_worker(data.dev.clone());
                    true
                } else {
                    false
                }
            }
        }
    }

    fn crashed(
        data: <Self::Data as kernel::types::ForeignOwnable>::Borrowed<'_>,
        crashlog: Option<&[u8]>,
    ) {
        data.crashed.store(true, Ordering::Release);
        dev_err!(
            data.dev.as_ref(),
            "G17P {} RTKit crash callback invoked; crashlog-present={}\n",
            data.processor.name(),
            crashlog.is_some()
        );
        #[cfg(CONFIG_DEV_COREDUMP)]
        let firmware_owned = crashlog.is_none();
        #[cfg(CONFIG_DEV_COREDUMP)]
        let capture = match crashlog {
            Some(crashlog) => {
                let captured_size = bounded_crashlog_size(crashlog.len());
                retain_g17p_crashlog_slice(
                    &data,
                    crashlog.len(),
                    &crashlog[..captured_size],
                )
            }
            None => capture_g17p_firmware_crash_dva(&data),
        };
        #[cfg(CONFIG_DEV_COREDUMP)]
        if let Err(error) = capture {
            if firmware_owned {
                let buffer = data.processor.firmware_crash_buffer();
                let live_root = data
                    .firmware_contexts
                    .context0_root()
                    .map(|root| root.ttb())
                    .unwrap_or(0);
                dev_err!(
                    data.dev.as_ref(),
                    "G17P {} crashlog copy from DVA {:#x} via live context-0 root {:#x} VA {:#x} size {:#x} failed ({:?})\n",
                    data.processor.name(),
                    buffer.dva,
                    live_root,
                    g17p_rtkit_uat_address(buffer.dva),
                    buffer.size,
                    error
                );
            } else {
                dev_err!(
                    data.dev.as_ref(),
                    "G17P {} crashlog coredump failed ({:?})\n",
                    data.processor.name(),
                    error
                );
            }
        }
        #[cfg(not(CONFIG_DEV_COREDUMP))]
        let _ = crashlog;
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct ExpectedProvider {
    role: G17PAscwrapV6Role,
    wrapper: (u64, u64),
    iop_vbar: (u64, u64),
}

const GFX_PROVIDER: ExpectedProvider = ExpectedProvider {
    role: G17PAscwrapV6Role::Gfx,
    wrapper: (0x4826_00000, 0x88000),
    iop_vbar: (0x4820_50000, 0x8),
};

const GFX1_PROVIDER: ExpectedProvider = ExpectedProvider {
    role: G17PAscwrapV6Role::Gfx1,
    wrapper: (0x482e_00000, 0x88000),
    iop_vbar: (0x4828_50000, 0x8),
};

fn validate_provider(
    provider: &G17PMailboxProvider,
    expected: ExpectedProvider,
) -> Result<G17PAscwrapV6Lifecycle> {
    let lifecycle = provider.lifecycle()?;
    if lifecycle.role != expected.role
        || lifecycle.wrapper != expected.wrapper
        || lifecycle.iop_vbar != expected.iop_vbar
    {
        return Err(EINVAL);
    }

    Ok(lifecycle)
}

impl g17_lifecycle::AscCpuLifecycle for G17PMailboxProvider {
    type Error = Error;

    fn start_cpu(&self) -> Result {
        G17PMailboxProvider::start_cpu(self)
    }

    fn stop_cpu(&self) -> Result {
        G17PMailboxProvider::stop_cpu(self)
    }
}

struct G17PBootedSession {
    cpus: g17_lifecycle::StartedCpuPair<G17PMailboxProvider>,
    gfx_state: Arc<G17PRoleRtkitState>,
    gfx_rtkit: rtkit::RtKit<G17PRtkitOps>,
    gfx1_state: Arc<G17PRoleRtkitState>,
    gfx1_rtkit: rtkit::RtKit<G17PRtkitOps>,
    queue: Option<g17_manager::G17PSksmQueue>,
    compute_uapi_enabled: bool,
    queue_setup_pending: bool,
}

struct G17PBootSessionConfig {
    queue: Option<g17_manager::G17PSksmQueue>,
    compute_uapi_enabled: bool,
    queue_setup_pending: bool,
}

/// Every owner which becomes live once either ASC processor starts.
///
/// Failed boot paths pass this complete transaction to
/// `rollback_failed_boot_session`.  It may be destroyed only after both CPU
/// stops succeed; otherwise the entire graph is intentionally retained.
struct G17PStartingSession {
    cpus: g17_lifecycle::StartedCpuPair<G17PMailboxProvider>,
    gfx_state: Arc<G17PRoleRtkitState>,
    gfx_rtkit: rtkit::RtKit<G17PRtkitOps>,
    gfx1_state: Arc<G17PRoleRtkitState>,
    gfx1_rtkit: rtkit::RtKit<G17PRtkitOps>,
}

impl G17PStartingSession {
    fn into_booted(self, config: G17PBootSessionConfig) -> G17PBootedSession {
        G17PBootedSession {
            cpus: self.cpus,
            gfx_state: self.gfx_state,
            gfx_rtkit: self.gfx_rtkit,
            gfx1_state: self.gfx1_state,
            gfx1_rtkit: self.gfx1_rtkit,
            queue: config.queue,
            compute_uapi_enabled: config.compute_uapi_enabled,
            queue_setup_pending: config.queue_setup_pending,
        }
    }
}

impl g17_lifecycle::FailedBootSession for G17PStartingSession {
    fn stop_cpu_pair(&mut self) -> bool {
        self.cpus.stop().is_ok()
    }

    fn retain_firmware_owners(self) {
        core::mem::forget(self);
    }
}

fn rollback_starting_session(
    pdev: &platform::Device,
    session: G17PStartingSession,
) -> bool {
    let stopped = g17_lifecycle::rollback_failed_boot_session(session);
    if !stopped {
        dev_err!(
            pdev.as_ref(),
            "G17P boot rollback: CPU stop unproven; retaining every firmware owner\n"
        );
    }
    stopped
}

fn rollback_boxed_starting_session(
    pdev: &platform::Device,
    session: KBox<G17PStartingSession>,
) -> bool {
    rollback_starting_session(pdev, KBox::into_inner(session))
}

fn wait_initdata_ack(state: &G17PRoleRtkitState) -> Result {
    for _ in 0..INITDATA_ACK_POLLS {
        if state.acked.load(Ordering::Acquire) {
            return Ok(());
        }
        if state.crashed.load(Ordering::Acquire) {
            return Err(EIO);
        }
        fsleep(Delta::from_millis(10));
    }
    Err(ETIMEDOUT)
}

fn finish_boot_session(
    pdev: &platform::Device,
    manager: &mut g17_manager::G17PManagerConstruction,
    test_stage: G17PTestStage,
    gfx_state: &G17PRoleRtkitState,
    gfx1_state: &G17PRoleRtkitState,
) -> Result<G17PBootSessionConfig> {
    let mut counters = manager.control_counters()?;
    // The single native 0x89 sent immediately after both initdata ACKs is both
    // powerTransitionEnd and the event that releases the opening control rings.
    // Once the root pointers use the real shared arena, firmware may retire the
    // rings before the host samples them here. Linux previously sent a second
    // synthetic 0x89 only because its disconnected state objects hid the first
    // transition from these counters.
    for _ in 0..CONTROL_COUNTER_POLLS {
        if counters.opening_retired() {
            break;
        }
        if gfx_state.crashed.load(Ordering::Acquire)
            || gfx1_state.crashed.load(Ordering::Acquire)
        {
            return Err(EIO);
        }
        fsleep(Delta::from_millis(1));
        counters = manager.control_counters()?;
    }
    if !counters.opening_retired() {
        dev_err!(
            pdev.as_ref(),
            "G17P control: counters stopped at {:?}/{:?}, expected {:?}/{:?}\n",
            counters.primary,
            counters.secondary,
            [g17_initdata::CONTROL_OPENING_PRIMARY_PRODUCER; 3],
            [g17_initdata::CONTROL_OPENING_SECONDARY_PRODUCER; 3],
        );
        return Err(ETIMEDOUT);
    }
    dev_info!(
        pdev.as_ref(),
        "G17P control: counters retired by the single native post-initdata 0x89; no duplicate start message sent\n"
    );
    log_g17p_firmware_recovery_handshake(
        pdev.as_ref(),
        manager,
        "native-opening-retired-baseline",
    );
    log_g17p_firmware_recovery_handshake(
        pdev.as_ref(),
        manager,
        "native-opening-recovery-deferred-to-runtime-worker",
    );
    log_g17p_fault_report(
        pdev.as_ref(),
        manager,
        "native-opening-recovery-deferred-to-runtime-worker",
    );

    if !g17_submission::G17P_NATIVE_PRE_USER_EP21_MESSAGES.is_empty() {
        return Err(EINVAL);
    }
    manager.validate_native_pre_user_roots()?;
    if test_stage.enables_compute_uapi() {
        // No bootstrap render runs in the production UAPI boot path. Activate
        // the already-mapped context-1 global FList root explicitly before
        // any client can publish its first render.
        manager.activate_render_global_context()?;
    }
    if !test_stage.configures_qid4() {
        dev_info!(
            pdev.as_ref(),
            "G17P session: native control stage complete; no pre-user EP21 work/control message; QID4 remains untouched\n"
        );
        return Ok(G17PBootSessionConfig {
            queue: None,
            compute_uapi_enabled: false,
            queue_setup_pending: false,
        });
    }

    if test_stage.prepares_qid4_at_startup() {
        dev_info!(
            pdev.as_ref(),
            "G17P session: explicit QID4 diagnostic defers queue memory preparation to serialized runtime work\n"
        );
    } else {
        dev_info!(
            pdev.as_ref(),
            "G17P session: render/compute UAPI ready; QID4 allocation deferred until first compute submit\n"
        );
    }
    dev_info!(
        pdev.as_ref(),
        "G17P diagnostic: j700-dual-initdata-publication-v1 armed\n"
    );
    Ok(G17PBootSessionConfig {
        queue: None,
        compute_uapi_enabled: test_stage.enables_compute_uapi(),
        queue_setup_pending: test_stage.prepares_qid4_at_startup(),
    })
}

fn boot_session(
    pdev: &platform::Device,
    drm: &ARef<AsahiDevice>,
    registers: &regs::Resources,
    manager: &mut g17_manager::G17PManagerConstruction,
    test_stage: G17PTestStage,
    stop_unproven: &mut bool,
) -> Result<G17PBootedSession> {
    let gfx = G17PMailboxProvider::get(pdev.as_ref(), 0)?;
    let gfx_lifecycle = validate_provider(&gfx, GFX_PROVIDER)?;
    let gfx1 = G17PMailboxProvider::get(pdev.as_ref(), 1)?;
    let gfx1_lifecycle = validate_provider(&gfx1, GFX1_PROVIDER)?;
    let gfx_gate = gfx.require_safe_cpu_lifecycle();
    let gfx1_gate = gfx1.require_safe_cpu_lifecycle();
    let transaction_gate =
        g17_lifecycle::admit_cpu_start(gfx_lifecycle.missing, gfx1_lifecycle.missing);
    if gfx_gate.is_err() || gfx1_gate.is_err() || transaction_gate.is_err() {
        return Err(ENOTSUPP);
    }
    dev_info!(pdev.as_ref(), "G17P session: providers ready\n");
    // Read what a restart actually depends on, at the point it is decided.
    // The failing rebuild reports only a 1.00s ETIME out of apple_rtkit_boot,
    // which is the same log line whether the ASC restarted and said nothing,
    // never restarted at all, or restarted with no reset vector to run.
    let gfx_diag = gfx.diagnostic_alias();
    let gfx1_diag = gfx1.diagnostic_alias();
    log_g17p_asc_state(pdev.as_ref(), &gfx_diag, "gfx", "rebuild-entry");
    log_g17p_asc_state(pdev.as_ref(), &gfx1_diag, "gfx1", "rebuild-entry");

    let firmware_contexts = manager.uat().firmware_context_table_reader();
    #[cfg(CONFIG_DEV_COREDUMP)]
    let crash_capture = new_g17p_crash_capture()?;
    let gfx_state = Arc::new(
        G17PRoleRtkitState::new(
            drm.clone(),
            G17PProcessor::Gfx,
            firmware_contexts.clone(),
            #[cfg(CONFIG_DEV_COREDUMP)]
            crash_capture.clone(),
        ),
        GFP_KERNEL,
    )?;
    let gfx1_state = Arc::new(
        G17PRoleRtkitState::new(
            drm.clone(),
            G17PProcessor::Gfx1,
            firmware_contexts,
            #[cfg(CONFIG_DEV_COREDUMP)]
            crash_capture,
        ),
        GFP_KERNEL,
    )?;
    let mut gfx_rtkit =
        rtkit::RtKit::<G17PRtkitOps>::new_g17p(drm.as_ref(), None, 0, gfx_state.clone())?;
    let gfx1_rtkit =
        rtkit::RtKit::<G17PRtkitOps>::new_g17p(drm.as_ref(), None, 1, gfx1_state.clone())?;
    dev_info!(pdev.as_ref(), "G17P session: both RTKit receivers armed\n");

    registers.validate_t8140_axi_transition()?;
    dev_info!(pdev.as_ref(), "G17P session: provider AXI transition validated\n");
    let trace_mask = module_parameters::g17p_fw_trace.value();
    if trace_mask != 0 {
        if let Err(error) = manager.arm_firmware_trace_classes(trace_mask) {
            dev_warn!(
                pdev.as_ref(),
                "G17P firmware trace arm failed ({:?})\n",
                error
            );
        }
    }
    let handoff = manager.handoff();
    let gfx_init =
        g17_rtkit::encode_initdata_doorbell(handoff.primary.instance.root).map_err(|_| EINVAL)?;
    let gfx1_init =
        g17_rtkit::encode_initdata_doorbell(handoff.secondary.instance.root).map_err(|_| EINVAL)?;
    // Marker + timing on the two steps a session REBUILD can hang in.
    //
    // `apple_rtkit_boot` performs two `apple_rtkit_wait_for_completion` waits,
    // each `msecs_to_jiffies(1000)`: first `epmap_completion` (the IOP's
    // HELLO/EPMAP), then `iop_pwr_ack_completion`. So ONE timed-out RTKit boot
    // costs almost exactly 1.00s -- which is what a 1.02s failed rebuild is
    // made of, and NOT a `wait_initdata_ack` timeout, whose budget is
    // INITDATA_ACK_POLLS (200) x 10ms = 2.0s. These markers say which step it
    // actually was, and on which role, without inferring it from arithmetic.
    let mut cpus = g17_lifecycle::StartedCpuPair::new(gfx, gfx1);
    // `apple_rtkit_boot` is PASSIVE: it waits for the IOP to volunteer a
    // HELLO and never pokes it. m1n1's `rtkit_boot_internal` and Linux's own
    // `apple_rtkit_wake` both send SET_IOP_PWR_STATE(INIT) straight after
    // starting the CPU, with the comment that it "can be sent unconditionally
    // to wake up a possibly sleeping IOP". A cold ASC coming out of reset
    // volunteers HELLO, which is why the probe path works; nothing establishes
    // that an ASC whose RUN bit was merely toggled does the same.
    let restart_mode = crate::g17_config::ASC_RESTART;
    let start_result = cpus.start_staged_in_place(|| {
        dev_info!(pdev.as_ref(), "G17P session: GFX processor started\n");
        log_g17p_asc_state(pdev.as_ref(), &gfx_diag, "gfx", "post-cpu-run");
        let started = Instant::<Monotonic>::now();
        let result = if restart_mode == 0 {
            Pin::new(&mut gfx_rtkit).boot()
        } else {
            Pin::new(&mut gfx_rtkit).wake()
        };
        log_g17p_asc_state(pdev.as_ref(), &gfx_diag, "gfx", "post-rtkit-boot");
        dev_info!(
            pdev.as_ref(),
            "G17PMARK rtkit-boot role=gfx restart_mode={} ok={} took={}us -- ~1000000us means the wait timed out on the IOP HELLO/EPMAP, i.e. the restarted ASC never re-ran its RTKit handshake\n",
            restart_mode,
            result.is_ok(),
            started.elapsed().as_micros_ceil(),
        );
        result?;
        dev_info!(pdev.as_ref(), "G17P session: GFX RTKit handshake complete\n");
        Ok(())
    });
    let mut session = G17PStartingSession {
        cpus,
        gfx_state,
        gfx_rtkit,
        gfx1_state,
        gfx1_rtkit,
    };
    if let Err(error) = start_result {
        if !rollback_starting_session(pdev, session) {
            *stop_unproven = true;
        }
        return Err(error);
    }

    let boot_result = (|| -> Result<G17PBootSessionConfig> {
        dev_info!(pdev.as_ref(), "G17P session: GFX1 processor started\n");
        {
            let started = Instant::<Monotonic>::now();
            let result = Pin::new(&mut session.gfx1_rtkit).boot();
            dev_info!(
                pdev.as_ref(),
                "G17PMARK rtkit-boot role=gfx1 ok={} took={}us\n",
                result.is_ok(),
                started.elapsed().as_micros_ceil(),
            );
            result?;
        }
        dev_info!(pdev.as_ref(), "G17P session: GFX1 RTKit handshake complete\n");

        let root_snapshot = manager.uat().prepare_t8140_firmware_high_roots()?;
        manager
            .uat()
            .confirm_t8140_primary_high_root(root_snapshot)?;
        manager.uat().mirror_t8140_secondary_high_root()?;
        registers.t8140_pre_initdata_clear()?;

        Pin::new(&mut session.gfx_rtkit)
            .start_endpoint(g17_rtkit::ENDPOINT_GFX_MESSAGES)?;
        Pin::new(&mut session.gfx_rtkit)
            .start_endpoint(g17_rtkit::ENDPOINT_GFX_INTERRUPTS)?;
        Pin::new(&mut session.gfx1_rtkit)
            .start_endpoint(g17_rtkit::ENDPOINT_GFX_MESSAGES)?;
        Pin::new(&mut session.gfx1_rtkit)
            .start_endpoint(g17_rtkit::ENDPOINT_GFX_INTERRUPTS)?;
        Pin::new(&mut session.gfx_rtkit)
            .send_message_logged(g17_rtkit::ENDPOINT_GFX_MESSAGES, gfx_init)?;
        Pin::new(&mut session.gfx1_rtkit)
            .send_message_logged(g17_rtkit::ENDPOINT_GFX_MESSAGES, gfx1_init)?;
        dev_info!(
            pdev.as_ref(),
            "G17P session: primary and secondary initdata published\n"
        );
        {
            let started = Instant::<Monotonic>::now();
            let result = wait_initdata_ack(&session.gfx_state);
            dev_info!(
                pdev.as_ref(),
                "G17PMARK initdata-ack role=gfx ok={} took={}us (budget 2000000us)\n",
                result.is_ok(),
                started.elapsed().as_micros_ceil(),
            );
            result?;
        }
        dev_info!(pdev.as_ref(), "G17P session: primary initdata ACK received\n");
        {
            let started = Instant::<Monotonic>::now();
            let result = wait_initdata_ack(&session.gfx1_state);
            dev_info!(
                pdev.as_ref(),
                "G17PMARK initdata-ack role=gfx1 ok={} took={}us\n",
                result.is_ok(),
                started.elapsed().as_micros_ceil(),
            );
            result?;
        }
        dev_info!(pdev.as_ref(), "G17P session: secondary initdata ACK received\n");

        probe_g17p_ktrace(pdev.as_ref(), manager, "post-initdata-ack");

        finish_boot_session(
            pdev,
            manager,
            test_stage,
            &session.gfx_state,
            &session.gfx1_state,
        )
    })();

    match boot_result {
        Ok(config) => Ok(session.into_booted(config)),
        Err(error) => {
            if !rollback_starting_session(pdev, session) {
                *stop_unproven = true;
            }
            Err(error)
        }
    }
}

#[inline(never)]
fn boot_initial_session(
    pdev: &platform::Device,
    drm: &ARef<AsahiDevice>,
    registers: &regs::Resources,
    soc: &'static hw::agx3::SocConfig,
    num_clusters: u32,
    gpc_perf_state_map: u32,
    gpc_perf_state_map_low: u32,
    gpc_perf_state_control: u32,
    test_stage: G17PTestStage,
) -> Result<(KBox<g17_manager::G17PManagerConstruction>, G17PBootedSession)> {
    // Allocate the transaction owner before either ASC can become live.  The
    // RTKit transports are large enough that retaining their aggregate by
    // value across resource construction exhausts the module-probe task's
    // stack.  In-place initialization below transfers them directly into
    // this preallocated owner without introducing an allocation failure after
    // CPU start.
    let session_slot = KBox::<G17PStartingSession>::new_uninit(GFP_KERNEL)?;
    let gfx = G17PMailboxProvider::get(pdev.as_ref(), 0)?;
    let gfx_lifecycle = validate_provider(&gfx, GFX_PROVIDER)?;
    let gfx1 = G17PMailboxProvider::get(pdev.as_ref(), 1)?;
    let gfx1_lifecycle = validate_provider(&gfx1, GFX1_PROVIDER)?;
    let gfx_gate = gfx.require_safe_cpu_lifecycle();
    let gfx1_gate = gfx1.require_safe_cpu_lifecycle();
    let transaction_gate =
        g17_lifecycle::admit_cpu_start(gfx_lifecycle.missing, gfx1_lifecycle.missing);
    if gfx_gate.is_err() || gfx1_gate.is_err() || transaction_gate.is_err() {
        return Err(ENOTSUPP);
    }
    dev_info!(pdev.as_ref(), "G17P session: providers ready\n");

    let mut uat = Some(KBox::new(mmu::Uat::new_t8140(drm, true)?, GFP_KERNEL)?);
    let firmware_contexts = uat
        .as_ref()
        .ok_or(EIO)?
        .firmware_context_table_reader();
    #[cfg(CONFIG_DEV_COREDUMP)]
    let crash_capture = new_g17p_crash_capture()?;
    let gfx_state = Arc::new(
        G17PRoleRtkitState::new(
            drm.clone(),
            G17PProcessor::Gfx,
            firmware_contexts.clone(),
            #[cfg(CONFIG_DEV_COREDUMP)]
            crash_capture.clone(),
        ),
        GFP_KERNEL,
    )?;
    let gfx1_state = Arc::new(
        G17PRoleRtkitState::new(
            drm.clone(),
            G17PProcessor::Gfx1,
            firmware_contexts,
            #[cfg(CONFIG_DEV_COREDUMP)]
            crash_capture,
        ),
        GFP_KERNEL,
    )?;
    // The mailbox provider resumes its genpd in apple_mbox_start().  A GFX
    // power-on that does not settle fails that resume with ETIMEDOUT; the
    // provider then clears the runtime PM error, which used to turn every
    // later attempt into EINVAL. A failed constructor releases its RX claim,
    // and neither ASC has started, so a bounded retry is safe here.
    let new_rtkit = |index, state: Arc<G17PRoleRtkitState>| {
        for attempt in 1..=3 {
            match rtkit::RtKit::<G17PRtkitOps>::new_g17p(
                drm.as_ref(), None, index, state.clone(),
            ) {
                Ok(rtkit) => return Ok(rtkit),
                Err(error) => {
                    dev_err!(pdev.as_ref(),
                        "G17P initial session: RTKit receiver {} constructor attempt {} failed ({:?})\n",
                        index, attempt, error);
                    if error != EINVAL && error != ETIMEDOUT {
                        return Err(error);
                    }
                    if attempt == 3 {
                        dev_err!(pdev.as_ref(),
                            "G17P initial session: RTKit receiver {} still failing ({:?}); deferring probe until providers settle\n",
                            index, error);
                        return Err(EPROBE_DEFER);
                    }
                    fsleep(Delta::from_millis(20));
                }
            }
        }
        Err(EPROBE_DEFER)
    };
    let mut gfx_rtkit = new_rtkit(0, gfx_state.clone())?;
    let gfx1_rtkit = new_rtkit(1, gfx1_state.clone())?;
    dev_info!(pdev.as_ref(), "G17P session: both RTKit receivers armed\n");

    registers.validate_t8140_axi_transition()?;
    dev_info!(pdev.as_ref(), "G17P session: provider AXI transition validated\n");
    // Marker + timing on the two steps a session REBUILD can hang in.
    //
    // `apple_rtkit_boot` performs two `apple_rtkit_wait_for_completion` waits,
    // each `msecs_to_jiffies(1000)`: first `epmap_completion` (the IOP's
    // HELLO/EPMAP), then `iop_pwr_ack_completion`. So ONE timed-out RTKit boot
    // costs almost exactly 1.00s -- which is what a 1.02s failed rebuild is
    // made of, and NOT a `wait_initdata_ack` timeout, whose budget is
    // INITDATA_ACK_POLLS (200) x 10ms = 2.0s. These markers say which step it
    // actually was, and on which role, without inferring it from arithmetic.
    let mut cpus = g17_lifecycle::StartedCpuPair::new(gfx, gfx1);
    let start_result = cpus.start_staged_in_place(|| {
        dev_info!(pdev.as_ref(), "G17P session: GFX processor started\n");
        let started = Instant::<Monotonic>::now();
        let result = Pin::new(&mut gfx_rtkit).boot();
        dev_info!(
            pdev.as_ref(),
            "G17PMARK rtkit-boot role=gfx ok={} took={}us -- ~1000000us means apple_rtkit_boot timed out on the IOP HELLO/EPMAP, i.e. the restarted ASC never re-ran its RTKit handshake\n",
            result.is_ok(),
            started.elapsed().as_micros_ceil(),
        );
        result?;
        dev_info!(pdev.as_ref(), "G17P session: GFX RTKit handshake complete\n");
        Ok(())
    });
    let mut session = session_slot.write_init(try_init!(G17PStartingSession {
        cpus,
        gfx_state,
        gfx_rtkit,
        gfx1_state,
        gfx1_rtkit,
    }))?;
    if let Err(error) = start_result {
        rollback_boxed_starting_session(pdev, session);
        return Err(error);
    }

    dev_info!(pdev.as_ref(), "G17P session: GFX1 processor started\n");
    let gfx1_boot_result = {
        let started = Instant::<Monotonic>::now();
        let result = Pin::new(&mut session.gfx1_rtkit).boot();
        dev_info!(
            pdev.as_ref(),
            "G17PMARK rtkit-boot role=gfx1 ok={} took={}us\n",
            result.is_ok(),
            started.elapsed().as_micros_ceil(),
        );
        result
    };
    if let Err(error) = gfx1_boot_result {
        rollback_boxed_starting_session(pdev, session);
        return Err(error);
    }
    dev_info!(pdev.as_ref(), "G17P session: GFX1 RTKit handshake complete\n");

    let root_snapshot = match uat
        .as_ref()
        .ok_or(EIO)
        .and_then(|uat| uat.prepare_t8140_firmware_high_roots())
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            rollback_boxed_starting_session(pdev, session);
            return Err(error);
        }
    };
    let manager_result = uat.take().ok_or(EIO).and_then(|uat| {
        g17_manager::G17PManagerConstruction::new_with_uat(
            drm,
            soc,
            g17_manager::T8140_G17P_PLATFORM_CONFIG,
            num_clusters,
            gpc_perf_state_map,
            gpc_perf_state_map_low,
            gpc_perf_state_control,
            uat,
        )
    });
    let mut manager = match manager_result {
        Ok(manager) => manager,
        Err(error) => {
            rollback_boxed_starting_session(pdev, session);
            return Err(error);
        }
    };
    let boot_result = (|| -> Result<G17PBootSessionConfig> {
        manager
            .uat()
            .confirm_t8140_primary_high_root(root_snapshot)?;
        manager.uat().mirror_t8140_secondary_high_root()?;
        registers.t8140_pre_initdata_clear()?;

        let trace_mask = module_parameters::g17p_fw_trace.value();
        if trace_mask != 0 {
            if let Err(error) = manager.arm_firmware_trace_classes(trace_mask) {
                dev_warn!(
                    pdev.as_ref(),
                    "G17P firmware trace arm failed ({:?})\n",
                    error
                );
            }
        }
        let handoff = manager.handoff();
        let gfx_init = g17_rtkit::encode_initdata_doorbell(handoff.primary.instance.root)
            .map_err(|_| EINVAL)?;
        let gfx1_init = g17_rtkit::encode_initdata_doorbell(handoff.secondary.instance.root)
            .map_err(|_| EINVAL)?;
        // Serialise the two roles' initdata handshakes, as native does. MEASURED
        // on the m1n1 hv capture 20260904-001852Z: macOS completes role 0 entirely
        // before it touches role 1's endpoints.
        //
        //     36.32ms role0 -> ep0  start ep 0x20
        //     38.88ms role0 -> ep0  start ep 0x21
        //     40.40ms role0 -> ep20 initdata
        //     43.59ms role0 <- ep20 ACK           <-- role 0 complete
        //     44.08ms role1 -> ep0  start ep 0x20 <-- role 1 only begins here
        //     52.01ms role1 -> ep20 initdata
        //     57.55ms role1 <- ep20 ACK
        //
        // This driver started all four endpoints, published both initdata blocks
        // and only then waited for either ACK, so the secondary's endpoints came up
        // while the primary was still mid-handshake.
        Pin::new(&mut session.gfx_rtkit)
            .start_endpoint(g17_rtkit::ENDPOINT_GFX_MESSAGES)?;
        Pin::new(&mut session.gfx_rtkit)
            .start_endpoint(g17_rtkit::ENDPOINT_GFX_INTERRUPTS)?;
        Pin::new(&mut session.gfx_rtkit)
            .send_message_logged(g17_rtkit::ENDPOINT_GFX_MESSAGES, gfx_init)?;
        dev_info!(
            pdev.as_ref(),
            "G17P session: primary and secondary initdata published\n"
        );
        {
            let started = Instant::<Monotonic>::now();
            let result = wait_initdata_ack(&session.gfx_state);
            dev_info!(
                pdev.as_ref(),
                "G17PMARK initdata-ack role=gfx ok={} took={}us (budget 2000000us)\n",
                result.is_ok(),
                started.elapsed().as_micros_ceil(),
            );
            result?;
        }
        dev_info!(pdev.as_ref(), "G17P session: primary initdata ACK received\n");

        // Only now bring up the secondary, per the native ordering above.
        Pin::new(&mut session.gfx1_rtkit)
            .start_endpoint(g17_rtkit::ENDPOINT_GFX_MESSAGES)?;
        Pin::new(&mut session.gfx1_rtkit)
            .start_endpoint(g17_rtkit::ENDPOINT_GFX_INTERRUPTS)?;
        Pin::new(&mut session.gfx1_rtkit)
            .send_message_logged(g17_rtkit::ENDPOINT_GFX_MESSAGES, gfx1_init)?;
        {
            let started = Instant::<Monotonic>::now();
            let result = wait_initdata_ack(&session.gfx1_state);
            dev_info!(
                pdev.as_ref(),
                "G17PMARK initdata-ack role=gfx1 ok={} took={}us\n",
                result.is_ok(),
                started.elapsed().as_micros_ceil(),
            );
            result?;
        }
        dev_info!(pdev.as_ref(), "G17P session: secondary initdata ACK received\n");

        // Close the GPU power-on transition, as AGXArmFirmware::powerTransitionEnd
        // does. MEASURED on the m1n1 hv capture 20260904-001852Z: this is the LAST
        // EP-0x21 message of native bring-up, one per role, immediately after the
        // initdata handshake and long before the first work doorbell. The kext
        // sends the bare tag with payload zero to both role objects. This driver
        // never sent it, and the firmware has been admitting our queues without
        // ever being told the power transition completed.
        {
            let note = g17_rtkit::encode_power_transition_end();
            Pin::new(&mut session.gfx_rtkit)
                .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, note)?;
            Pin::new(&mut session.gfx1_rtkit)
                .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, note)?;
            dev_info!(
                pdev.as_ref(),
                "G17P session: power-transition-end {:#018x} sent to both roles\n",
                note,
            );
        }

        probe_g17p_ktrace(pdev.as_ref(), &mut manager, "post-initdata-ack");

        finish_boot_session(
            pdev,
            &mut manager,
            test_stage,
            &session.gfx_state,
            &session.gfx1_state,
        )
    })();

    match boot_result {
        Ok(config) => Ok((manager, KBox::into_inner(session).into_booted(config))),
        Err(error) => {
            if !rollback_boxed_starting_session(pdev, session) {
                // The initdata roots are live and a stop failed. The manager
                // owns every DMA object behind those roots, so it must outlive
                // the unproven processors along with the starting session.
                core::mem::forget(manager);
            }
            Err(error)
        }
    }
}

/// State retained for the full lifetime of an exact T8140 platform device.
/// Hardware/runtime owner used by the T8140 DRM frontend.
/// One firmware completion, as the firmware actually reports it.
///
/// The G17 firmware signals completion the same way the M1/M2 parts do: with an
/// event "stamp" that advances by `0x100` per completed submission (upstream's
/// `EventValue::next`). Our first submit is answered with `stamp = 0x100`.
#[derive(Debug, Copy, Clone)]
pub(crate) struct G17PComputeCompletion {
    /// The outermost GPU timestamp pair from the KSM completion record.
    pub(crate) timestamps: [u64; 2],
}

/// What a publish returned: either a finished submission (legacy polling wait)
/// or a live handle to wait on with the runtime mutex released.
pub(crate) enum G17PSubmitOutcome {
    Completed([u64; 2]),
    Pending(Arc<G17PCompletionSignal>),
}

/// Interrupt-driven handoff between the firmware-event drain and a submitter.
///
/// The firmware raises an RTKit doorbell on the GFX messages endpoint and
/// publishes a completion record in the event ring; the drain decodes it and
/// wakes the waiter here. Waiting on this must be done with the runtime mutex
/// released -- the drain needs that mutex to run at all, which is exactly why
/// the previous timestamp-polling wait could never observe a completion.
#[pin_data]
pub(crate) struct G17PCompletionSignal {
    #[pin]
    state: Mutex<Option<G17PComputeCompletion>>,
    #[pin]
    cond: CondVar,
}

impl G17PCompletionSignal {
    fn new() -> Result<Arc<Self>> {
        Arc::pin_init(
            pin_init!(G17PCompletionSignal {
                state <- new_mutex!(None, "G17PCompletionSignal::state"),
                cond <- new_condvar!("G17PCompletionSignal::cond"),
            }),
            GFP_KERNEL,
        )
    }

    /// Clear any stale completion before publishing new work.
    pub(crate) fn arm(&self) {
        *self.state.lock() = None;
    }

    /// Publish a completion decoded from the firmware event ring and wake the
    /// submitter. Called from the drain, which already holds the runtime mutex.
    pub(crate) fn record(&self, completion: G17PComputeCompletion) {
        *self.state.lock() = Some(completion);
        self.cond.notify_all();
    }

    /// Block until the firmware signals, or the timeout expires.
    pub(crate) fn wait_timeout(&self, millis: u32) -> Option<G17PComputeCompletion> {
        let jiffies = msecs_to_jiffies(millis);
        let mut state = self.state.lock();
        while state.is_none() {
            match self.cond.wait_interruptible_timeout(&mut state, jiffies) {
                CondVarTimeoutResult::Timeout => break,
                CondVarTimeoutResult::Signal { .. } => break,
                CondVarTimeoutResult::Woken { .. } => continue,
            }
        }
        *state
    }
}

pub(crate) struct G17PLiveRuntime {
    pdev: Option<ARef<platform::Device>>,
    drm: Option<ARef<AsahiDevice>>,
    registers: Option<regs::Resources>,
    manager: Option<KBox<g17_manager::G17PManagerConstruction>>,
    cpus: Option<g17_lifecycle::StartedCpuPair<G17PMailboxProvider>>,
    /// A failed session rebuild could not prove that its newly started CPU
    /// pair stopped. That pair and its RTKit owners were intentionally leaked;
    /// final teardown must therefore retain the shared manager/UAT as well.
    boot_stop_unproven: bool,
    /// A processor-stop recovery started but has not installed and resumed a
    /// complete replacement session. Role state can be absent after a clean
    /// boot failure, so it cannot by itself represent this offline state.
    session_recovery_pending: bool,
    /// A failed publication requested a whole-session rebuild. Retained until
    /// the recovery worker (or an explicit synchronous rebuild) consumes it.
    session_rebuild_requested: bool,
    preparations: Arc<crate::g17_preparation::G17PPreparations>,
    /// This driver has taken over runtime PM for its own device so that the
    /// shared `gfx` power domain can be gated deliberately. Armed at most
    /// once, lazily, and only when a restart mode asks for it.
    runtime_pm_armed: bool,
    /// Both roles' firmware `__DATA` exactly as iBoot left it, taken before
    /// either processor was ever started. `None` when the pinned carveout
    /// geometry did not match this board's reset vectors.
    #[cfg(CONFIG_DEV_COREDUMP)]
    firmware_data: Option<G17PFirmwareDataSnapshot>,
    gfx_state: Option<Arc<G17PRoleRtkitState>>,
    gfx_rtkit: Option<rtkit::RtKit<G17PRtkitOps>>,
    gfx1_state: Option<Arc<G17PRoleRtkitState>>,
    gfx1_rtkit: Option<rtkit::RtKit<G17PRtkitOps>>,
    queue: Option<g17_manager::G17PSksmQueue>,
    compute_binding: Option<g17_manager::G17PComputeUserBinding>,
    compute_timestamps: Option<mmu::KernelMapping>,
    completion_indices: g17_completion::QueueIndices,
    render_tracker: Option<g17_manager::G17PPartialOpeningRenderTracker>,
    test_stage: G17PTestStage,
    accepting_submissions: bool,
    /// Permanent platform removal; unlike firmware recovery this never reopens.
    platform_removed: bool,
    queue_setup_pending: bool,
    /// DRM-side id of the queue object that currently owns the single
    /// hardware compute graph (QID 4) and the single bindable user-VM slot.
    ///
    /// `None` means nothing is bound and the next submitter may take it. A
    /// submit from a different id is a *handoff*: the previous owner's
    /// binding is released and the queue graph is recycled before the new
    /// owner's first bind, because `validate_prepublication_compute_queue`
    /// legitimately refuses a queue that has already run.
    compute_owner: Option<u64>,
    /// The owner released the graph (queue_destroy / file close) but the
    /// recycle was deferred to the next submit. Doing it at release time would
    /// mean running the firmware handshake from a dying process's file-release
    /// path; doing it here keeps every firmware-visible step on a submit.
    compute_recycle_pending: bool,
    /// A submission was published and never retired: the doorbell wait timed
    /// out, or the finish step failed. The firmware may still be holding a
    /// kick that names the owner's VM, so the graph must NOT be handed on
    /// cheaply -- the next handoff stops both CPUs first, which is the only
    /// state in which releasing that binding is provably safe.
    compute_abandoned: bool,
    /// Queue graphs a handoff took off the SKSM bridge.
    ///
    /// They are NOT freed: the firmware keeps per-QID state at
    /// `0x127628 + qid * 0x28` that names the entry ring and descriptor pages
    /// of whichever graph was last configured, and no host-visible message
    /// proves it has forgotten them. Retiring instead of freeing means a stale
    /// firmware pointer can only reach memory nothing else owns. They are
    /// released together once both CPUs stop, in `release_runtime_objects`.
    retired_queues: KVec<g17_manager::G17PSksmQueue>,
    async_compute_queues: KVec<G17PAsyncComputeQueue>,
    /// Bindings of clients whose submission was published and never seen to
    /// retire.
    ///
    /// They are NOT dropped: the firmware may still hold that kick, and the
    /// kick names this client's page tables and the GEM objects mapped into
    /// them. Each entry keeps those alive while having already handed UAT slot
    /// 1 back, so the next client binds immediately. Released together once
    /// both CPUs stop, in `release_runtime_objects`.
    quarantined_bindings: KVec<g17_manager::G17PQuarantinedComputeBinding>,
    /// Crashed-session rebuilds attempted, bounding `recover_crashed_session_once`.
    crash_recoveries: u32,
    /// The firmware retired a submission the GPU demonstrably never executed
    /// (`end == start`), so the GPU core power / KSM pause gate is closed and
    /// no host lever this driver has is known to re-open it. The next handoff
    /// must rebuild the firmware session rather than hand the retained graph
    /// on, because a rebuild is the only state in which execution has ever
    /// been observed to come back.
    compute_gate_suspect: bool,
    /// `DIAG_IDLE_HOLD` diagnostic: an IdlePowerOff(0) is in force.
    idle_hold_published: bool,
    /// `DIAG_SUBMIT_ACCOUNTING`: host copy of RegionC+0x80, and whether the
    /// bracket is currently open.
    submit_count: u32,
    submit_open: bool,
    /// `DIAG_CLPC_EFFORT`: last effort published to RegionC+0xe4.
    clpc_effort: u32,
    /// Last GPU power budget published to RegionC+0x84 (0 = none yet).
    power_target_mw: u32,
    /// Opt-in `g17p_dvfs_parity` bit0: work-tied effort state.
    dvfs_ce: crate::g17_dvfs_policy::CePolicy,
    /// Opt-in `g17p_dvfs_parity`: the open bracket used the native order.
    dvfs_native_bracket: bool,
    completion_signal: Arc<G17PCompletionSignal>,
    /// Set by a publish that returned `Pending`; consumed by the finish step
    /// once the firmware has signalled.
    pending_compute: Option<(bool, mmu::KernelMapping)>,
    /// Event-driven hardware fences for the two independently owned render
    /// slots. Installed before the work doorbell and removed only by exact
    /// retirement or the DRM scheduler's bounded timeout recovery.
    pending_render: KVec<Option<Arc<crate::g17_drm::G17PRenderCompletion>>>,
    pending_render_queued: KVec<g17_completion::PairFifo<
        Arc<crate::g17_drm::G17PRenderCompletion>, { g17_resources::G17P_RENDER_QUEUED_PER_PAIR }>>,
    queue_registry: Option<Arc<crate::g17_drm::G17PQueueRegistry>>,
    /// v93r: when a drained-pair reclaim first found work, for polling.
    reclaim_pending_since: Option<kernel::time::Instant<kernel::time::Monotonic>>,
    /// Lost render packets whose firmware retirement was not proven. Their
    /// VM job guards keep userspace mappings alive until both processors stop
    /// (or, with v93r g17p_recovery_path bit0, until their pair is drained).
    quarantined_render_completions:
        KVec<Arc<crate::g17_drm::G17PRenderCompletion>>,
    /// Type-1 EventMachine QID bits observed since each slot was armed.
    /// Cleared at arm/settlement so a compute-only or prior render event can
    /// never retire the current fence.
    render_event_masks: KVec<[u64; 2]>,
    /// Preallocated heap scratch for a render's explicit dependency set
    /// (~1 KiB), taken while one publication computes and consumes it, so the
    /// set is never staged on the publication stack.
    render_dependency_scratch: Option<KBox<g17_manager::G17PRenderDependencies>>,
    /// Exact completion tokens live in pending_render/queued. This mask makes
    /// the ordinary return path one zero check, with no allocation or scan.
    deferred_render_timeouts: u64,
    render_fence_state: g17_completion::RenderFenceState,
    /// CNTPCT at entry of the current firmware-event service pass (event
    /// worker, inline drain or recovery), carried into the render completion
    /// record. Written only while the timing buffer is armed.
    trace_service_entry_counter: u64,
    /// Completions proven retired under the runtime mutex, signalled by
    /// whoever releases it: the worker wrappers in `g17_drm` drain this list
    /// after unlocking; any other settle site queues the event worker to do
    /// so (`defer_completion`).
    deferred_completions: KVec<crate::g17_drm::G17PDeferredCompletion>,
    /// True while a worker wrapper owns the drain, so settles inside its
    /// pass do not queue a redundant worker turn.
    deferred_completion_drain_owned: bool,
}

impl G17PLiveRuntime {
    /// Called only after ingress is closed and all device workers are joined.
    /// Strong host handles may outlive unbind, but no hardware owner or DRM
    /// back-reference may remain in the closed runtime. This also breaks the
    /// runtime -> DRM -> adapter -> runtime cycle on every probe failure.
    pub(crate) fn shutdown(&mut self) {
        if self.platform_removed { return; }
        self.platform_removed = true;
        crate::g17_gpu_power::unregister();
        self.session_rebuild_requested = false;
        self.disarm_firmware_event_worker();
        g17_lifecycle::teardown_persistent_runtime(self);
        drop(self.drm.take());
        drop(self.registers.take());
        drop(self.pdev.take());
    }

    pub(crate) fn timeout_progress_snapshot(&mut self) -> Option<(u64, u32)> {
        if self.session_recovery_pending { return None; }
        let manager = self.manager.as_mut()?;
        let handshake = manager.firmware_recovery_handshake_snapshot().ok()?;
        if handshake.state != 0 { return None; }
        let fault = manager.firmware_fault_report_snapshot().ok()?;
        if fault.state != 0 { return None; }
        let progress = manager.firmware_gpu_progress_snapshot().ok()?;
        if progress.0 != handshake.epoch { return None; }
        Some(progress)
    }

    pub(crate) fn preparation_control(&self) -> Arc<crate::g17_preparation::G17PPreparations> {
        self.preparations.clone()
    }

    pub(crate) fn attach_queue_registry(
        &mut self, registry: Arc<crate::g17_drm::G17PQueueRegistry>,
    ) -> Result {
        if self.queue_registry.is_some() { return Err(EBUSY); }
        self.queue_registry = Some(registry);
        Ok(())
    }

    /// Close every path which can enqueue device-owning firmware work.
    /// Platform unbind calls this before synchronously cancelling the work
    /// items; runtime Drop then performs the firmware-visible teardown.
    pub(crate) fn disarm_firmware_event_worker(&mut self) {
        self.preparations.close(true);
        self.accepting_submissions = false;
        if let Some(state) = self.gfx_state.as_ref() {
            state.event_worker_armed.store(false, Ordering::Release);
        }
        if let Some(state) = self.gfx1_state.as_ref() {
            state.event_worker_armed.store(false, Ordering::Release);
        }
    }

    pub(crate) fn arm_primary_firmware_event_worker(&self) -> Result {
        let state = self.gfx_state.as_ref().ok_or(ENODEV)?;
        state.event_worker_armed.store(true, Ordering::Release);
        // Arm the secondary role too. `recv_message` gates queueing the worker
        // on the flag of whichever role received the notification, and the
        // firmware raises compute completions on GFX1 as well as GFX -- arming
        // only the primary silently dropped every GFX1 notification, which with
        // the doorbell-driven completion path would mean the waiter is never
        // woken at all.
        if let Some(secondary) = self.gfx1_state.as_ref() {
            secondary.event_worker_armed.store(true, Ordering::Release);
        }
        if self.primary_firmware_event_pending()
        {
            crate::driver::queue_g17p_firmware_event_worker(state.dev.clone());
        }
        Ok(())
    }

    /// Queue a control-backpressure continuation while the caller owns the
    /// runtime mutex. Unbind takes the same mutex before disarming, so it
    /// either cancels this work or makes the enqueue impossible.
    fn queue_primary_control_continuation(&self) {
        if let Some(state) = self.gfx_state.as_ref() {
            if state.event_worker_armed.load(Ordering::Acquire) {
                crate::driver::queue_g17p_firmware_event_worker(state.dev.clone());
            }
        }
    }

    /// The interrupt-driven completion handoff for this runtime.
    ///
    /// Cloned out before the caller drops the runtime mutex, so the wait can
    /// happen with that mutex released.
    pub(crate) fn completion_signal(&self) -> Arc<G17PCompletionSignal> {
        self.completion_signal.clone()
    }

    pub(crate) fn firmware_event_worker_armed(&self) -> bool {
        self.gfx_state.as_ref().is_some_and(|state| {
            state.event_worker_armed.load(Ordering::Acquire)
        })
    }

    pub(crate) fn primary_device_control_backpressured(&self) -> bool {
        !self.is_crashed() && self.manager.as_ref().is_some_and(|manager| {
            manager.primary_device_control_backpressured()
        })
    }

    fn primary_tvb_reply_pending(&self) -> bool {
        !self.is_crashed() && self.manager.as_ref().is_some_and(|manager| {
            manager.primary_tvb_reply_pending()
        })
    }

    /// Publish an already encoded mandatory reply before consuming another
    /// firmware event from either role. The manager clears the pending slot
    /// after producer publication and before this doorbell, so an RTKit send
    /// failure is fail-closed and can never replay the record or its TVB grow.
    fn flush_pending_primary_device_control(&mut self) -> Result<bool> {
        let pdev = self.pdev.as_ref().cloned().ok_or(ENODEV)?;
        let state = self.gfx_state.as_ref().cloned().ok_or(ENODEV)?;
        let result = (|| -> Result<bool> {
            let flush = self
                .manager
                .as_mut()
                .ok_or(ENODEV)?
                .flush_pending_primary_device_control()?;
            match flush {
                g17_manager::G17PPendingPrimaryControlFlush::Empty => Ok(true),
                g17_manager::G17PPendingPrimaryControlFlush::Blocked => Ok(false),
                g17_manager::G17PPendingPrimaryControlFlush::Published(_) => {
                    fence(Ordering::SeqCst);
                    Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?).send_message_logged(
                        g17_rtkit::ENDPOINT_GFX_INTERRUPTS,
                        g17_rtkit::encode_primary_device_control(),
                    )?;
                    Ok(true)
                }
            }
        })();
        if let Err(error) = result {
            note_state_crashed(&pdev, &state, "primary-control-retry", error, None);
        }
        result
    }

    /// Publish a render completion's retained FList release after any
    /// mandatory TVB reply. EAGAIN means the encoded lifecycle remains Ready
    /// and can be retried; every post-publication failure ends the session.
    fn flush_pending_render_usc_freelist_release(&mut self) -> Result<bool> {
        let Some(release) = self
            .manager
            .as_ref()
            .and_then(|manager| manager.pending_render_usc_freelist_release())
        else {
            return Ok(true);
        };
        match self.service_render_usc_freelist_release(release) {
            Ok(()) => Ok(true),
            Err(error) if error == EAGAIN => Ok(false),
            Err(error) => {
                let pdev = self.pdev.as_ref().cloned().ok_or(ENODEV)?;
                let state = self.gfx_state.as_ref().cloned().ok_or(ENODEV)?;
                note_state_crashed(&pdev, &state, "render-usc-freelist-release", error, None);
                Err(error)
            }
        }
    }

    pub(crate) fn primary_firmware_event_pending(&self) -> bool {
        if self.is_crashed() {
            return false;
        }
        self.gfx_state.as_ref().is_some_and(|state| {
            state.firmware_event_notifications.load(Ordering::Acquire) != 0
        }) || self.gfx1_state.as_ref().is_some_and(|state| {
            state.firmware_event_notifications.load(Ordering::Acquire) != 0
        }) || self.queue_setup_pending || self.primary_device_control_backpressured()
    }

    pub(crate) fn primary_recovery_pending(&self) -> bool {
        if self.session_rebuild_requested { return true; }
        self.manager
            .as_ref()
            .is_some_and(|manager| manager.pending_primary_recovery().is_some())
    }

    /// v97d: start the 3D stall watch after a render doorbell (bit4 only).
    /// Enqueued under the Runtime mutex after the armed check, like the
    /// event worker's self-requeue; unbind disables the work after disarm.
    fn arm_render_stall_watch(&self) {
        if !crate::g17_recovery_path::enabled(crate::g17_recovery_path::STALL_WATCH)
            || !self.firmware_event_worker_armed()
        {
            return;
        }
        if let Some(drm) = self.drm.as_ref() {
            crate::driver::arm_g17p_render_stall_watch(drm.clone(),
                crate::g17_recovery_path::STALL_POLL_MS);
        }
    }

    /// v97d STALL_WATCH / STALL_REDOORBELL / STALL_FRAGMENT (DIAGNOSTIC).
    /// Called from the stall work with the Runtime mutex held. Returns the
    /// re-arm delay in ms while any render pair has tracked outstanding work.
    /// Reads only firmware DATA and host state; the only message it can send
    /// is the render doorbell (bit5) or the Fragment doorbell (bit6).
    pub(crate) fn service_render_stall_watch(&mut self) -> Option<u64> {
        use crate::g17_recovery_path as rp;
        use core::sync::atomic::Ordering::Relaxed;
        if !rp::enabled(rp::STALL_WATCH) || !self.firmware_event_worker_armed() || self.is_crashed() {
            return None;
        }
        let now = crate::g17_timing_buffer::now();
        rp::STALL_POLLS.fetch_add(1, Relaxed);
        let recovery_pending = self.primary_recovery_pending();
        let candidates = self.manager.as_ref()?.render_stall_candidates();
        if candidates.is_empty() {
            for slot in rp::STALL_SLOT.iter() {
                rp::discard_stall_observation(slot);
            }
            return None;
        }
        // (pair, TA qid, 3D qid, TA completed, 3D completed, latest 3D, next 3D)
        let mut observed: KVec<(u8, u8, u8, u64, u64, u64, Option<u64>)> = KVec::new();
        let mut observed_slots = 0u64;
        for &(pair, tiling_qid, fragment_qid, latest, count, stride) in candidates.iter() {
            let (Some(ta_completed), Some(fragment_completed)) = (
                g17_manager::G17PManagerConstruction::firmware_dependency_completed(tiling_qid),
                g17_manager::G17PManagerConstruction::firmware_dependency_completed(fragment_qid),
            ) else { continue };
            let next = g17_submission::prepare_kick_submission(fragment_completed, fragment_qid, count, stride)
                .ok().map(|prepared| prepared.next_timestamp);
            if observed.push((pair, tiling_qid, fragment_qid, ta_completed, fragment_completed, latest, next),
                GFP_KERNEL).is_err() { break; }
            observed_slots |= 1u64 << pair;
        }
        // A single pair may disappear while other clients remain busy. Do
        // not carry its old interval across a graph borrow or failed read.
        for (index, slot) in rp::STALL_SLOT.iter().enumerate() {
            if observed_slots & (1u64 << index) == 0 {
                rp::discard_stall_observation(slot);
            }
        }
        let threshold = rp::STALL_MS * 1_000_000;
        for &(pair, tiling_qid, fragment_qid, ta_completed, fragment_completed, latest, next) in observed.iter() {
            let slot = &rp::STALL_SLOT[usize::from(pair) % rp::STALL_SLOTS];
            if !rp::sample_stall_slot(slot, now, fragment_completed,
                rp::stall_candidate(ta_completed, fragment_completed, latest, next)) {
                continue;
            }
            let since = slot.since.load(Relaxed);
            let stage = slot.stage.load(Relaxed);
            if stage == 0 {
                slot.stage.store(1, Relaxed);
                slot.acted_at.store(now, Relaxed);
                let detected = rp::STALL_DETECTED.fetch_add(1, Relaxed) + 1;
                if let Some(pdev) = self.pdev.as_ref() {
                    dev_err!(pdev.as_ref(),
                        "G17P 3D stall #{}: pair={} ta-qid={} 3d-qid={} ta-completed={:#x} 3d-completed={:#x} 3d-next={:#x?} 3d-latest={:#x} candidate-for={}us recovery-pending={}\n",
                        detected, pair, tiling_qid, fragment_qid, ta_completed, fragment_completed,
                        next, latest, now.saturating_sub(since) / 1000, recovery_pending);
                }
                if let Some(manager) = self.manager.as_mut() {
                    let (requested, deferred) = manager.log_b1_scheduler_state("3d-stall",
                        &[tiling_qid, fragment_qid], Some(fragment_qid));
                    if deferred { rp::STALL_DEFERRED.fetch_add(1, Relaxed); }
                    if let Some(pdev) = self.pdev.as_ref() {
                        dev_err!(pdev.as_ref(),
                            "G17P 3D stall #{}: 3d-qid={} in requested={} deferred={}\n",
                            detected, fragment_qid, requested, deferred);
                    }
                }
            }
            if recovery_pending || !rp::enabled(rp::STALL_REDOORBELL) {
                continue;
            }
            let stage = slot.stage.load(Relaxed);
            let acted_at = slot.acted_at.load(Relaxed);
            let message = if stage <= 1 {
                Some((2u32, g17_submission::G17P_PARTIAL_OPENING_PAIRED_DOORBELL, "paired"))
            } else if stage == 2 && rp::enabled(rp::STALL_FRAGMENT)
                && now.saturating_sub(acted_at) >= threshold
            {
                Some((3u32, g17_submission::G17P_NATIVE_3D_DOORBELL, "fragment"))
            } else {
                None
            };
            let Some((next_stage, doorbell, kind)) = message else { continue };
            let sent = match self.gfx_rtkit.as_mut() {
                Some(rtkit) => Pin::new(rtkit).send_message_logged(
                    g17_rtkit::ENDPOINT_GFX_INTERRUPTS, doorbell),
                None => Err(ENODEV),
            };
            slot.stage.store(if sent.is_ok() { next_stage } else { 4 }, Relaxed);
            slot.acted_at.store(now, Relaxed);
            let counter = if next_stage == 2 { &rp::STALL_DOORBELLS } else { &rp::STALL_FRAGMENT_DOORBELLS };
            let count = if sent.is_ok() { counter.fetch_add(1, Relaxed) + 1 }
                else { counter.load(Relaxed) };
            if let Some(pdev) = self.pdev.as_ref() {
                dev_err!(pdev.as_ref(),
                    "G17P 3D stall: {} re-doorbell #{} message={:#018x} pair={} 3d-qid={} 3d-completed={:#x} result={:?}\n",
                    kind, count, doorbell, pair, fragment_qid, fragment_completed, sent.map(|_| ()));
            }
        }
        Some(rp::STALL_POLL_MS)
    }

    /// Acquire this pair's FList lease before any render doorbell. A cold
    /// grow publishes 0x20 asynchronously; overlapping commands retain that
    /// request and add their own references without waiting or reissuing it.
    fn publish_render_usc_freelist_request(&mut self, render_slot: u8) -> Result {
        if crate::g17_config::RENDER_USC_FREELIST == 0 {
            return Ok(());
        }
        const RING_FULL_POLLS: usize = 1000;
        let mut ring_full_polls = 0usize;
        let publication = loop {
            match self.manager.as_mut().ok_or(ENODEV)?.publish_render_usc_freelist_request(render_slot) {
                Ok(Some(publication)) => break publication,
                Ok(None) => return Ok(()),
                Err(error) if error == EAGAIN => {
                    // A mandatory TVB reply or an unpublished 0x2e release
                    // is ahead of this grow in channel 12. Publish it now;
                    // only a genuinely full ring waits for firmware.
                    if self.is_crashed() {
                        return Err(EIO);
                    }
                    let flushed = self.flush_pending_primary_device_control()?
                        && self.flush_pending_render_usc_freelist_release()?;
                    if !flushed {
                        if ring_full_polls >= RING_FULL_POLLS {
                            return Err(ETIMEDOUT);
                        }
                        ring_full_polls += 1;
                        fsleep(Delta::from_micros(100));
                    }
                }
                Err(error) => return Err(error),
            }
        };
        fence(Ordering::SeqCst);
        let announcement = g17_rtkit::encode_primary_device_control();
        if crate::util::g17p_verbose() {
            dev_info!(
                self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                "G17P render USC FList: staged opcode={:#x} stamp={} slot={} record={:#x} producer {}->{} consumer={}; announcing {:#x}\n",
                publication.opcode,
                publication.arg,
                publication.slot,
                publication.record_address,
                publication.producer_before,
                publication.producer_after,
                publication.consumer_before,
                announcement,
            );
        }
        Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
            .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, announcement)?;

        // Native updateSubmitInfo keeps an outstanding grow lease and returns
        // without waiting for channel consumption or type 13. Its request is
        // already retained above. Firmware schedules the dependent commands;
        // event service retires only the grow reference, independently of all
        // command references. No sleep or inline event drain is needed here.
        Ok(())
    }

    /// Drop the final host reference named by a matched type-13 event. Native
    /// emits this exact 0x2e control asynchronously from releaseResources.
    fn service_render_usc_freelist_release(
        &mut self,
        release: g17_manager::G17PBootstrapUmaRelease,
    ) -> Result {
        let publication = match self
            .manager
            .as_mut()
            .ok_or(ENODEV)?
            .publish_render_usc_freelist_release(release)
        {
            Ok(publication) => publication,
            Err(error) if error == EAGAIN => {
                self.queue_primary_control_continuation();
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        fence(Ordering::SeqCst);
        let announcement = g17_rtkit::encode_primary_device_control();
        if crate::util::g17p_verbose() {
            dev_info!(
                self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                "G17P render USC FList: staged release opcode={:#x} sequence={:#x} ID={} slot={} producer {}->{}; announcing {:#x}\n",
                publication.opcode,
                release.sequence,
                release.hardware_buffer_id,
                publication.slot,
                publication.producer_before,
                publication.producer_after,
                announcement,
            );
        }
        Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
            .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, announcement)?;
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .mark_render_usc_freelist_release_sent(release, publication.producer_after)?;

        // Native release is asynchronous. Take one nonblocking snapshot; any
        // sent owner not yet consumed remains retained and is settled by later
        // event/publication boundaries. Never sleep under the runtime mutex.
        fence(Ordering::SeqCst);
        let settled = self
            .manager
            .as_mut()
            .ok_or(ENODEV)?
            .settle_consumed_render_usc_freelist_releases()?;
        if settled != 0 && crate::util::g17p_verbose() {
            dev_info!(
                self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                "G17P render USC FList: settled {} asynchronous release(s) after producer {}\n",
                settled,
                publication.producer_after,
            );
        }
        Ok(())
    }

    /// Refresh every outstanding release witness without waiting. Nothing
    /// retries on the result: a sent release never gates the next kick.
    pub(crate) fn settle_render_usc_freelist_release_before_submission(&mut self) -> Result {
        fence(Ordering::SeqCst);
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .settle_consumed_render_usc_freelist_releases()?;
        Ok(())
    }

    /// Timing-buffer sample at the entry of a service pass; zero cost while
    /// the buffer is disarmed.
    fn note_service_pass_entry(&mut self) {
        if crate::g17_timing_buffer::enabled() || crate::g17_e2e::armed() {
            self.trace_service_entry_counter = crate::g17_timing_buffer::arch_counter();
        }
    }

    /// Both roles' last notification-ingress samples (primary, secondary).
    fn notification_ingress_counters(&self) -> [u64; 2] {
        let sample = |state: &Option<Arc<G17PRoleRtkitState>>| {
            state.as_ref().map_or(0, |state| {
                state.firmware_event_ingress_counter.load(Ordering::Relaxed)
            })
        };
        [sample(&self.gfx_state), sample(&self.gfx1_state)]
    }

    /// Hand a proven retirement to whoever releases the runtime mutex.
    ///
    /// Inside a worker wrapper (`G17PRuntimeRef::service_primary_firmware_events`
    /// / `service_primary_recovery`) the wrapper drains and signals after
    /// unlocking. Anywhere else -- an inline drain under a publisher, the
    /// scheduler watchdog, a VM sweep -- the caller releases the mutex without
    /// draining, so the event worker is queued to signal it. That costs one
    /// worker turn, which is what a notification costs anyway, and it never
    /// happens on the event-driven production path.
    fn defer_completion(&mut self, completion: crate::g17_drm::G17PDeferredCompletion) {
        completion.defer_into(&mut self.deferred_completions);
        kick_deferred_completion_drain_from(
            &self.deferred_completions,
            self.gfx_state.as_ref(),
            self.deferred_completion_drain_owned,
        );
    }

    pub(crate) fn begin_deferred_completion_drain(&mut self) {
        self.deferred_completion_drain_owned = true;
    }

    /// Take everything deferred so far; the caller signals it after
    /// releasing the runtime mutex. Ends a wrapper's drain ownership. The
    /// list's storage goes with it, so the next deferral allocates once.
    pub(crate) fn take_deferred_completions(
        &mut self,
    ) -> KVec<crate::g17_drm::G17PDeferredCompletion> {
        self.deferred_completion_drain_owned = false;
        core::mem::take(&mut self.deferred_completions)
    }

    /// Rebuild or teardown, with or without a proven processor stop: signal
    /// what was deferred, in place, where `complete(Ok)` ran before deferral.
    /// A deferred completion is work the firmware has already retired, so it
    /// owns nothing a failed stop must retain, and leaving it unsignalled
    /// would strand a retired fence. Nothing publishes here, so this mutex
    /// hold cannot delay a submission.
    fn signal_deferred_completions_in_place(&mut self) {
        for completion in core::mem::take(&mut self.deferred_completions) {
            completion.signal();
        }
    }

    pub(crate) fn service_primary_firmware_events(&mut self) -> Result {
        self.note_service_pass_entry();
        if self.is_crashed() {
            return Ok(());
        }
        let pdev = self.pdev.as_ref().cloned().ok_or(ENODEV)?;
        let state = self.gfx_state.as_ref().cloned().ok_or(ENODEV)?;
        let signal = self.completion_signal.clone();
        let flushed_pending_control = if self.primary_tvb_reply_pending() {
            let flushed = self.flush_pending_primary_device_control()?;
            if !flushed {
                if let Err(error) = self.manager
                    .as_mut()
                    .ok_or(ENODEV)?
                    .note_primary_device_control_backpressure()
                {
                    note_state_crashed(
                        &pdev,
                        &state,
                        "primary-control-backpressure-timeout",
                        error,
                        None,
                    );
                    return Err(error);
                }
                return Ok(());
            }
            true
        } else {
            false
        };
        let flushed_pending_release = if self
            .manager
            .as_ref()
            .is_some_and(|manager| manager.pending_render_usc_freelist_release().is_some())
        {
            if !self.flush_pending_render_usc_freelist_release()? {
                if let Err(error) = self
                    .manager
                    .as_mut()
                    .ok_or(ENODEV)?
                    .note_flist_release_backpressure()
                {
                    note_state_crashed(
                        &pdev,
                        &state,
                        "render-usc-freelist-release-timeout",
                        error,
                        None,
                    );
                    return Err(error);
                }
                return Ok(());
            }
            true
        } else {
            false
        };
        if let Some(manager) = self.manager.as_mut() {
            manager.observe_grow_reply_consumption();
        }
        if self.manager.as_ref().is_some_and(|manager| manager.preparation_grow_pending()) {
            if let Some(drm) = self.drm.as_ref() {
                crate::driver::queue_g17p_tvb_grow_worker(drm.clone());
            }
        }
        if let Some(manager) = self.manager.as_mut() {
            // Native AGXFirmware::handleEvent (0xfffffe0008881028) ends every
            // event interrupt, from either role, with
            // drainFirmwareRings(true) (0xfffffe0008881308): trace ring role
            // 0, trace ring role 1, then drainFirmwareStatisticsRing for both
            // roles, each entry by entry. Firmware has ungated trace sites
            // (GFX1 0x247) and time-driven statistics producers, so a zero
            // capture mask does not quiesce either ring; left undrained, both
            // producers keep notifying the host. Default-on parity; 0 keeps
            // the previous diagnostic-gated consumers for A/B.
            let native_rings = module_parameters::g17p_native_ring_drain.value() != 0
                && !crate::g17_timing_buffer::diag_knob(
                    crate::g17_timing_buffer::DIAG_NATIVE_RING_DRAIN_OFF);
            let stats_drain = crate::g17_timing_buffer::diag_knob(
                crate::g17_timing_buffer::DIAG_STATS_DRAIN);
            let msglog = crate::g17_timing_buffer::diag_knob(
                crate::g17_timing_buffer::DIAG_MSGLOG);
            if !native_rings && (stats_drain || msglog) {
                for role in [
                    g17_initdata::InstanceRole::Primary,
                    g17_initdata::InstanceRole::Secondary,
                ] {
                    if let Err(error) = manager.drain_stats_ring(role, stats_drain) {
                        dev_warn!(pdev.as_ref(),
                            "G17P event worker: statistics ring {:?} failed ({:?})\n", role, error);
                    }
                }
            } else if native_rings && msglog {
                // Cursor-only msglog record (consume=false never writes).
                for role in [
                    g17_initdata::InstanceRole::Primary,
                    g17_initdata::InstanceRole::Secondary,
                ] {
                    let _ = manager.drain_stats_ring(role, false);
                }
            }
            if native_rings || g17p_ktrace_enabled() {
                let verbose = module_parameters::g17p_fw_ktrace_verbose.value() != 0;
                let budget = module_parameters::g17p_fw_ktrace_budget.value();
                for role in [
                    g17_initdata::InstanceRole::Primary,
                    g17_initdata::InstanceRole::Secondary,
                ] {
                    if let Err(error) = manager.drain_ktrace_ring(role, verbose, budget) {
                        if native_rings {
                            // Quarantine (idempotent) and report once.
                            g17_resources::g17p_native_ring_note_error(
                                pdev.as_ref(), role, false, error);
                        } else {
                            dev_warn!(
                                pdev.as_ref(),
                                "G17P event worker: KTrace drain failed for {:?} ({:?})\n",
                                role,
                                error,
                            );
                        }
                    }
                }
            }
            if native_rings {
                for role in [
                    g17_initdata::InstanceRole::Primary,
                    g17_initdata::InstanceRole::Secondary,
                ] {
                    // Transport faults quarantine the cursor and report once.
                    if let Err(error) = manager.drain_statistics_ring_native(role) {
                        g17_resources::g17p_native_ring_note_error(
                            pdev.as_ref(), role, true, error);
                    }
                }
            }
            // The shared legacy completion waiter is armed only by the
            // synchronous/bootstrap compute path.  Render doorbells and the
            // independently tracked async-compute queues retire below from
            // their exact event identities; scanning the entire shared KSM
            // completion ring for every one of those events only holds the
            // runtime mutex and delays render publication/completion.
            if self.pending_compute.is_some() {
                if let Ok(Some(timestamps)) = manager.read_compute_completion() {
                    signal.record(G17PComputeCompletion { timestamps });
                }
            }
        }
        // Both RTKit roles notify the shared completion/event resources.
        // Consume GFX1 ingress too; otherwise a coalesced secondary-only edge
        // disappears when WorkItem1 is already running and no primary pending
        // count asks it to run again.
        let secondary_notifications = self.gfx1_state.as_ref().map_or(0, |secondary| {
            secondary.firmware_event_notifications.load(Ordering::Acquire)
        });
        let notifications_pending = flushed_pending_control
            || flushed_pending_release
            || secondary_notifications != 0
            || state.firmware_event_notifications.load(Ordering::Acquire) != 0;
        let recovery_pending = self
            .manager
            .as_ref()
            .ok_or(ENODEV)?
            .pending_primary_recovery()
            .is_some();
        let mut uma_release = None;
        let mut completion_masks = [0u64; 2];
        // Every step below can end the session, and they all surface the same
        // handful of errnos, so carry the step's name alongside the error. The
        // crashed line is the last thing this role ever logs; an unattributed
        // EIO there is not enough to tell eight different failures apart.
        let step = |name: &'static str| move |error: Error| (name, error);
        let result: core::result::Result<(), (&'static str, Error)> = if recovery_pending {
            Ok(())
        } else if self.queue_setup_pending && !notifications_pending {
            let result = self.ensure_compute_queue();
            dev_info!(
                pdev.as_ref(),
                "G17P session: deferred SKSM QID 4 / selector 2 memory prepared after recovery-gate work; submissions={}\n",
                self.accepting_submissions
            );
            result.map_err(step("ensure-compute-queue"))
        } else {
            // Not inside the labelled chain yet: this `?` leaves the function.
            let manager = self.manager.as_mut().ok_or(ENODEV)?;
            let rtkit = self.gfx_rtkit.as_mut().ok_or(ENODEV)?;
            let mut announce = || Pin::new(&mut *rtkit).send_message_logged(
                g17_rtkit::ENDPOINT_GFX_INTERRUPTS, g17_rtkit::encode_primary_device_control());
            match settle_primary_empty_recovery(&pdev, manager, &state, &mut announce) {
                Ok((release, masks)) => {
                    uma_release = release;
                    completion_masks = masks;
                    Ok(())
                }
                Err(error) => Err(("settle-primary-empty-recovery", error)),
            }
        };
        let result = result.and_then(|()| {
            if self.primary_device_control_backpressured() {
                Ok(())
            } else if let Some(release) = uma_release {
                self.service_render_usc_freelist_release(release)
                    .map_err(step("render-usc-freelist-release"))
            } else {
                Ok(())
            }
        });
        let result = result.and_then(|()| {
            if !self.primary_device_control_backpressured() {
                if let Some(secondary) = self.gfx1_state.as_ref().cloned() {
                    let rtkit = self.gfx_rtkit.as_mut().ok_or(("no-gfx-rtkit", ENODEV))?;
                    let mut announce = || Pin::new(&mut *rtkit).send_message_logged(
                        g17_rtkit::ENDPOINT_GFX_INTERRUPTS,
                        g17_rtkit::encode_primary_device_control(),
                    );
                    let batch = drain_primary_firmware_event_batch(
                        &pdev,
                        self.manager.as_mut().ok_or(("no-manager", ENODEV))?,
                        &secondary,
                        &mut announce,
                    )
                    .map_err(step("drain-gfx1-event-ring"))?;
                    completion_masks[0] |= batch.completion_masks[0];
                    completion_masks[1] |= batch.completion_masks[1];
                    if let Some(release) = batch.uma_release {
                        self.service_render_usc_freelist_release(release)
                            .map_err(step("gfx1-render-usc-freelist-release"))?;
                    }
                    if let Some(generation) = batch.recovery_generation {
                        // The firmware raises ONE restart request and both roles can
                        // see it, so this is routinely the same request the gfx ring
                        // queued a few lines ago. `queue_primary_recovery` answers
                        // that with EBUSY, and EBUSY here killed the session.
                        let queued = self.manager.as_mut().ok_or(("no-manager", ENODEV))?
                            .queue_or_coalesce_primary_recovery(
                                generation,
                                batch.recovery_records,
                                batch.blamed_stamp,
                            )
                            .map_err(step("queue-gfx1-recovery"))?;
                        if !queued {
                            dev_info!(
                                pdev.as_ref(),
                                "G17P recovery: gfx1 type4 generation={} records={} folded into the request the gfx ring already queued\n",
                                generation,
                                batch.recovery_records,
                            );
                        }
                    }
                }
            }
            self.retain_render_completion_masks(completion_masks);
            self.service_async_compute_completion_masks(completion_masks)
                .map_err(step("async-compute-completions"))
        });
        match result {
            Ok(()) => Ok(()),
            Err((reason, error)) => {
                note_state_crashed(&pdev, &state, reason, error, None);
                Err(error)
            }
        }
    }

    /// Service firmware events inline while a submission is in flight.
    ///
    /// The submit path holds the runtime mutex for the whole wait, so the
    /// workqueue event worker cannot acquire it until submit returns. The
    /// firmware's handshake (`state == 1`, "waiting for first response",
    /// raised together with the GPC power-up) therefore never gets an answer
    /// inside the submit window -- observed on J700 as the worker running
    /// 1.6s late, after the 625ms timeout had already expired. Drain and
    /// service here, where the lock is already held.
    fn service_firmware_events_inline(&mut self) -> Result<u32> {
        self.note_service_pass_entry();
        let pdev = self.pdev.as_ref().cloned().ok_or(ENODEV)?;
        let state = self.gfx_state.as_ref().cloned().ok_or(ENODEV)?;
        let pending_control = self.primary_tvb_reply_pending();
        let pending_release = self
            .manager
            .as_ref()
            .is_some_and(|manager| manager.pending_render_usc_freelist_release().is_some());
        if state.firmware_event_notifications.load(Ordering::Acquire) == 0
            && !pending_control
            && !pending_release
        {
            return Ok(0);
        }
        if pending_control && !self.flush_pending_primary_device_control()? {
            return Ok(0);
        }
        if pending_release && !self.flush_pending_render_usc_freelist_release()? {
            return Ok(0);
        }
        let batch = {
            let manager = self.manager.as_mut().ok_or(ENODEV)?;
            let rtkit = self.gfx_rtkit.as_mut().ok_or(ENODEV)?;
            let mut announce = || Pin::new(&mut *rtkit).send_message_logged(
                g17_rtkit::ENDPOINT_GFX_INTERRUPTS, g17_rtkit::encode_primary_device_control());
            drain_primary_firmware_event_batch(&pdev, manager, &state, &mut announce)?
        };
        let completion_masks = batch.completion_masks;
        for slot in 0..g17_resources::G17PRenderQueuePair::SLOT_COUNT as usize {
            if self.pending_render[slot].is_some() {
                self.render_event_masks[slot][0] |= completion_masks[0];
                self.render_event_masks[slot][1] |= completion_masks[1];
            }
        }
        // This path owns the runtime mutex and has consumed the primary event
        // records plus their notification count. Route compute completions
        // here as well; the ordinary worker cannot rediscover this edge after
        // the inline drain returns. Do this before any other fallible service
        // or diagnostic recovery early-return so a mixed batch loses no QID.
        self.service_async_compute_completion_masks(completion_masks)?;
        if let Some(release) = batch.uma_release {
            self.service_render_usc_freelist_release(release)?;
        }
        if let Some(generation) = batch.recovery_generation {
            // Diagnostic cap: the J700 firmware re-raises a fault-free
            // recovery immediately after every final clear, ~500 times per
            // submit window. Capping lets us tell whether our own servicing
            // sustains the loop or the firmware would raise it regardless.
            let limit = module_parameters::g17p_max_recoveries.value();
            // 0xffff = service none at all. Discriminates "the firmware halts
            // KSM when it *requests* recovery" from "our answer is what leaves
            // it halted".
            if limit == 0xffff {
                return Ok(batch.notification_count);
            }
            let serviced = INLINE_RECOVERIES_SERVICED.load(Ordering::Relaxed);
            if limit != 0 && serviced >= limit {
                if serviced == limit {
                    INLINE_RECOVERIES_SERVICED.store(limit + 1, Ordering::Relaxed);
                    dev_info!(
                        pdev.as_ref(),
                        "G17P recovery: inline servicing capped at {}; ignoring further requests\n",
                        limit
                    );
                }
                return Ok(batch.notification_count);
            }
            INLINE_RECOVERIES_SERVICED.fetch_add(1, Ordering::Relaxed);
            self.manager
                .as_mut()
                .ok_or(ENODEV)?
                .queue_or_coalesce_primary_recovery(
                    generation,
                    batch.recovery_records,
                    batch.blamed_stamp,
                )?;
            self.service_primary_recovery_nested()?;
        }
        self.service_async_render_completions([0; 2])?;
        Ok(batch.notification_count)
    }

    /// Top-level recovery service (the recovery worker's outermost hold):
    /// may rebuild the session in place when one was requested.
    pub(crate) fn service_primary_recovery(&mut self) -> Result {
        if !self.recovery_service_admitted()? { return Ok(()); }
        if self.service_requested_session_rebuild()? { return Ok(()); }
        self.service_primary_recovery_body()
    }

    /// Recovery service from inline event service, which can run inside a
    /// publication. Identical except that it never rebuilds the session in
    /// place: a requested rebuild already queued the recovery worker, which
    /// performs it at its own top level once this hold ends. The rebuild
    /// replaces the session, so the pending firmware recovery is not serviced
    /// here either -- exactly what the top-level path does after rebuilding.
    /// Keeps the session rebuild (and `boot_session`) off nested stacks
    /// (tests/audit_g17_publication_stack.py).
    fn service_primary_recovery_nested(&mut self) -> Result {
        if !self.recovery_service_admitted()? { return Ok(()); }
        if self.session_rebuild_requested {
            if let Some(drm) = self.drm.as_ref() {
                crate::driver::queue_g17p_recovery_worker(drm.clone());
            }
            return Ok(());
        }
        self.service_primary_recovery_body()
    }

    /// Common admission: a pending recovery, the device present, and every
    /// preparation drained (otherwise the recovery worker joins them first).
    fn recovery_service_admitted(&mut self) -> Result<bool> {
        if !self.primary_recovery_pending() { return Ok(false); }
        if self.preparations.is_removed() { return Err(ENODEV); }
        self.preparations.close(false);
        if !self.preparations.is_drained() {
            if let Some(drm) = self.drm.as_ref() {
                crate::driver::queue_g17p_recovery_worker(drm.clone());
            }
            return Ok(false);
        }
        Ok(true)
    }

    #[inline(never)]
    fn service_primary_recovery_body(&mut self) -> Result {
        self.note_service_pass_entry();
        let pdev = self.pdev.as_ref().cloned().ok_or(ENODEV)?;
        let state = self.gfx_state.as_ref().cloned().ok_or(ENODEV)?;
        for (pending_slot, completion) in self.pending_render.iter().enumerate() {
            let Some(completion) = completion.as_ref() else { continue };
            let (submission_id, vdm_base, lease_slot, signalled) =
                completion.diagnostic_identity();
            dev_err!(
                pdev.as_ref(),
                "G17P fault correlation: pending-slot={} lease-slot={} submission={} vdm={:#x} signalled={} fragment-dependency={:#x}\n",
                pending_slot,
                lease_slot,
                submission_id,
                vdm_base,
                signalled,
                completion.diagnostic_fragment_dependency(),
            );
        }
        for (pending_slot, completion) in self.pending_render_queued.iter().enumerate()
            .flat_map(|(slot, queued)| queued.iter().map(move |completion| (slot, completion)))
        {
            let (submission_id, vdm_base, lease_slot, signalled) =
                completion.diagnostic_identity();
            dev_err!(
                pdev.as_ref(),
                "G17P fault correlation: queued-slot={} lease-slot={} submission={} vdm={:#x} signalled={} fragment-dependency={:#x}\n",
                pending_slot, lease_slot, submission_id, vdm_base, signalled,
                completion.diagnostic_fragment_dependency(),
            );
        }
        // Sample the queue/firmware agreement either side of the recovery.
        // A boot with no fault hands the retained graph to any number of
        // sequential clients; one GMMU fault in between and every later kick is
        // accepted, retired and never executed. Whatever moves across this
        // boundary is the mechanism, so measure it rather than infer it. Host
        // DRAM only -- see `log_compute_recovery_alignment`.
        self.log_recovery_alignment("pre-recovery");
        let registers = self.registers.as_ref();
        let primary_queue = self.queue.as_mut();
        let async_compute_queues = &mut self.async_compute_queues;
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        let Some((generation, records, blamed_stamp)) = manager.pending_primary_recovery() else {
            return Ok(());
        };
        let result = complete_primary_real_recovery(
            &pdev,
            manager,
            primary_queue,
            async_compute_queues,
            registers,
            &state,
            generation,
            records,
        )
        .map_err(|error| ("complete-real-recovery", error))
        .and_then(|()| {
            manager
                .complete_pending_primary_recovery(generation, records, blamed_stamp)
                .map_err(|error| ("complete-pending-recovery", error))
        });
        let result = match result {
            Ok(()) => Ok(()),
            Err((reason, error)) => {
                // Same permanent-deafness transition as the event worker, and
                // this one also skips the event-drain re-arm at the end of this
                // function.
                note_state_crashed(&pdev, &state, reason, error, Some(generation));
                self.preparations.fail();
                return Err(error);
            }
        };
        // Recovery isolation: classify every in-flight completion against this
        // recovery's guilty set BEFORE anything below can fail it.
        let (guilty_qids, _spared_cancelled) = if crate::g17_recovery_isolation::enabled(
            crate::g17_recovery_isolation::INNOCENT)
        {
            self.classify_recovery_victims(blamed_stamp)
        } else { (blamed_stamp.map_or(0, |qid| 1u128 << (qid & 127)), 0) };
        // Classification may have re-attributed the firmware's original QID.
        // Never settle that original owner after classifying it as innocent.
        let mut settled_slots = 0u64;
        for qid in 0..128u8 {
            if guilty_qids & (1u128 << qid) == 0 { continue; }
            let Some(slot) = self.manager.as_ref()
                .and_then(|manager| manager.render_slot_for_qid(qid)) else { continue; };
            if settled_slots & (1u64 << slot) != 0 { continue; }
            settled_slots |= 1u64 << slot;
            if self.pending_render[usize::from(slot)].as_ref()
                .is_some_and(|completion| completion.recovery_spared()) { continue; }
            self.settle_render_blamed_by_recovery(qid, EIO);
        }
        self.service_spared_recovery_renders()?;
        // The recovery acknowledged its final clear at state 0: the firmware
        // is alive and reset. Release the compute admission poison here;
        // otherwise only a session rebuild (which ends in rtkit-boot ETIME)
        // could clear it and one fault locked every later client out.
        if let Some(registry) = self.queue_registry.as_ref() {
            if registry.clear_compute_poison() {
                dev_info!(pdev.as_ref(),
                    "G17PMARK recovery cleared the device-wide compute admission poison\n");
            }
        }
        // Compute completions observed while the recovery was open were left
        // armed. Their completion edges may already have been consumed, so
        // re-scan every active queue now rather than waiting for the watchdog.
        if let Err(error) = self.service_async_compute_completions() {
            dev_warn!(pdev.as_ref(),
                "G17P recovery: post-recovery compute scan failed ({:?})\n", error);
        }
        // The scan above re-reads quarantined commands too, but it can fail
        // before reaching them (the warning above). Firmware has just retired
        // whatever the recovery cancelled; observe that independently.
        self.release_firmware_retired_quarantined_compute_nested("post-recovery");
        self.log_recovery_alignment("post-recovery");
        // Realign before the next kick. Continuity across a recovery is exactly
        // the assumption the evidence contradicts.
        if let (Some(manager), Some(queue)) = (self.manager.as_mut(), self.queue.as_mut()) {
            if let Err(error) = manager.resynchronise_compute_queue_after_recovery(queue) {
                dev_warn!(
                    pdev.as_ref(),
                    "G17P recovery: post-recovery queue resync failed ({:?}); the graph keeps its pre-recovery state\n",
                    error
                );
            }
        }
        // The batch drain deliberately stops at type 4 so restartWorkQueue can
        // release both native command gates before the next record is read.
        // Its notification count was already swapped to zero, however, and a
        // type-1/type-13 record may already follow type 4 in the same producer
        // snapshot.  Schedule one continuation after every successful actual
        // recovery; queue_work coalescing makes a concurrent fresh IRQ share
        // this pass, while an empty ring costs one bounded drain.  Keeping the
        // continuation here covers both the workqueue and inline callers.
        if !self.is_crashed() && !self.primary_recovery_pending() {
            self.preparations.reopen();
            // Recovery isolation bit4 (opt-in): one paired doorbell when a
            // spared render the firmware did not cancel is still pending.
            if crate::g17_recovery_isolation::enabled(crate::g17_recovery_isolation::RESUME_DOORBELL)
                && self.pending_render.iter().flatten().any(|completion| completion.recovery_spared())
            {
                let sent = match self.gfx_rtkit.as_mut() {
                    Some(rtkit) => Pin::new(rtkit).send_message_logged(
                        g17_rtkit::ENDPOINT_GFX_INTERRUPTS,
                        g17_submission::G17P_PARTIAL_OPENING_PAIRED_DOORBELL),
                    None => Err(ENODEV),
                };
                if sent.is_ok() {
                    crate::g17_recovery_isolation::count(&crate::g17_recovery_isolation::RESUME_DOORBELLS);
                }
                dev_info!(pdev.as_ref(),
                    "G17P recovery isolation: post-recovery paired doorbell for pending spared renders result={:?}\n",
                    sent.map(|_| ()));
            }
        }
        crate::driver::queue_g17p_firmware_event_worker(state.dev.clone());
        result
    }

    /// One recovery-boundary sample of the retained queue, if there is one.
    fn log_recovery_alignment(&mut self, label: &'static str) {
        if let (Some(manager), Some(queue)) = (self.manager.as_mut(), self.queue.as_mut()) {
            manager.log_compute_recovery_alignment(queue, label);
        }
    }

    pub(crate) fn drm_ref(&self) -> Result<ARef<AsahiDevice>> {
        self.drm.as_ref().cloned().ok_or(ENODEV)
    }

    pub(crate) fn is_crashed(&self) -> bool {
        self.platform_removed || self.preparations.is_removed() || g17_lifecycle::persistent_session_needs_recovery(
            self.session_recovery_pending,
            self.gfx_state
                .as_ref()
                .is_some_and(|state| state.crashed.load(Ordering::Acquire)),
            self.gfx1_state
                .as_ref()
                .is_some_and(|state| state.crashed.load(Ordering::Acquire)),
        )
    }

    /// Power-gate the whole GFX domain and bring it back, so both ASCs
    /// restart from their reset vectors.
    ///
    /// `482600000.gpu` sits in the same `gfx` genpd as both ASCWrap mailbox
    /// providers. This driver has never called into runtime PM at all, and
    /// `genpd_power_off()` counts every device that is not
    /// `pm_runtime_suspended()` -- which a device whose runtime PM was never
    /// enabled never is -- so while asahi is bound the domain is pinned on.
    /// That is why a session rebuild's entire hardware effect is one
    /// `CPU_RUN` write, and why the following `apple_rtkit_boot` waits a
    /// second for a HELLO that a processor which was never reset has no
    /// reason to send.
    ///
    /// The boot log shows what a genuine gate does instead. The value the
    /// PMGR driver reads back immediately before asahi's own probe turns the
    /// domain on is `0x0f002300`: `PS_TARGET` and `PS_ACTUAL` both zero with
    /// `WAS_PWRGATED` and `WAS_CLKGATED` set. The ASC that comes out of that
    /// gate volunteers HELLO in about 1.2 ms, with no firmware upload and no
    /// reset-vector write from Linux. So the restart primitive works; the
    /// driver simply never invokes it after probe.
    fn cycle_gfx_power_domain(&mut self, pdev: &platform::Device) -> Result {
        let dev = pdev.as_ref().as_raw();
        if !self.runtime_pm_armed {
            // Publish that the device is on BEFORE enabling runtime PM.
            // Enabling first would leave it reading as suspended, and the next
            // mailbox suspend would then gate the domain under a live driver.
            // SAFETY: runtime PM is still disabled for this device, which is
            // the documented precondition for `pm_runtime_set_active()`.
            let set_active = unsafe { __pm_runtime_set_status(dev, RPM_ACTIVE) };
            // SAFETY: `dev` is this driver's own bound device; the devres
            // action keeps the enable balanced across unbind.
            let enable = unsafe { devm_pm_runtime_enable(dev) };
            // `pm_runtime_get_sync()`: the reference that keeps the domain up
            // outside a deliberate cycle.
            // SAFETY: as above.
            let get = unsafe { __pm_runtime_resume(dev, RPM_GET_PUT) };
            dev_info!(
                pdev.as_ref(),
                "G17PMARK asc-powercycle arm set_active={} enable={} get={}\n",
                set_active,
                enable,
                get
            );
            if set_active < 0 || enable < 0 || get < 0 {
                return Err(EIO);
            }
            self.runtime_pm_armed = true;
        }

        if crate::g17_config::ASC_TEARDOWN & 1 != 0 {
            wait_for_g17p_asc_idle(pdev, "pre-gate");
        }

        // Both CPUs are stopped and both RTKit owners are gone, so each
        // mailbox has already dropped its runtime reference -- asynchronously.
        // Flush those before asking the domain down, or genpd counts them as
        // still active and refuses the whole gate.
        for index in 0..2usize {
            let provider = G17PMailboxProvider::get(pdev.as_ref(), index)?;
            // SAFETY: the provider owns this device for as long as the device
            // link taken by `get()` is held.
            let barrier = unsafe { pm_runtime_barrier(provider.provider_device()) };
            dev_info!(
                pdev.as_ref(),
                "G17PMARK asc-powercycle mailbox{} barrier={}\n",
                index,
                barrier
            );
        }

        // Nothing below may touch ASC or SGX MMIO: the aperture is gone while
        // the domain is gated, and a read would take an external abort. The
        // PMGR provider logs the transition itself, which is the witness.
        // `pm_runtime_put_sync_suspend()`.
        // SAFETY: this driver's own device, runtime PM armed above.
        let off = unsafe { __pm_runtime_suspend(dev, RPM_GET_PUT) };
        // The GFX PMGR register latches BUSY (bit 11) across the power-down and
        // clears it on its own some time later. `apple_pmgr_ps_set` rebuilds the
        // value it writes from a fresh read and its clear-mask does not include
        // BUSY, so an ON issued while BUSY is still set writes BUSY back -- and
        // measured on J700 that write is ignored outright (requested TARGET=0xf,
        // register unmoved at 0x0f002b00) and the domain stays dead for the rest
        // of the boot. The same boot's control is the proof: an identical
        // power-down at t=0.8s also left BUSY set, nothing wrote the register,
        // and by the time probe issued its ON at t=16s BUSY had cleared and the
        // ON succeeded. A fixed 10ms was simply losing that race. Waiting longer
        // is a measurement of how long BUSY needs, not a fix; the fix belongs in
        // pmgr-pwrstate.c, which should mask BUSY out of the written value and
        // poll it clear before the ON the way it already polls it after.
        fsleep(Delta::from_millis(
            (crate::g17_config::ASC_GATE_SETTLE_MS).clamp(1, 2000) as i64,
        ));
        // `pm_runtime_resume_and_get()`.
        // SAFETY: as above.
        let on = unsafe { __pm_runtime_resume(dev, RPM_GET_PUT) };
        dev_info!(
            pdev.as_ref(),
            "G17PMARK asc-powercycle gate off_rc={} on_rc={} -- a GFX PMGR set-target pair either side of this line is the proof the domain actually cycled\n",
            off,
            on
        );
        if on < 0 {
            return Err(EIO);
        }
        Ok(())
    }

    /// Say how far each role's firmware `__DATA` has drifted from the copy
    /// iBoot left, and at restart mode 3 put it back.
    ///
    /// A power-cycled ASC re-enters at `IORVBAR`, which is the `__TEXT` base:
    /// the code is pristine, but every global the previous firmware instance
    /// wrote is still sitting in `__DATA`. Measured on J700, a restarted
    /// processor does execute and then immediately sends on the crashlog
    /// endpoint instead of HELLO, which is exactly what re-entering over dirty
    /// globals looks like. The drift count says whether that reading is right;
    /// the restore tests it.
    #[cfg(CONFIG_DEV_COREDUMP)]
    fn report_and_restore_firmware_data(&self, pdev: &platform::Device, restore: bool) {
        let Some(snapshot) = self.firmware_data.as_ref() else {
            dev_warn!(
                pdev.as_ref(),
                "G17PMARK fw-restore unavailable: no pristine __DATA snapshot was taken at probe\n"
            );
            return;
        };
        for (name, image, pristine) in [
            ("gfx", &G17P_GFX_FIRMWARE_IMAGE, &snapshot.gfx),
            ("gfx1", &G17P_GFX1_FIRMWARE_IMAGE, &snapshot.gfx1),
        ] {
            match count_g17p_firmware_data_drift(image, pristine) {
                Ok(drift) => dev_info!(
                    pdev.as_ref(),
                    "G17PMARK fw-drift role={} bytes_changed={} of {} at {:#x} read=invalidated\n",
                    name,
                    drift,
                    image.data_size,
                    image.data_phys,
                ),
                Err(error) => dev_warn!(
                    pdev.as_ref(),
                    "G17PMARK fw-drift role={} unreadable ({:?})\n",
                    name,
                    error
                ),
            }
            if restore {
                let result = crate::pgtable::UatPageTable::restore_firmware_physical_range(
                    image.data_phys,
                    pristine,
                );
                dev_info!(
                    pdev.as_ref(),
                    "G17PMARK fw-restore role={} ok={} ({:#x}+{:#x})\n",
                    name,
                    result.is_ok(),
                    image.data_phys,
                    image.data_size,
                );
            }
        }
    }

    /// Close admission before destructive reset. Active builders finish or
    /// reject their epoch with runtime released; the worker supplies that join.
    fn rebuild_session_or_defer(&mut self) -> Result {
        if self.platform_removed || self.preparations.is_removed() { return Err(ENODEV); }
        self.preparations.close(false);
        if !self.preparations.is_drained() {
            return Err(self.request_session_rebuild(EAGAIN));
        }
        let result = g17_lifecycle::recover_persistent_runtime(self);
        if result.is_err() { self.preparations.fail(); }
        result
    }

    /// Failed publication is not an invisibility witness. Keep the manager,
    /// pending completions and every DMA owner intact, close admission, and
    /// schedule recovery independently of any subsequent firmware interrupt.
    /// The caller still reports the original operation's failure immediately.
    fn request_session_rebuild(&mut self, cause: Error) -> Error {
        if self.platform_removed || self.preparations.is_removed() { return cause; }
        self.preparations.close(false);
        self.accepting_submissions = false;
        note_session_recovery_pending(
            &mut self.session_recovery_pending, self.pdev.as_ref(),
            "publication-rebuild-requested",
        );
        if !self.session_rebuild_requested {
            self.session_rebuild_requested = true;
            if let Some(drm) = self.drm.as_ref() {
                crate::driver::queue_g17p_recovery_worker(drm.clone());
            }
        }
        cause
    }

    /// Legacy compute carries its timestamp mapping as a local during direct
    /// doorbell publication. Transfer it before returning an asynchronous error.
    fn request_compute_session_rebuild(
        &mut self, cause: Error, timestamps: mmu::KernelMapping,
    ) -> Error {
        self.compute_timestamps = Some(timestamps);
        self.request_session_rebuild(cause)
    }

    /// Consume at most one coalesced host request. This is separate from
    /// firmware's generation-tagged recovery record. The wrapper owns deferred
    /// fence signalling, so reset settles DMA owners before callbacks run.
    fn service_requested_session_rebuild(&mut self) -> Result<bool> {
        if !self.session_rebuild_requested { return Ok(false); }
        if self.platform_removed {
            self.session_rebuild_requested = false;
            return Err(ENODEV);
        }
        self.session_rebuild_requested = false;
        self.rebuild_session_or_defer()?;
        Ok(true)
    }

    /// Rebuild a crashed session so a NEW client can still open the device.
    ///
    /// `file.rs::get_params` refuses with ENODEV while the session is crashed,
    /// and that check runs before any ioctl that could trigger the submit
    /// path's own recovery. So a single crashed session made every subsequent
    /// process fail at its first ioctl and the only way out was a reboot --
    /// which is exactly the cost a run of deliberately-failing workloads must
    /// not pay. `rebuild_session_in_place` installs fresh role state with
    /// `crashed` clear, so a recovery genuinely clears the condition.
    ///
    /// Bounded and loud: at most `CRASH_RECOVERY_ATTEMPTS` per module load, so
    /// a firmware that is really dead reports ENODEV instead of being restarted
    /// once per ioctl forever.
    pub(crate) fn recover_crashed_session_once(&mut self) -> bool {
        if self.platform_removed || self.preparations.is_removed() { return true; }
        if self.session_rebuild_requested { return true; }
        const CRASH_RECOVERY_ATTEMPTS: u32 = 4;
        if !self.is_crashed() {
            return false;
        }
        if crate::g17_config::ABANDON_RECOVERY == 0
            || self.crash_recoveries >= CRASH_RECOVERY_ATTEMPTS
        {
            return true;
        }
        self.crash_recoveries += 1;
        let attempt = self.crash_recoveries;
        let result = self.rebuild_session_or_defer();
        if let Some(pdev) = self.pdev.as_ref() {
            match result {
                Ok(()) => dev_info!(
                    pdev.as_ref(),
                    "G17P session: crashed session rebuilt on attempt {}/{}; the device is open to new clients again\n",
                    attempt,
                    CRASH_RECOVERY_ATTEMPTS
                ),
                Err(error) => dev_err!(
                    pdev.as_ref(),
                    "G17P session: crashed-session rebuild attempt {}/{} failed ({:?}); reporting ENODEV\n",
                    attempt,
                    CRASH_RECOVERY_ATTEMPTS,
                    error
                ),
            }
        }
        self.is_crashed()
    }

    pub(crate) fn mark_firmware_cache_flush_ready(&self) -> Result {
        self.manager
            .as_ref()
            .ok_or(ENODEV)?
            .mark_firmware_cache_flush_ready();
        Ok(())
    }

    /// Create one client address space.
    ///
    /// Gated on the compute UAPI *stage* only -- which is what the selftest's
    /// "use g17p_test_stage=2" hint refers to -- and never on
    /// `accepting_submissions`. An `mmu::Vm` is page tables and nothing else:
    /// it needs no firmware queue, takes no UAT context slot until it is
    /// bound, and every client may hold one at the same time. Gating it on
    /// `accepting_submissions` made VM_CREATE fail with ENOTSUPP for the whole
    /// window between a session recovery and the workqueue that re-prepared
    /// QID 4 -- which is why a second client after a faulting one saw
    /// "VM_CREATE: Unknown error 524".
    pub(crate) fn new_user_vm(
        &mut self,
        id: u64,
        kernel_range: core::ops::Range<u64>,
    ) -> Result<mmu::Vm> {
        if !self.test_stage.enables_compute_uapi() {
            return Err(ENOTSUPP);
        }
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .new_user_vm(id, kernel_range)
    }

    pub(crate) fn map_timestamp_buffer(
        &self,
        bo: gem::ObjectRef,
        range: core::ops::Range<usize>,
    ) -> Result<mmu::KernelMapping> {
        if !self.test_stage.enables_compute_uapi() {
            return Err(ENOTSUPP);
        }
        self.manager
            .as_ref()
            .ok_or(ENODEV)?
            .map_timestamp_buffer(bo, range)
    }

    /// Build the retained QID 4 graph if this session has none.
    ///
    /// Normal startup leaves this graph absent so first render does not depend
    /// on compute-owned allocations or their live UAT mappings. The compute
    /// submit path calls this synchronously, including after recovery; only
    /// the explicit QID4 diagnostic asks the event worker to prepare it. This
    /// allocates and encodes mapped memory, but sends no firmware message and
    /// does not register the queue or issue compute work.
    pub(crate) fn ensure_compute_queue(&mut self) -> Result {
        if !self.test_stage.configures_qid4() {
            return Ok(());
        }
        if self.queue.is_none() {
            let drm = self.drm.as_ref().ok_or(ENODEV)?.clone();
            let queue = self
                .manager
                .as_mut()
                .ok_or(ENODEV)?
                .prepare_g17p_compute_queue(&drm)?;
            self.queue = Some(queue);
        }
        self.queue_setup_pending = false;
        self.accepting_submissions = self.test_stage.enables_compute_uapi();
        Ok(())
    }

    /// Number of DRM queue objects one firmware session can hand the hardware
    /// compute graph to before a full session recycle is forced.
    ///
    /// Each handoff retires the previous graph instead of freeing it (see
    /// `retired_queues`), so this is the bound on how much memory a session
    /// may park that way. The firmware's KSM restore set has 128 queue slots,
    /// which is the ceiling on *concurrently configured* QIDs -- a different
    /// quantity, and one this driver is nowhere near: it configures exactly
    /// one compute QID, because the SKSM tag/data port is unreachable from the
    /// AP (`g17p_sksm_mmio`) and the firmware only knows the QID the opening
    /// installed.
    const RETIRED_QUEUE_LIMIT: usize = 8;

    /// Take ownership of the hardware compute graph for `owner`.
    ///
    /// Same owner and nothing released in between: the retained graph and
    /// binding stay, which is what makes a steady-state resubmit possible.
    /// Anything else is a handoff.
    fn acquire_compute_owner(&mut self, owner: u64) -> Result {
        if self.compute_owner == Some(owner) && !self.compute_recycle_pending {
            return Ok(());
        }
        if self.compute_owner.is_some() || self.compute_recycle_pending {
            self.handoff_compute_queue()?;
        }
        self.compute_owner = Some(owner);
        Ok(())
    }

    /// Hand the one hardware compute queue from one DRM client to the next.
    ///
    /// ROOT CAUSE this exists to fix (second DRM client on one boot: submit
    /// accepted, no fault, no blamed queue, doorbell wait times out):
    ///
    ///  * The firmware installs a QID from the tag-15 ConfigUpdate's
    ///    queue-install flag at `+0x1a`. Firmware RE recorded in
    ///    `g17_compute::encode_compute_config_update` says the sole writer of
    ///    the per-queue slot `0x127628 + qid*0x28` -- together with
    ///    `slot[+0x00] = 1`, the per-data-master active bitmap and the
    ///    `sgx+0x21008` queue-base store -- is reachable only when that flag is
    ///    non-zero, and that leaving it set on an ALREADY-installed queue
    ///    forces a disable/reprogram/enable cycle on the KSMFE queue, whose
    ///    observed symptom is precisely "published fine and then never
    ///    completed".
    ///  * Nothing on the host can un-install it. `unregister_compute_sksm_queue`
    ///    issues `disable_pair` through `g17p_sksm_publish_write_pair`, which is
    ///    a logged no-op whenever `g17p_sksm_mmio == 0` -- the default, and the
    ///    only setting that does not SError on J700. So after the first client's
    ///    first bind, QID 4 stays installed in the firmware and in KSMFE for the
    ///    rest of the boot.
    ///  * Both previous handoff modes then re-ran the FIRST-BIND chronology on
    ///    that still-installed QID with the install flag set again -- mode 1
    ///    because the replacement graph is a fresh `PreparedMemory` object, and
    ///    mode 0 because a rebuilt session's queue is fresh for the same reason.
    ///    That is why the two modes fail identically, and why the failure is
    ///    deterministic on the SECOND binding rather than being a race.
    ///
    /// The fix is to stop rebuilding the queue at all. The QID 4 graph, its
    /// item ring, its SKSM stamp sequence and the firmware's installed queue
    /// slot are properties of the firmware SESSION, not of a DRM client; the
    /// only per-client state is the user-VM binding and the timestamp page.
    /// Releasing just those and letting the next client bind its own VM makes
    /// its first submission take the same steady-state chronology (tag 3 ->
    /// 15 with the install flag CLEAR -> 14 -> 16 -> CL_2, appended at the
    /// live producer) that already runs 300/300 within one client.
    ///
    /// Falls back to the old rebuild path whenever the retained graph cannot
    /// be trusted: a crashed session, a submission still in flight, a queue the
    /// firmware never installed, or an exhausted quarantine budget. NOTE that
    /// the fallback is itself broken for the NEXT client, for the reason above
    /// -- it is a last resort, not a safe default, and the
    /// `G17P TAG15-INSTALL ... flag=1` line names it whenever it happens.
    fn handoff_compute_queue(&mut self) -> Result {
        // A closed core-power / KSM pause gate is the one condition retention
        // cannot survive. Measured: before a fault the GPU executes (retire
        // duration 0xda-0xdd), after a fault-blamed recovery every submission
        // on the retained graph retires with duration 0 having executed
        // nothing, the pre/post recovery alignment is byte-identical, and the
        // one host lever we have (device-control IdlePowerOff(0), opcode 0x0a
        // arg 0 -- `g17p_power_wire=1`) does not clear it. Nothing about the
        // QUEUE diverged, so resynchronising the queue cannot help; the only
        // state in which execution has ever been observed to return is a fresh
        // firmware session. So rebuild -- and rebuild with a rotated QID, or
        // the rebuilt session's first bind re-asserts the tag-15 install flag
        // on a QID the firmware still has installed and fails for the
        // completely different reason this work started from.
        if self.compute_gate_suspect && crate::g17_config::ZERO_DURATION >= 2 {
            // Time the rebuild. Whether thousands of these become a bottleneck
            // across a conformance run is a question about milliseconds, and
            // nobody has measured one; guessing at a cheaper recovery before
            // knowing what the expensive one costs is how effort gets wasted.
            let started = Instant::<Monotonic>::now();
            let result = self.recycle_compute_queue();
            let elapsed_us = started.elapsed().as_micros_ceil();
            self.compute_gate_suspect = false;
            if let Some(pdev) = self.pdev.as_ref() {
                let (installs, zeros, _, retains) = g17_manager::g17p_mark_counts();
                let rebuilds = g17_manager::G17P_MARK_GATE_REBUILDS
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                dev_err!(
                    pdev.as_ref(),
                    "G17PMARK gate-rebuild i={} z={} r={} h={} took {}us ok={} -- firmware session rebuilt after a zero-duration retire instead of retaining the gated graph\n",
                    installs,
                    zeros,
                    rebuilds,
                    retains,
                    elapsed_us,
                    result.is_ok(),
                );
            }
            return result;
        }
        if crate::g17_config::QUEUE_RECYCLE >= 2
            && self.compute_queue_can_rebind()
        {
            return self.rebind_compute_queue();
        }
        self.recycle_compute_queue()
    }

    /// How many abandoned clients' bindings one firmware session may park.
    ///
    /// Each entry pins one failed client's page tables and mapped GEM objects
    /// until both CPUs stop, so this bounds how much memory a run of failing
    /// workloads may hold. Past the limit the handoff falls back to the full
    /// rebuild, which stops both CPUs and frees the whole quarantine at once.
    const QUARANTINED_BINDING_LIMIT: usize = 64;

    /// Whether the retained QID 4 graph may be handed on with a bare rebind.
    ///
    /// `compute_abandoned` is deliberately NOT a refusal any more. It used to
    /// be, on the reasoning that only a CPU stop makes releasing an
    /// outstanding client's binding safe -- but the fallback that reasoning
    /// selects is a queue REBUILD, and a rebuilt queue re-asserts the tag-15
    /// install flag against a QID the firmware already has installed, which is
    /// the very failure `handoff_compute_queue` exists to avoid. So one
    /// faulting client poisoned every client after it for the rest of the boot.
    ///
    /// The safety property is kept a different way: the outgoing binding is
    /// QUARANTINED rather than dropped (see `rebind_compute_queue`), so nothing
    /// the firmware might still name is unmapped or freed, and no CPU stop is
    /// needed to establish that.
    ///
    /// `pending_compute` still refuses: that is a submission this driver is
    /// actively waiting on, not one it has given up on.
    fn compute_queue_can_rebind(&self) -> bool {
        if self.is_crashed() || self.pending_compute.is_some() {
            return false;
        }
        if self.compute_abandoned
            && (crate::g17_config::ABANDON_RECOVERY == 0
                || self.quarantined_bindings.len() >= Self::QUARANTINED_BINDING_LIMIT)
        {
            return false;
        }
        match (self.queue.as_ref(), self.manager.as_ref()) {
            (Some(queue), Some(manager)) => manager.compute_queue_is_installed(queue),
            _ => false,
        }
    }

    /// Release only the outgoing client's state and keep the installed graph.
    ///
    /// Order matters: the owned timestamp page is a mapping in the outgoing
    /// VM, so it goes before the binding that holds that VM alive. Dropping the
    /// binding releases UAT slot 1 and lets `VmInner::drop` retract the ctx2/3
    /// aliases it published; the next `bind_compute_user_vm` republishes them
    /// for the incoming VM before anything is kicked.
    ///
    /// Nothing firmware-visible is touched: no SKSM transaction, no tag-15
    /// install, no CPU stop. The firmware keeps naming the same entry ring,
    /// descriptor storage and channel control pages, none of which are freed.
    fn rebind_compute_queue(&mut self) -> Result {
        self.compute_owner = None;
        self.compute_recycle_pending = false;
        let abandoned = self.compute_abandoned;
        drop(self.compute_timestamps.take());
        if abandoned {
            // The outgoing client published a kick that was never seen to
            // retire. Park its memory instead of dropping it -- see
            // `G17PQuarantinedComputeBinding` -- and repair the accounting the
            // abandoned kick leaked on the retained graph, so the incoming
            // client submits against a queue that is merely FURTHER ALONG
            // rather than one that is broken.
            if let Some(binding) = self.compute_binding.take() {
                self.quarantined_bindings.push(binding.quarantine(), GFP_KERNEL)?;
            }
            if let (Some(manager), Some(queue)) = (self.manager.as_mut(), self.queue.as_mut()) {
                manager.resynchronise_compute_queue_after_abandon(queue)?;
            }
            self.compute_abandoned = false;
            if let Some(pdev) = self.pdev.as_ref() {
                dev_info!(
                    pdev.as_ref(),
                    "G17P compute: handoff from an ABANDONED submission; {} client binding(s) quarantined this session\n",
                    self.quarantined_bindings.len()
                );
            }
        } else {
            drop(self.compute_binding.take());
        }
        let snapshot = self.queue.as_mut().map(|queue| queue.handoff_snapshot());
        if let (Some(pdev), Some((ordinal, stamp, ring))) = (self.pdev.as_ref(), snapshot) {
            let (installs, zeros, rebuilds, _) = g17_manager::g17p_mark_counts();
            let retains = g17_manager::G17P_MARK_RETAINED_HANDOFFS
                .fetch_add(1, Ordering::Relaxed)
                + 1;
            dev_info!(
                pdev.as_ref(),
                "G17PMARK handoff-retained i={} z={} r={} h={} ordinal={} stamp={:#x} ring-producer={} -- graph kept, next submit takes the steady-state chronology with the tag-15 install flag clear\n",
                installs,
                zeros,
                rebuilds,
                retains,
                ordinal,
                stamp,
                ring
            );
        }
        Ok(())
    }

    /// Release the hardware compute graph held by `owner`, if it holds it.
    ///
    /// Called from the DRM queue object's `Drop`, i.e. on `queue_destroy` and
    /// on file close -- including the close the kernel performs for a client
    /// that died mid-submission.
    ///
    /// This ONLY marks the graph as released. It deliberately does not drop the
    /// binding: `install_t8140_compute_context_alias` left this VM's root in
    /// hardware contexts 2 and 3, and nothing rewrites those two entries until
    /// the *next* client binds. Freeing the page tables here would leave the
    /// firmware's compute context pointing at memory the allocator has taken
    /// back. The binding is therefore released inside `handoff_compute_queue`
    /// instead -- immediately before the next bind reinstalls those roots, or,
    /// on the full path, after both CPUs have stopped.
    pub(crate) fn release_compute_owner(&mut self, owner: u64) {
        if self.compute_owner != Some(owner) {
            return;
        }
        self.compute_owner = None;
        self.compute_recycle_pending = true;
        if let Some(pdev) = self.pdev.as_ref() {
            dev_info!(
                pdev.as_ref(),
                "G17P compute: client queue {} released the QID4 graph (outstanding={}); recycle deferred to the next submit\n",
                owner,
                self.compute_abandoned || self.pending_compute.is_some()
            );
        }
    }

    /// Queue destruction runs after the render scheduler entity has drained,
    /// so no submission carrying this descriptor context can remain pending.
    /// Arm the queue's firmware teardown: its scheduler-state release is
    /// published once its pairs and compute are quiescent, and its graphs,
    /// VM root and scheduler page are freed when the firmware consumes it.
    pub(crate) fn release_render_owner(
        &mut self,
        owner: u64,
        context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
    ) {
        let Some(manager) = self.manager.as_mut() else { return };
        let descriptor_context = context.context_id();
        if let Err(error) = manager.release_render_context_owner(context) {
            if let Some(pdev) = self.pdev.as_ref() {
                dev_err!(
                    pdev.as_ref(),
                    "G17P render: client queue {} could not arm teardown of context {} ({:?})\n",
                    owner, descriptor_context, error,
                );
            }
            return;
        }
        if let Some(pdev) = self.pdev.as_ref().filter(|_| crate::util::g17p_verbose()) {
            dev_info!(
                pdev.as_ref(),
                "G17P render: client queue {} released descriptor context {} after scheduler drain; teardown armed\n",
                owner,
                descriptor_context,
            );
        }
    }

    /// Whether any of this context's compute commands is still in flight on
    /// its installed queue. A quarantined runaway keeps its packet, and with
    /// it the teardown, until firmware retires the command or both
    /// processors stop -- the same lifetime a work queue has on macOS.
    fn compute_idle_for_context(
        &self,
        context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
    ) -> bool {
        self.async_compute_queues.iter().all(|q| {
            !q.context.as_ref().is_some_and(|owner| Arc::ptr_eq(owner, context))
                || q.inflight.is_none()
        })
    }

    /// Refresh the `g17p_queue_slots` snapshot of who holds queue slots.
    fn note_queue_slots(&self) {
        let compute = &self.async_compute_queues;
        crate::g17_queue_slots::note(crate::g17_queue_slots::Snapshot {
            compute_graphs: compute.len(),
            compute_owned: compute.iter().filter(|q| q.owner.is_some()).count(),
            compute_released: compute.iter().filter(|q| q.released && !q.quarantined).count(),
            compute_quarantined: compute.iter().filter(|q| q.quarantined).count(),
            compute_exit_killed: compute.iter().filter(|q| q.exit_kill.is_some()).count(),
            retired_graphs: self.retired_queues.len(),
            teardowns: self.manager.as_ref()
                .map_or(0, |manager| manager.logical_queue_teardowns_outstanding()),
            render_quarantined: self.quarantined_render_completions.len(),
        });
    }

    /// One teardown turn: publish the scheduler-state release of every closed
    /// queue whose render pairs and compute are quiescent, then hand back the
    /// teardowns whose release the firmware has consumed. The caller drops
    /// those after releasing the runtime mutex; nothing here sleeps.
    pub(crate) fn service_logical_queue_teardowns(
        &mut self,
    ) -> Result<KVec<g17_manager::G17PLogicalQueueTeardown>> {
        if self.is_crashed() {
            return Ok(KVec::new());
        }
        // v93r g17p_recovery_path bit0: return drained quarantined pairs
        // first, so a closed queue's teardown gate can open this turn.
        self.service_spared_recovery_renders()?;
        self.reclaim_drained_quarantined_render_slots();
        // g17p_exit_kill: settle acknowledged process-exit kills first, so a
        // context whose compute packets they release can publish its
        // scheduler-state release in this same turn.
        self.release_exit_killed_compute();
        self.note_queue_slots();
        let outstanding = self
            .manager
            .as_ref()
            .map_or(0, |manager| manager.logical_queue_teardowns_outstanding());
        if outstanding == 0 {
            return Ok(KVec::new());
        }
        let pdev = self.pdev.as_ref().cloned().ok_or(ENODEV)?;
        // Never publish while a firmware recovery is open: the control-ring
        // consumer is halted with it, exactly as the FList release path holds
        // back. Settling below is a read of the counters and stays valid.
        let recovery_pending = self
            .manager
            .as_ref()
            .ok_or(ENODEV)?
            .pending_primary_recovery()
            .is_some();
        // v92b: observe consumed render FList (0x2e) releases first. The
        // render gate below (`render_slot_idle_for_context`) requires each
        // pair's FList lifecycle to be `Completed`, and only a settle turns a
        // sent release into `Completed`. Firmware does not notify on control
        // consumption, and the only other settle points are the snapshot taken
        // microseconds after the 0x2e doorbell and the next submission's
        // pre-publication settle. A closing client's final release therefore
        // stayed unobserved -- teardown "still Draining", scheduler-state
        // release never published -- until another client submitted. This is
        // the same allocation-free, sleep-free read of the control counters
        // the submission path performs; it publishes and sends nothing.
        if !crate::g17_timing_buffer::diag_knob(crate::g17_timing_buffer::DIAG_TEARDOWN_SETTLE_OFF) {
            self.manager
                .as_mut()
                .ok_or(ENODEV)?
                .settle_consumed_render_usc_freelist_releases()?;
        }
        // v92c: a young teardown whose control records stay unconsumed gets
        // the device-control doorbell again (rate-limited), so a lost EP 0x21
        // announcement cannot leave the release pending until another client
        // submits. Recovery holds the consumer; never announce then.
        if !recovery_pending {
            self.reannounce_pending_primary_control("teardown-poll");
        }
        // Publish, in order, every release whose gate is closed.
        let mut from = 0;
        while !recovery_pending {
            let Some(index) = self
                .manager
                .as_mut()
                .ok_or(ENODEV)?
                .logical_queue_teardown_render_ready(from)?
            else {
                break;
            };
            from = index + 1;
            let Some(context) = self
                .manager
                .as_ref()
                .ok_or(ENODEV)?
                .logical_queue_teardown_context(index)
            else {
                continue;
            };
            if !self.compute_idle_for_context(&context) {
                // g17p_exit_kill: the owner left with compute kicks that will
                // not retire on their own (a quarantined or cancelled queue);
                // ask firmware to kill the context's work, as native does.
                let age = self.manager.as_ref().ok_or(ENODEV)?.logical_queue_teardown_age_ns(index);
                if let Err(error) = self.publish_exit_kill_for_context(&context, age) {
                    dev_err!(
                        pdev.as_ref(),
                        "G17P exit kill: publication for context {} failed ({:?}); retaining\n",
                        context.context_id(), error,
                    );
                }
                continue;
            }
            if self.primary_device_control_backpressured() {
                break;
            }
            let publication = match self
                .manager
                .as_mut()
                .ok_or(ENODEV)?
                .publish_logical_queue_release(index)
            {
                Ok(publication) => publication,
                Err(error) if error == EAGAIN => break,
                Err(error) => {
                    dev_err!(
                        pdev.as_ref(),
                        "G17P teardown: release publication for context {} failed ({:?}); retaining\n",
                        context.context_id(), error,
                    );
                    continue;
                }
            };
            fence(Ordering::SeqCst);
            // Same announcement as the FList release: native submit mode 1
            // rings EP 0x21 once per published record and waits for nothing.
            if let Err(error) = Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
                .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, g17_rtkit::encode_primary_device_control())
            {
                dev_warn!(
                    pdev.as_ref(),
                    "G17P teardown: release announcement for context {} failed ({:?}); the record at slot {} stays queued\n",
                    context.context_id(), error, publication.slot,
                );
            }
        }
        fence(Ordering::SeqCst);
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        let settled = manager.settle_logical_queue_teardowns()?;
        manager.warn_stalled_logical_queue_teardowns();
        Ok(settled)
    }

    /// v92c: re-send the primary device-control announcement (EP 0x21, mode
    /// 1) when records have stayed unconsumed for longer than
    /// `G17P_CONTROL_REANNOUNCE_NS`. Native announces once per record and
    /// the firmware drains every pending record per announcement, so an extra
    /// announcement with nothing new is a no-op drain. Rate-limited; nothing
    /// is published and nothing waits. Best effort: errors are ignored.
    fn reannounce_pending_primary_control(&mut self, reason: &'static str) {
        static PENDING_SINCE: AtomicU64 = AtomicU64::new(0);
        static LAST_SENT: AtomicU64 = AtomicU64::new(0);
        static LAST_LOG: AtomicU64 = AtomicU64::new(0);
        const G17P_CONTROL_REANNOUNCE_NS: u64 = 2_000_000;
        if crate::g17_timing_buffer::diag_knob(crate::g17_timing_buffer::DIAG_CONTROL_REANNOUNCE_OFF)
            || self.is_crashed()
        {
            return;
        }
        let Some(manager) = self.manager.as_mut() else { return };
        let Ok((consumer, producer)) = manager.primary_control_cursors() else { return };
        let now = crate::g17_timing_buffer::now();
        if consumer == producer {
            PENDING_SINCE.store(0, Ordering::Relaxed);
            return;
        }
        let since = PENDING_SINCE.load(Ordering::Relaxed);
        if since == 0 {
            PENDING_SINCE.store(now, Ordering::Relaxed);
            return;
        }
        if now.saturating_sub(since) < G17P_CONTROL_REANNOUNCE_NS
            || now.saturating_sub(LAST_SENT.load(Ordering::Relaxed)) < G17P_CONTROL_REANNOUNCE_NS
        {
            return;
        }
        LAST_SENT.store(now, Ordering::Relaxed);
        let Some(rtkit) = self.gfx_rtkit.as_mut() else { return };
        let sent = Pin::new(rtkit).send_message_logged(
            g17_rtkit::ENDPOINT_GFX_INTERRUPTS, g17_rtkit::encode_primary_device_control());
        // v93r g17p_recovery_path bit1: name the oldest pending record.
        let pending = crate::g17_recovery_path::describe_pending(consumer);
        if now.saturating_sub(LAST_LOG.load(Ordering::Relaxed)) >= 1_000_000_000 {
            LAST_LOG.store(now, Ordering::Relaxed);
            if let Some(pdev) = self.pdev.as_ref() {
                dev_info!(
                    pdev.as_ref(),
                    "G17P control re-announce: reason={} consumer={} producer={} pending_ms={} sent={:?}\n",
                    reason, consumer, producer, now.saturating_sub(since) / 1_000_000, sent.is_ok(),
                );
                if let Some((opcode, arg, age, announced)) = pending {
                    dev_info!(
                        pdev.as_ref(),
                        "G17P control re-announce: oldest pending slot={} opcode={:#x} arg={:#x} age_us={} announced-after-publish={}\n",
                        consumer, opcode, arg,
                        age.saturating_mul(1_000_000)
                            / crate::g17_timing_buffer::arch_frequency().max(1),
                        announced,
                    );
                }
            }
        }
    }

    /// v92c FList takeover guard for the submit path (runtime held). Settles
    /// every consumed 0x2e release, then answers whether `slot` and its warm
    /// `sibling` may be taken over by `context`: false while either still
    /// carries an unobserved release witness from another owner. A false
    /// answer re-announces the control ring; the caller drops Runtime, sleeps
    /// and retries. Steady state (same owner, or witnesses already consumed)
    /// costs one counter snapshot.
    pub(crate) fn render_flist_takeover_ready(
        &mut self,
        slot: u8,
        sibling: Option<u8>,
        context: &Arc<g17_resources::G17PLogicalQueueContext>,
    ) -> Result<bool> {
        if crate::g17_timing_buffer::diag_knob(crate::g17_timing_buffer::DIAG_FLIST_TAKEOVER_GUARD_OFF) {
            return Ok(true);
        }
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        if ![Some(slot), sibling].into_iter().flatten()
            .any(|pair| manager.render_flist_takeover_blocked(pair, context))
        {
            return Ok(true);
        }
        manager.settle_consumed_render_usc_freelist_releases()?;
        let blocked = [Some(slot), sibling].into_iter().flatten()
            .find(|pair| manager.render_flist_takeover_blocked(*pair, context));
        let Some(blocked) = blocked else { return Ok(true) };
        let cursors = manager.primary_control_cursors().ok();
        G17P_TAKEOVER_WAITS.fetch_add(1, Ordering::Relaxed);
        if G17P_TAKEOVER_WAITS.load(Ordering::Relaxed) % 1024 == 1 {
            if let Some(pdev) = self.pdev.as_ref() {
                dev_info!(
                    pdev.as_ref(),
                    "G17P FList takeover guard: pair {} for context {} waits for an unobserved 0x2e release (control cursors {:?}; waits={})\n",
                    blocked, context.context_id(), cursors, G17P_TAKEOVER_WAITS.load(Ordering::Relaxed),
                );
            }
        }
        self.reannounce_pending_primary_control("flist-takeover");
        Ok(false)
    }

    /// Whether the event worker should re-run at millisecond cadence for a
    /// young teardown that has not been acknowledged yet.
    pub(crate) fn logical_queue_teardown_polling(&self) -> bool {
        !self.is_crashed()
            && (self
                .manager
                .as_ref()
                .is_some_and(|manager| manager.logical_queue_teardown_polling())
                // v93r: keep the event worker polling for a bounded window
                // while a quarantined pair awaits its firmware drain.
                || self.reclaim_pending_since.is_some_and(|since| {
                    crate::g17_recovery_path::enabled(crate::g17_recovery_path::RECLAIM_DRAINED)
                        && since.elapsed().as_nanos() < 5_000_000_000
                })
                // g17p_exit_kill: a closed context's compute becoming due
                // for the kill, or a kill awaiting event 12 (bounded).
                || self.exit_kill_polling())
    }

    /// Wake the event worker for teardown service once a queue has closed.
    pub(crate) fn kick_logical_queue_teardown(&self) {
        self.queue_primary_control_continuation();
    }

    /// Make the hardware compute graph fit for a first bind by a new client.
    ///
    /// NOTE: both modes below re-run the FIRST-BIND chronology on the next
    /// submit, which republishes the tag-15 queue-install flag against a QID
    /// the firmware still has installed -- see `handoff_compute_queue` for why
    /// that is the second-client failure. They are kept as the fallback for a
    /// graph that genuinely cannot be handed on (crashed session, abandoned or
    /// in-flight submission, queue never installed) and as an A/B lever.
    ///
    /// Two modes, selected by `g17p_queue_recycle`:
    ///  * 1 (default): recycle the queue graph alone. The old graph is taken
    ///    off the SKSM bridge, retired (not freed), and replaced with a fresh
    ///    `PreparedMemory` graph. Neither ASC is restarted, so a client handoff
    ///    costs an allocation rather than a firmware session.
    ///  * 0: the previously shipped behaviour -- a full
    ///    `recover_persistent_runtime`, stopping both CPUs and rebuilding the
    ///    session. Slower by orders of magnitude, but it is the path a VM
    ///    change has always taken.
    /// A crashed session, a queue-only recycle that errors, and an exhausted
    /// retirement budget all fall through to mode 0.
    fn recycle_compute_queue(&mut self) -> Result {
        self.compute_owner = None;
        self.compute_recycle_pending = false;
        // `compute_gate_suspect` joins this set deliberately: the queue-only
        // recycle keeps both ASCs running, and a gated GPU is exactly what a
        // running session cannot be talked out of.
        let outstanding =
            self.compute_abandoned || self.pending_compute.is_some() || self.compute_gate_suspect;
        let queue_only = crate::g17_config::QUEUE_RECYCLE != 0
            && !outstanding
            && !self.is_crashed()
            && self.retired_queues.len() < Self::RETIRED_QUEUE_LIMIT;
        if queue_only {
            drop(self.pending_compute.take());
            drop(self.compute_timestamps.take());
            drop(self.compute_binding.take());
            match self.recycle_compute_queue_in_place() {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if let Some(pdev) = self.pdev.as_ref() {
                        dev_err!(
                            pdev.as_ref(),
                            "G17P compute: queue-only recycle failed ({:?}); rebuilding the session\n",
                            error
                        );
                    }
                }
            }
        }
        // The full path stops both CPUs before `release_runtime_objects`
        // frees anything, which is what makes releasing an outstanding
        // client's binding safe.
        if let Err(error) = self.rebuild_session_or_defer() {
            if error == EAGAIN { return Err(error); }
            // The rebuild has already dropped both CPUs and both RTKit owners
            // and never reached `resume_submissions`, so the runtime is now
            // gutted: nothing here can put it back. Say so loudly and mark the
            // session crashed, which is the one state
            // `recover_crashed_session_once` will retry from when a later
            // client opens the device -- otherwise the failure is silent and
            // every subsequent ioctl returns a bare ENODEV.
            if let Some(pdev) = self.pdev.as_ref() {
                dev_err!(
                    pdev.as_ref(),
                    "G17PMARK rebuild-failed err={:?}; the firmware session is gone (no CPUs, no RTKit) and this device is dead until a rebuild succeeds\n",
                    error
                );
            }
            if let Some(state) = self.gfx_state.as_ref() {
                state.crashed.store(true, Ordering::Release);
            }
            return Err(error);
        }
        self.compute_abandoned = false;
        self.ensure_compute_queue()
    }

    fn recycle_compute_queue_in_place(&mut self) -> Result {
        let mut retired_queue_id = None;
        if let Some(mut queue) = self.queue.take() {
            retired_queue_id = Some(queue.queue_id() as usize);
            let unregister = match (self.manager.as_ref(), self.registers.as_ref()) {
                (Some(manager), Some(registers)) => {
                    manager.unregister_compute_sksm_queue(registers, &mut queue)
                }
                _ => Err(ENODEV),
            };
            // Retire the graph either way. It must not be freed while the
            // firmware may still name it, and a failed disable is exactly the
            // case where that is most likely.
            self.retired_queues.push(queue, GFP_KERNEL)?;
            unregister?;
        }
        // Restore the firmware's per-QID progress record before the
        // replacement graph is built.
        //
        // `prepare_g17p_compute_queue` hands back a graph whose producer is
        // re-seeded to `geometry.timestamp_seed()`, i.e. the first kick a new
        // client publishes carries the same low stamp the previous owner's
        // first kick did. The firmware still has the RETIRING owner's
        // last-submitted stamp recorded for this QID in the 128-entry table
        // published at primary main-config `+0x274` (bundle view 4). A stamp
        // at or below that is the "already retired" condition -- the kick is
        // accepted and never run, so the
        // submit reaches the doorbell wait and times out with nothing faulted.
        //
        // Nothing else on this path clears it:
        //  * `reset_empty_sksm_last_submitted_hw_timestamps` is reached only
        //    from the recovery service, and only for `generation == 0`;
        //  * the B2 republication `publish_compute_b2_once` performs (the fresh
        //    graph comes back `Pending`) writes region_1 at 0x10..0x20, which
        //    is QID 1's slot -- QID 4's sits at 0x40 and is untouched;
        //  * the SKSM disable/configure transactions either side of this are
        //    suppressed no-ops whenever `g17p_sksm_mmio == 0`, which is the
        //    default and the only setting that does not SError on J700, so the
        //    bridge never re-arms anything either.
        //
        // The full `recover_persistent_runtime` path does not need this: it
        // stops both CPUs and takes the generation-0 recovery, which publishes
        // the same 128 zero entries. That asymmetry is exactly why a second DRM
        // client works with `g17p_queue_recycle=0` and not with the default.
        if let Some(qid) = retired_queue_id {
            let pdev = self.pdev.as_ref().cloned();
            if let Some(manager) = self.manager.as_mut() {
                let before = manager.g17p_last_submitted_hw_timestamp(qid);
                manager.reset_empty_sksm_last_submitted_hw_timestamps()?;
                let after = manager.g17p_last_submitted_hw_timestamp(qid);
                if let Some(pdev) = pdev.as_ref() {
                    match (before, after) {
                        (Ok((valid_before, stamp_before)), Ok((valid_after, stamp_after))) => {
                            dev_info!(
                                pdev.as_ref(),
                                "G17P compute: handoff reset QID {} last-submitted HW timestamp valid {} -> {} stamp {:#x} -> {:#x}\n",
                                qid,
                                valid_before,
                                valid_after,
                                stamp_before,
                                stamp_after,
                            );
                        }
                        _ => dev_info!(
                            pdev.as_ref(),
                            "G17P compute: handoff reset the last-submitted HW timestamp table; QID {} read-back unavailable\n",
                            qid,
                        ),
                    }
                }
            }
        }
        // Kept as-is deliberately, so this change stays single-variable.
        // `completion_indices` is session state, not queue state, and the
        // session did not restart here -- but it is only ever written and never
        // read, so the reset is inert. Flagged rather than removed: if a reader
        // is ever added, an in-place handoff must NOT reset it, because the
        // firmware's completion ring keeps counting across one.
        // `release_runtime_objects` resets it correctly, after both CPUs stop.
        self.completion_indices = g17_completion::QueueIndices {
            done: 0,
            read: 0,
            write: 0,
        };
        self.queue_setup_pending = true;
        self.ensure_compute_queue()?;
        if let Some(pdev) = self.pdev.as_ref() {
            dev_info!(
                pdev.as_ref(),
                "G17P compute: QID4 graph recycled in place; {} retired graph(s) held by this session\n",
                self.retired_queues.len()
            );
        }
        Ok(())
    }

    /// Build, publish, notify, and synchronously retire one translated user
    /// render through the Linux-owned G17P queue graph.
    /// Wait for the render to complete, servicing firmware events between
    /// polls when `g17p_render_service_events` is set.
    ///
    /// WHY THIS EXISTS. The render submit runs under the runtime mutex --
    /// `g17_drm.rs` calls `submit_translated_render` on a locked guard and the
    /// wait happens inside that call -- so the workqueue item that would drain
    /// the firmware event ring blocks on `mutex().lock()` for the whole wait.
    /// Measured consequence: the firmware halts, posts a type-4
    /// `kAGFIFirmwareEventTypeGPURestart` naming stamp slot 0, raises the RTKit
    /// notification (four seen, role=GFX1) -- and the ring still reads
    /// `producer=1 consumer=0 pending=1` when the wait gives up. The firmware
    /// spins until the host writes 2 into `status_b[0x4900]`; never answering
    /// leaves KSM halted forever.
    ///
    /// The fix mirrors `submit_translated_compute`, which polls with the mutex
    /// held and therefore calls `service_firmware_events_inline` from inside
    /// its own loop rather than relying on the worker. That is deliberately NOT
    /// the same as releasing the lock: no locking changes here, so compute and
    /// the DRM-level wait are untouched, and `service_firmware_events_inline`
    /// already does the `queue_primary_recovery(generation, ..)` bookkeeping
    /// that a bare drain would skip.
    ///
    /// SCOPE: this makes a render halt RECOVERABLE. It does not make a render
    /// succeed. The completion records were read directly from the KSM rings
    /// and were genuinely empty, so nothing about this makes the work run.
    fn wait_render_completion_servicing_events(
        &mut self,
        render_slot: u8,
    ) -> Result<g17_completion::PairedRenderCompletion> {
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .ensure_render_slot(render_slot)?;
        if crate::g17_config::RENDER_SERVICE_EVENTS == 0 {
            let completion = self
                .manager
                .as_mut()
                .ok_or(ENODEV)?
                .wait_translated_render_completion(render_slot)?;
            if let (Some(pdev), Some(manager)) = (self.pdev.as_ref(), self.manager.as_mut()) {
                finish_g17p_render_trace(pdev.as_ref(), manager, render_slot,
                    crate::g17_trace_capture::Phase::AfterPairRetirement);
            }
            let release = self
                .manager
                .as_mut()
                .ok_or(ENODEV)?
                .retire_render_usc_freelist_at_completion(render_slot)?;
            if let Some(release) = release {
                match self.service_render_usc_freelist_release(release) {
                    Ok(()) => {}
                    Err(error) if error == EAGAIN => {
                        // The lifecycle remains Ready with zero references.
                        // Event service gives the mandatory TVB reply the next
                        // credit, then publishes this release exactly once.
                    }
                    Err(error) => return Err(error),
                }
            }
            return Ok(completion);
        }
        let mut serviced = 0u32;
        let mut service_failures = 0u32;
        // Same schedule as the DRM-level wait: start at 20us so a short pass
        // is observed promptly, settle at the 1ms interval this loop always
        // used. The stuck-firmware bound is a wall-clock deadline instead of a
        // poll count, so it no longer moves with the poll interval.
        let mut schedule =
            crate::g17_perf::PollSchedule::new(module_parameters::g17p_render_poll_us.value());
        let deadline_ns =
            i64::from(module_parameters::g17p_render_wait_ms.value()).saturating_mul(1_000_000);
        let started = kernel::time::Instant::<kernel::time::Monotonic>::now();
        loop {
            // Event service visits every pending render slot. Assert the exact
            // submission identity at each poll boundary instead of depending
            // on ambient manager selection.
            self.manager
                .as_mut()
                .ok_or(ENODEV)?
                .ensure_render_slot(render_slot)?;
            match self
                .manager
                .as_mut()
                .ok_or(ENODEV)?
                .poll_translated_render_completion(render_slot)?
            {
                g17_manager::G17PRenderPollOutcome::Complete(completion) => {
                    // Drain pending firmware notifications BEFORE retiring the
                    // USC FList references. The lifecycle holds three: the
                    // async-grow request, the TA and the 3D. The firmware's
                    // type-13 async-grow completion retires the first (3 -> 2)
                    // and only then may this paired completion retire the other
                    // two. Returning straight out of the poll skipped that, so
                    // the retire saw references=3 and refused with WrongPhase,
                    // which the caller reported as a bare EIO -- AFTER the GPU
                    // had finished the whole render. Ordering only: nothing is
                    // retired here that was not already complete.
                    match self.service_firmware_events_inline() {
                        Ok(count) => serviced += count,
                        Err(error) => {
                            if let Some(pdev) = self.pdev.as_ref() {
                                dev_warn!(
                                    pdev.as_ref(),
                                    "G17P render: event service at completion failed ({:?})\n",
                                    error
                                );
                            }
                        }
                    }
                    self.manager
                        .as_mut()
                        .ok_or(ENODEV)?
                        .ensure_render_slot(render_slot)?;
                    if let (Some(pdev), Some(manager)) = (self.pdev.as_ref(), self.manager.as_mut()) {
                        finish_g17p_render_trace(pdev.as_ref(), manager, render_slot,
                            crate::g17_trace_capture::Phase::AfterPairRetirement);
                    }
                    let release = self
                        .manager
                        .as_mut()
                        .ok_or(ENODEV)?
                        .retire_render_usc_freelist_at_completion(render_slot)?;
                    if let Some(release) = release {
                        match self.service_render_usc_freelist_release(release) {
                            Ok(()) => {}
                            Err(error) if error == EAGAIN => {}
                            Err(error) => return Err(error),
                        }
                    }
                    if serviced != 0 {
                        if let Some(pdev) = self.pdev.as_ref() {
                            dev_info!(
                                pdev.as_ref(),
                                "G17P render: completed after servicing {} firmware event notifications\n",
                                serviced
                            );
                        }
                    }
                    return Ok(completion);
                }
                // "A completed job list was quiesced; re-poll immediately,
                // do not sleep." The deadline still applies: with a poll count
                // gone, an outcome that never advances would otherwise spin
                // this loop forever inside the kernel.
                g17_manager::G17PRenderPollOutcome::Retry => {
                    if started.elapsed().as_nanos() >= deadline_ns {
                        break;
                    }
                    continue;
                }
                g17_manager::G17PRenderPollOutcome::Pending => {}
            }
            match self.service_firmware_events_inline() {
                Ok(count) => serviced += count,
                Err(error) => {
                    if service_failures == 0 {
                        if let Some(pdev) = self.pdev.as_ref() {
                            dev_warn!(
                                pdev.as_ref(),
                                "G17P render: inline event service failed ({:?})\n",
                                error
                            );
                        }
                    }
                    service_failures += 1;
                }
            }
            self.manager
                .as_mut()
                .ok_or(ENODEV)?
                .ensure_render_slot(render_slot)?;
            if started.elapsed().as_nanos() >= deadline_ns {
                break;
            }
            fsleep(Delta::from_micros(i64::from(schedule.next_delay_us())));
        }
        if let Some(pdev) = self.pdev.as_ref() {
            dev_err!(
                pdev.as_ref(),
                "G17P render: wait timed out; serviced {} notifications, {} service failures\n",
                serviced,
                service_failures
            );
        }
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .ensure_render_slot(render_slot)?;
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .report_render_wait_timeout(render_slot, "event-serviced-wait-expired");
        Err(ETIMEDOUT)
    }

    /// Poll one independently owned render slot while holding the runtime
    /// mutex only for this bounded observation/service transaction.  The DRM
    /// caller sleeps with the mutex released, allowing the other slot to
    /// publish and allowing asynchronous compute retirement to proceed.
    pub(crate) fn poll_deferred_render_completion(
        &mut self,
        render_slot: u8,
    ) -> Result<Option<g17_completion::PairedRenderCompletion>> {
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .ensure_render_slot(render_slot)?;
        let outcome = self
            .manager
            .as_mut()
            .ok_or(ENODEV)?
            .poll_translated_render_completion(render_slot)?;
        self.finish_render_poll(render_slot, outcome)
    }

    fn finish_render_poll(&mut self, render_slot: u8,
        outcome: g17_manager::G17PRenderPollOutcome,
    ) -> Result<Option<g17_completion::PairedRenderCompletion>> {
        match outcome {
            g17_manager::G17PRenderPollOutcome::Complete(completion) => {
                if let (Some(pdev), Some(manager)) = (self.pdev.as_ref(), self.manager.as_mut()) {
                    finish_g17p_render_trace(
                        pdev.as_ref(),
                        manager,
                        render_slot,
                        crate::g17_trace_capture::Phase::AfterPairRetirement,
                    );
                }
                if let (Some(pdev), Some(manager)) = (self.pdev.as_ref(), self.manager.as_mut()) {
                    log_g17p_status_a(pdev.as_ref(), manager, "render-post-wait");
                    probe_g17p_ktrace(pdev.as_ref(), manager, "render-post-wait");
                    dump_g17p_firmware_log(pdev.as_ref(), manager, "render-post-wait");
                    manager.log_render_status_pages(render_slot, "post-completion");
                }
                Ok(Some(completion))
            }
            g17_manager::G17PRenderPollOutcome::Retry => Ok(None),
            g17_manager::G17PRenderPollOutcome::Pending => Ok(None),
        }
    }

    /// Commit the exact render completion before fallible device-global
    /// cleanup.
    ///
    /// `poll_translated_render_completion` has already committed the paired
    /// queue retirement: it clears the manager tracker, returns the HWPB ID,
    /// advances the ordinal, and proves EventControl unlinked.  A subsequent
    /// FList release error therefore cannot be retried as a render completion.
    /// Keeping the fence armed until after that cleanup made a GPU-successful
    /// job wait for the two-second scheduler watchdog and then fail as EIO.
    /// FList storage is driver-owned and remains retained by the manager, so
    /// commit the truthful completion first (its fence is signalled once the
    /// runtime mutex is released, or by the rebuild itself under processor
    /// stop); if cleanup fails, stop both processors and rebuild before
    /// another submission can reuse that state.
    fn settle_deferred_render_completion(
        &mut self,
        render_slot: u8,
        observed: g17_completion::PairedRenderCompletion,
    ) -> Result<bool> {
        self.settle_retired_render_completion(render_slot, Ok(observed))
    }

    /// The manager has already proved retirement and committed its tracker.
    /// Cancelled work reports an error fence; it never fabricates GPU output.
    fn settle_retired_render_completion(&mut self, render_slot: u8,
        observed: Result<g17_completion::PairedRenderCompletion>) -> Result<bool> {
        let index = usize::from(render_slot);
        if self.pending_render.get(index).and_then(Option::as_ref).is_none() {
            return Err(EIO);
        }
        let signal = self.pending_render[index].take().ok_or(EIO)?;
        if crate::g17_e2e::armed() {
            if let Ok(observed) = observed.as_ref() {
                let ingress = self.notification_ingress_counters();
                crate::g17_e2e::record(crate::g17_e2e::K_OBSERVED, u32::from(render_slot),
                    signal.submission_id(), [ingress[0], ingress[1], self.trace_service_entry_counter,
                    observed.execution.fragment_timestamps[0], observed.execution.fragment_timestamps[1]]);
                crate::g17_e2e::power_sample(2, render_slot);
            }
        }
        if crate::g17_timing_buffer::enabled() {
            signal.note_service_trace(
                self.notification_ingress_counters(),
                self.trace_service_entry_counter,
                crate::g17_timing_buffer::arch_counter(),
            );
        }
        // Proven retired. The fence is signalled with the runtime mutex
        // released (`defer_completion`), still ahead of the fallible FList
        // cleanup below.
        self.defer_completion(crate::g17_drm::G17PDeferredCompletion::Render {
            completion: signal, observed,
        });
        if let Some(next) = self.pending_render_queued[index].take() {
            self.pending_render[index] = Some(next);
        } else {
            if !self.render_fence_state.settle(render_slot) {
                return Ok(false);
            }
            self.render_event_masks[index] = [0; 2];
        }

        let cleanup = (|| -> Result {
            let release = self
                .manager
                .as_mut()
                .ok_or(ENODEV)?
                .retire_render_usc_freelist_at_completion(render_slot)?;
            if let Some(release) = release {
                match self.service_render_usc_freelist_release(release) {
                    Ok(()) => {}
                    Err(error) if error == EAGAIN => {
                        // The lifecycle remains Ready with zero references.
                        // Event service publishes it after the pending TVB
                        // reply without re-signalling this render fence.
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        })();
        if let Err(error) = cleanup {
            if let Some(pdev) = self.pdev.as_ref() {
                dev_err!(
                    pdev.as_ref(),
                    "G17P render: paired completion signalled, but retained FList cleanup failed ({:?}); rebuilding before reuse\n",
                    error,
                );
            }
            // Completions are settled inside event and recovery service,
            // which can itself run inside a publication: never rebuild the
            // session that deep (kernel stack). Close admission and hand the
            // rebuild to the recovery worker -- exactly the outcome
            // `rebuild_session_or_defer` already gives when preparations are
            // in flight -- so no submission can reuse this state first.
            return Err(self.request_session_rebuild(EAGAIN));
        }
        Ok(true)
    }

    pub(crate) fn note_render_observation_error(&mut self, error: Error) {
        if let (Some(pdev), Some(state)) = (self.pdev.as_ref(), self.gfx_state.as_ref()) {
            note_state_crashed(pdev, state, "async-render-completions", error, None);
        }
    }

    fn retain_render_completion_masks(&mut self, masks: [u64; 2]) {
        for slot in 0..g17_resources::G17PRenderQueuePair::SLOT_COUNT {
            if self.render_fence_state.pending(slot) {
                self.render_event_masks[usize::from(slot)][0] |= masks[0];
                self.render_event_masks[usize::from(slot)][1] |= masks[1];
            }
        }
    }

    pub(crate) fn next_render_grow(&mut self)
        -> Result<Option<(crate::driver::AsahiDevRef, g17_manager::G17PRenderGrow)>> {
        if self.is_crashed() || self.primary_recovery_pending() { return Ok(None); }
        let drm = self.drm.as_ref().ok_or(ENODEV)?.clone();
        Ok(self.manager.as_mut().ok_or(ENODEV)?.take_render_grow()?.map(|grow| (drm, grow)))
    }

    pub(crate) fn return_render_grow(&mut self, grow: g17_manager::G17PRenderGrow,
        result: Result<bool>, commit: bool) -> Result {
        let commit = commit && !self.is_crashed() && !self.primary_recovery_pending();
        let disposition = self.manager.as_mut().expect("grow epoch pins manager")
            .return_render_grow(grow, result, commit);
        self.resolve_deferred_render_timeouts();
        let disposition = disposition?;
        if disposition == g17_manager::G17PTvbGrowDisposition::Published {
            Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?).send_message_logged(
                g17_rtkit::ENDPOINT_GFX_INTERRUPTS, g17_rtkit::encode_primary_device_control())?;
        }
        if let Some(drm) = self.drm.as_ref() {
            crate::driver::queue_g17p_firmware_event_worker(drm.clone());
        }
        Ok(())
    }

    /// v87 OBS_SKIP: the pre-filter of `next_render_observation` over every
    /// slot, without taking or diagnosing anything. False only when that
    /// function would return `None` for every slot at this instant; any
    /// lookup error answers true so the loop still reports it.
    pub(crate) fn render_observation_possible(&self) -> bool {
        if self.is_crashed() || self.primary_recovery_pending() { return false; }
        let Some(manager) = self.manager.as_ref() else { return true; };
        for slot in 0..g17_resources::G17PRenderQueuePair::SLOT_COUNT {
            if !self.render_fence_state.pending(slot) { continue; }
            match manager.render_completion_masks_for_slot(slot) {
                Ok(expected) => if g17_completion::g17p_render_type1_complete(
                    self.render_event_masks[usize::from(slot)], expected) { return true; },
                Err(_) => return true,
            }
        }
        false
    }

    pub(crate) fn next_render_observation(&mut self, start: u8)
        -> Result<Option<g17_manager::G17PRenderObservation>>
    {
        if self.is_crashed() || self.primary_recovery_pending() { return Ok(None); }
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        for slot in start..g17_resources::G17PRenderQueuePair::SLOT_COUNT {
            if !self.render_fence_state.pending(slot) { continue; }
            let expected = manager.render_completion_masks_for_slot(slot)?;
            if !g17_completion::g17p_render_type1_complete(
                self.render_event_masks[usize::from(slot)], expected) {
                let observed = self.render_event_masks[usize::from(slot)];
                crate::g17_timing_buffer::record_render_gate(slot, u64::MAX, 6,
                    0, [0; 2], [observed[0], observed[1], expected[0], expected[1]]);
                continue;
            }
            if let Some(observation) = manager.take_render_observation(slot)? {
                return Ok(Some(observation));
            }
        }
        Ok(None)
    }

    pub(crate) fn return_render_observation(&mut self, observation: g17_manager::G17PRenderObservation) {
        let manager = self.manager.as_mut().expect("observation epoch pins manager");
        manager.return_render_observation(observation);
        if manager.preparation_grow_pending() {
            if let Some(state) = self.gfx_state.as_ref() {
                crate::driver::queue_g17p_firmware_event_worker(state.dev.clone());
            }
        }
    }

    pub(crate) fn commit_render_observation(&mut self, slot: u8,
        observed: Result<(g17_resources::G17PUserRenderQueueState,
            g17_completion::PairedRenderCompletion)>,
    ) -> Result<bool> {
        // A watchdog/VM failure can quarantine the pending fence while the
        // reader owns the graph. Return retained memory but do not retire that
        // manager tracker through a now-absent runtime completion owner.
        if self.is_crashed() || self.primary_recovery_pending()
            || !self.render_fence_state.pending(slot)
            || self.pending_render[usize::from(slot)].is_none() {
            return Ok(false);
        }
        let observed = match observed {
            Ok(observed) => observed,
            Err(error) if error == EAGAIN => return Ok(false),
            Err(error) => return Err(error),
        };
        let outcome = self.manager.as_mut().ok_or(ENODEV)?
            .commit_render_observation(slot, Some(observed))?;
        match self.finish_render_poll(slot, outcome)? {
            Some(completion) => self.settle_deferred_render_completion(slot, completion),
            None => Ok(false),
        }
    }

    /// Retire any render whose firmware record became visible on this RTKit
    /// event edge. Both slots are inspected so a later submission completing
    /// first cannot strand the earlier slot, and taking the pending Arc makes
    /// duplicate notifications idempotent.
    fn service_async_render_completions(&mut self, new_masks: [u64; 2]) -> Result {
        let serviced = (|| -> Result {
            for slot in 0..g17_resources::G17PRenderQueuePair::SLOT_COUNT {
                if !self.render_fence_state.pending(slot) {
                    continue;
                }
                let observed = &mut self.render_event_masks[slot as usize];
                observed[0] |= new_masks[0];
                observed[1] |= new_masks[1];
                let expected = self.manager.as_ref().ok_or(ENODEV)?
                    .render_completion_masks_for_slot(slot)?;
                if !g17_completion::g17p_render_type1_complete(*observed, expected) {
                    continue;
                }
                // One event edge may cover both ordered groups. Settle the front,
                // promote its queued successor, and immediately inspect that
                // successor before discarding the already-observed QID mask.
                for _ in 0..g17_resources::G17P_RENDER_MAX_IN_FLIGHT {
                    let mut completion = None;
                    for _ in 0..2 {
                        match self.poll_deferred_render_completion(slot)? {
                            Some(observed) => {
                                completion = Some(observed);
                                break;
                            }
                            None => {}
                        }
                    }
                    let Some(observed) = completion else { break };
                    if !self.settle_deferred_render_completion(slot, observed)? {
                        break;
                    }
                    // Final settlement disarms the physical slot. Only a promoted
                    // successor leaves it armed; polling again without one asks
                    // the manager for a tracker that no longer exists and turns a
                    // successful one-deep render into a spurious ENODEV crash.
                    if !self.render_fence_state.pending(slot) {
                        break;
                    }
                }
            }
            Ok(())
        })();
        serviced
    }

    fn install_render_completion(
        &mut self,
        render_slot: u8,
        completion: Arc<crate::g17_drm::G17PRenderCompletion>,
    ) -> Result {
        let pending = self.pending_render
            .get_mut(render_slot as usize)
            .ok_or(EINVAL)?;
        if pending.is_none() {
            if !self.render_fence_state.arm(render_slot) {
                return Err(EBUSY);
            }
            self.render_event_masks[render_slot as usize] = [0; 2];
            *pending = Some(completion);
            return Ok(());
        }
        let queued = self.pending_render_queued
            .get_mut(render_slot as usize)
            .ok_or(EINVAL)?;
        queued.push(completion).map_err(|_| EBUSY)
    }

    /// v93r `g17p_recovery_path` bit0. Return every quarantined render pair
    /// the firmware has provably drained since the recovery that failed it.
    ///
    /// `fail_render_after_final_observation` keeps the failed render and its
    /// queued successors (their fences already failed) together with their
    /// VM-job and lease guards, because at that instant firmware may still
    /// own the group. The only release so far was processor stop, so one
    /// stamp-blamed recovery cost a physical pair and left the client's
    /// closed-queue teardown `Draining` for the rest of the boot. Here, under
    /// the runtime mutex with the recovery closed, a pair is reclaimed only
    /// when `render_slot_firmware_drained` proves every publication consumed
    /// (stamps cover the last published ordinal, FList completed ==
    /// submitted, handshake state 0). Its trackers are then retired in
    /// order: an executed group through the normal paired-completion
    /// transaction, the group the recovery skipped through the existing
    /// firmware-retired classifier (queue cursors at target, job list
    /// unlinked), each followed by the FList reference retirement a
    /// completion performs. Only then are the retained guards released. Any
    /// unmet condition leaves everything as it was for a later turn.
    fn reclaim_drained_quarantined_render_slots(&mut self) {
        use crate::g17_recovery_path::{self as rp, ReclaimBlock};
        if !rp::enabled(rp::RECLAIM_DRAINED)
            || self.quarantined_render_completions.is_empty()
            || self.is_crashed()
        {
            self.reclaim_pending_since = None;
            return;
        }
        if self.reclaim_pending_since.is_none() {
            self.reclaim_pending_since =
                Some(kernel::time::Instant::<kernel::time::Monotonic>::now());
        }
        rp::RECLAIM_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        if self.primary_recovery_pending() {
            rp::note_blocked(ReclaimBlock::Recovery);
            return;
        }
        // Physical pairs named by the retained completions, in slot order.
        let mut slots = 0u64;
        for completion in self.quarantined_render_completions.iter() {
            if let Some(slot) = completion.render_slot() {
                slots |= 1u64 << (slot & 63);
            }
        }
        let mut serviced = false;
        while slots != 0 {
            let slot = slots.trailing_zeros() as u8;
            slots &= slots - 1;
            match self.reclaim_drained_render_slot(slot, &mut serviced) {
                Ok(None) => {
                    let mut released = 0u64;
                    let mut index = 0;
                    while index < self.quarantined_render_completions.len() {
                        if self.quarantined_render_completions[index].render_slot() == Some(slot) {
                            let completion = self.quarantined_render_completions.swap_remove(index);
                            // VM-job and lease guards drop after the runtime
                            // mutex is released, like a completion's.
                            self.defer_completion(
                                crate::g17_drm::G17PDeferredCompletion::ReleaseAfterDrain { completion });
                            released += 1;
                        } else {
                            index += 1;
                        }
                    }
                    rp::RECLAIM_PAIRS.fetch_add(1, Ordering::Relaxed);
                    rp::RECLAIM_RELEASED.fetch_add(released, Ordering::Relaxed);
                    if let Some(pdev) = self.pdev.as_ref() {
                        dev_warn!(
                            pdev.as_ref(),
                            "G17P recovery reclaim: render slot {} drained by firmware; released {} quarantined completion(s) and returned the pair\n",
                            slot, released,
                        );
                    }
                }
                Ok(Some(reason)) => rp::note_blocked(reason),
                Err(error) => {
                    rp::note_blocked(ReclaimBlock::Error);
                    if let Some(pdev) = self.pdev.as_ref() {
                        dev_warn!(
                            pdev.as_ref(),
                            "G17P recovery reclaim: render slot {} stays quarantined ({:?})\n",
                            slot, error,
                        );
                    }
                }
            }
        }
        if self.quarantined_render_completions.is_empty() {
            self.reclaim_pending_since = None;
        }
    }

    /// One pair of `reclaim_drained_quarantined_render_slots`: `Ok(None)`
    /// once the pair tracks nothing, `Ok(Some(reason))` to retry later.
    fn reclaim_drained_render_slot(
        &mut self, slot: u8, serviced: &mut bool,
    ) -> Result<Option<crate::g17_recovery_path::ReclaimBlock>> {
        use crate::g17_recovery_path::{self as rp, ReclaimBlock};
        self.manager.as_mut().ok_or(ENODEV)?.ensure_render_slot(slot)?;
        if let Some(reason) = self.manager.as_mut().ok_or(ENODEV)?
            .render_slot_firmware_drained(slot)?
        {
            return Ok(Some(reason));
        }
        // Same ordering as a completion: pending notifications (the async
        // grow completion owns one FList reference) before FList retirement.
        if !*serviced {
            self.service_firmware_events_inline()?;
            *serviced = true;
            self.manager.as_mut().ok_or(ENODEV)?.ensure_render_slot(slot)?;
        }
        // Event service is fallible and can open/service another recovery.
        // A proof acquired before it cannot authorize releasing quarantined
        // references afterwards. Keep the pair quarantined on every failure
        // or changed recovery/drain observation, before retiring any tracker.
        if self.is_crashed() { return Err(EIO); }
        if self.primary_recovery_pending() {
            return Ok(Some(ReclaimBlock::Recovery));
        }
        if let Some(reason) = self.manager.as_mut().ok_or(ENODEV)?
            .render_slot_firmware_drained(slot)?
        {
            return Ok(Some(reason));
        }
        while self.manager.as_ref().ok_or(ENODEV)?.render_slot_tracked(slot)? {
            let retired = match self.manager.as_mut().ok_or(ENODEV)?
                .poll_translated_render_completion(slot)?
            {
                g17_manager::G17PRenderPollOutcome::Complete(_) => true,
                g17_manager::G17PRenderPollOutcome::Pending
                | g17_manager::G17PRenderPollOutcome::Retry => self.manager.as_mut()
                    .ok_or(ENODEV)?
                    .retire_abandoned_render_if_firmware_retired(slot, true)?,
            };
            if !retired {
                return Ok(Some(ReclaimBlock::Tracker));
            }
            rp::RECLAIM_TRACKERS.fetch_add(1, Ordering::Relaxed);
            let release = self.manager.as_mut().ok_or(ENODEV)?
                .retire_render_usc_freelist_at_completion(slot)?;
            if let Some(release) = release {
                match self.service_render_usc_freelist_release(release) {
                    // EAGAIN: the lifecycle stays Ready with zero references
                    // and event service publishes it later, as for a completion.
                    Ok(()) => {}
                    Err(error) if error == EAGAIN => {}
                    Err(error) => return Err(error),
                }
            }
        }
        // FList release publishes asynchronous firmware control records.
        // Recheck the proof before returning quarantined ownership.
        if self.is_crashed() { return Err(EIO); }
        if self.primary_recovery_pending() {
            return Ok(Some(ReclaimBlock::Recovery));
        }
        self.manager.as_mut().ok_or(ENODEV)?.render_slot_firmware_drained(slot)
    }

    /// Release error-fence owners only after both processors are stopped.
    /// Scheduler packets may retain their completion Arcs after the fence was
    /// signalled, so clearing this vector alone is not an ownership boundary.
    fn release_quarantined_render_after_processor_stop(&mut self) {
        for completion in &self.quarantined_render_completions {
            completion.release_after_processor_stop();
        }
        self.quarantined_render_completions.clear();
    }

    /// Bounded watchdog/cancel settlement. This is never the normal
    /// completion path: ordinary retirement is driven by the RTKit event
    /// worker above. An unresolved completion quarantines its own physical
    /// slot before signalling the error fence, so no later caller can race a
    /// session rebuild by publishing that bit late.
    pub(crate) fn timeout_deferred_render(
        &mut self,
        render_slot: u8,
        submission_id: u64,
        completion: &Arc<crate::g17_drm::G17PRenderCompletion>,
        error: Error,
    ) {
        if !self.render_completion_pending(render_slot, completion) { return; }
        if self.defer_render_timeout_if_borrowed(render_slot, completion, error) { return; }
        // The guilty scheduler packet need not be the pair's current front.
        // Retire the bounded completed prefix through this exact Arc before
        // classifying it; completing only a predecessor does not answer it.
        for _ in 0..g17_resources::G17P_RENDER_MAX_IN_FLIGHT {
            if !self.render_completion_pending(render_slot, completion) { return; }
            match self.poll_deferred_render_completion(render_slot) {
                Ok(Some(observed)) => match self.settle_deferred_render_completion(render_slot, observed) {
                    Ok(true) => {},
                    _ => break,
                },
                _ => break,
            }
        }
        if self.render_completion_pending(render_slot, completion) {
            self.fail_render_after_final_observation(render_slot, Some(submission_id), None, error);
        }
    }

    fn render_completion_pending(&self, render_slot: u8,
        completion: &Arc<crate::g17_drm::G17PRenderCompletion>) -> bool {
        self.pending_render.get(usize::from(render_slot)).and_then(Option::as_ref)
            .is_some_and(|current| Arc::ptr_eq(current, completion))
            || self.pending_render_queued.get(usize::from(render_slot))
                .is_some_and(|queued| queued.iter().any(|current| Arc::ptr_eq(current, completion)))
    }

    pub(crate) fn defer_render_timeout_if_borrowed(&mut self, render_slot: u8,
        completion: &Arc<crate::g17_drm::G17PRenderCompletion>, error: Error) -> bool {
        if !self.render_completion_pending(render_slot, completion)
            || !self.manager.as_ref().is_some_and(|manager| manager.render_pair_preparing(render_slot)) {
            return false;
        }
        completion.defer_timeout(error);
        self.deferred_render_timeouts |= 1u64 << render_slot;
        true
    }

    /// Called under Runtime after every installed graph return. Never wait for
    /// firmware or renew a deadline. If an exact expired owner is still pending
    /// after the final post-maintenance observation, classify it immediately.
    pub(crate) fn resolve_deferred_render_timeouts(&mut self) {
        if self.deferred_render_timeouts == 0 || self.is_crashed() || self.primary_recovery_pending() {
            return;
        }
        let mut slots = self.deferred_render_timeouts;
        while slots != 0 {
            let slot = slots.trailing_zeros() as u8;
            let bit = 1u64 << slot;
            slots &= !bit;
            if self.manager.as_ref().is_none_or(|manager| manager.render_pair_preparing(slot)) {
                continue;
            }
            self.deferred_render_timeouts &= !bit;
            // All tokens share retained completion ownership. No extra Arc is
            // allocated and duplicate watchdog callbacks do not add tokens.
            for _ in 0..g17_resources::G17P_RENDER_MAX_IN_FLIGHT {
                let token = Iterator::chain(self.pending_render[usize::from(slot)].iter(),
                    self.pending_render_queued[usize::from(slot)].iter())
                    .find(|owner| owner.deferred_timeout().is_some()).cloned();
                let Some(owner) = token else { break; };
                let Some(error) = owner.take_deferred_timeout() else { continue; };
                self.timeout_deferred_render(slot, owner.submission_id(), &owner, error);
                if self.is_crashed() || self.primary_recovery_pending() { return; }
            }
        }
    }

    /// Settle the render named by a firmware type-4 recovery record.
    ///
    /// Type-1 event masks and type-4 `stamp_index` use the same 128-bit
    /// EventMachine QID namespace. Resolve the current lease owner rather than
    /// inferring a render slot from the numeric QID. Do this after the firmware
    /// reaches state 0: at that point a
    /// final DRAM observation can either salvage an actual completion or
    /// classify the rejected group without waiting for the scheduler's
    /// unrelated two-second watchdog.
    fn settle_render_blamed_by_recovery(&mut self, stamp: u8, error: Error) {
        let Some(slot) = self.manager.as_ref().and_then(|manager|
            manager.render_slot_for_qid(stamp)
        ) else { return; };
        if !self.render_fence_state.pending(slot) { return; }
        self.settle_failed_deferred_render(slot, None, Some(stamp), error);
    }

    /// Exact-once settlement shared by the watchdog and a firmware-blamed
    /// recovery. The optional identities affect diagnostics only; ownership,
    /// final completion observation, VM status, and fence order are identical.
    fn settle_failed_deferred_render(
        &mut self,
        render_slot: u8,
        submission_id: Option<u64>,
        blamed_stamp: Option<u8>,
        error: Error,
    ) {
        // Construction and direct-registration failures happen before a
        // completion owner is installed. They have no published work to
        // observe or abandon; probing the selected slot here manufactured an
        // ENODEV "final watchdog" warning and obscured the original EAGAIN or
        // EBUSY. Only an armed fence denotes a deferred render transaction.
        //
        // This is also the path the scheduler watchdog takes after a
        // firmware-blamed recovery has already settled the same slot. It
        // returned silently, so the only externally visible timeout in the
        // driver could fire and leave nothing at all in the log. Say so.
        if !self.render_fence_state.pending(render_slot) {
            if let Some(pdev) = self.pdev.as_ref() {
                dev_info!(
                    pdev.as_ref(),
                    "G17P render: settle skipped for submission {:?} recovery-stamp {:?} slot {} ({:?}); slot already settled\n",
                    submission_id,
                    blamed_stamp,
                    render_slot,
                    error,
                );
            }
            return;
        }
        // A firmware event can precede visibility of the final paired queue
        // cache lines. Recheck once at the scheduler's existing watchdog
        // boundary before declaring the device lost. This is not a polling
        // completion path: ordinary retirement remains event-driven, and the
        // watchdog supplies the sole bounded lost-event fallback.
        match self.poll_deferred_render_completion(render_slot) {
            Ok(Some(observed)) => {
                match self.settle_deferred_render_completion(render_slot, observed) {
                    Ok(true) => {
                        if let Some(pdev) = self.pdev.as_ref() {
                            if let Some(submission_id) = submission_id {
                                dev_warn!(
                                    pdev.as_ref(),
                                    "G17P render: watchdog recovered completed submission {} slot {} after an early firmware event\n",
                                    submission_id,
                                    render_slot,
                                );
                            } else {
                                dev_warn!(
                                    pdev.as_ref(),
                                    "G17P render: recovery stamp {:?} raced a completed slot {}\n",
                                    blamed_stamp,
                                    render_slot,
                                );
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(cleanup_error) => {
                        if let Some(pdev) = self.pdev.as_ref() {
                            dev_err!(
                                pdev.as_ref(),
                                "G17P render: completed submission {:?} slot {} could not settle retained cleanup ({:?})\n",
                                submission_id,
                                render_slot,
                                cleanup_error,
                            );
                        }
                    }
                }
                return;
            }
            Ok(None) => {}
            Err(observe_error) => {
                if let Some(pdev) = self.pdev.as_ref() {
                    dev_warn!(
                        pdev.as_ref(),
                        "G17P render: final failure observation for submission {:?} recovery-stamp {:?} slot {} failed ({:?})\n",
                        submission_id,
                        blamed_stamp,
                        render_slot,
                        observe_error,
                    );
                }
            }
        }
        self.fail_render_after_final_observation(render_slot, submission_id, blamed_stamp, error);
    }

    /// Fail a published render without releasing anything firmware may name.
    /// Both the blamed pair and VM-loss siblings use the same retained-owner
    /// collection, whose release requires whole-pair drain or processor stop.
    fn quarantine_failed_render_completion(
        &mut self,
        completion: Arc<crate::g17_drm::G17PRenderCompletion>,
        error: Error,
    ) -> bool {
        let newly_failed = completion.fail_while_owned(error);
        if let Err(error) = self.quarantined_render_completions.push_within_capacity(completion) {
            // Capacity covers every in-flight member of every physical pair.
            // If that invariant is violated, leak rather than unmap memory
            // firmware may still dereference.
            core::mem::forget(error.0);
            pr_err!(
                "G17P render: completion quarantine exhausted; retaining VM mappings permanently\n"
            );
        }
        newly_failed
    }

    fn fail_render_after_final_observation(&mut self, render_slot: u8,
        submission_id: Option<u64>, blamed_stamp: Option<u8>, error: Error) {
        if self.render_fence_state.settle(render_slot) {
            self.render_event_masks[render_slot as usize] = [0; 2];
            let Some(signal) = self.pending_render
                .get_mut(render_slot as usize)
                .and_then(Option::take) else { return; };
            let mut queued_signals = g17_completion::PairFifo::<
                Arc<crate::g17_drm::G17PRenderCompletion>,
                { g17_resources::G17P_RENDER_QUEUED_PER_PAIR },
            >::new();
            while let Some(queued) = self.pending_render_queued[render_slot as usize].take() {
                let _ = queued_signals.push(queued);
            }
            // Retirement below must remain forbidden with live successors.
            // That short circuit also skipped the only detailed timeout
            // report, hiding the first failing pair under sustained load.
            // Keep read-only diagnosis independent of retirement permission.
            if !queued_signals.is_none()
                && module_parameters::g17p_fault_report.value() & 4 != 0
            {
                if let Some(manager) = self.manager.as_mut() {
                    if let Err(report_error) = manager.report_queued_render_failure(render_slot) {
                        pr_warn!("G17P queued render failure snapshot failed: {:?}\n", report_error);
                    }
                }
            }
            // iso4: sample before retirement settles the tracker. Whether the
            // head pass's tiler ever stamped its start sizes render replay.
            let head_started = self.manager.as_mut()
                .and_then(|manager| manager.render_slot_head_pass_started(render_slot));
            let queued_count = queued_signals.len();
            let retired = queued_signals.is_none() && self
                .retire_failed_deferred_render(render_slot, submission_id.is_some())
                .unwrap_or(false);
            if let Some(pdev) = self.pdev.as_ref() {
                dev_err!(
                    pdev.as_ref(),
                    "G17P render: settled lost submission {:?} recovery-stamp {:?} slot {} ({:?}, firmware-retired={})\n",
                    submission_id,
                    blamed_stamp,
                    render_slot,
                    error,
                    retired,
                );
            }
            let status = signal.vm_status();
            // Recovery isolation: a spared render fails its own fence (and,
            // unretired, keeps its pair quarantined) but does not take the
            // rest of its VM down with it.
            //
            // iso3.1: the whole failing group shares that class. Work the
            // spared client queued on this pair after the recovery's
            // classification (class unset) fails only because the spared
            // head ahead of it never ran; marking it spared keeps its
            // `fail_while_owned` from writing the sticky VM status. Measured:
            // boot 9701621a 07:18:04.19, spared VM 89's slot 6 head timed
            // out 1.9 s after the recovery and a queued successor poisoned
            // the VM with ETIMEDOUT at g17_drm.rs:1353 just before the
            // "its VM keeps running" line.
            let spared = signal.recovery_spared()
                || queued_signals.iter().any(|queued| queued.recovery_spared());
            if spared {
                signal.set_recovery_class(crate::g17_recovery_isolation::CLASS_SPARED);
                for queued in queued_signals.iter() {
                    if !queued.recovery_spared() {
                        queued.set_recovery_class(crate::g17_recovery_isolation::CLASS_SPARED);
                        crate::g17_recovery_isolation::count(&crate::g17_recovery_isolation::SPARED_SUCCESSORS);
                    }
                }
            }
            if retired {
                signal.complete(Err(EIO));
            } else {
                // The output fence is about to wake a client which may close
                // every mapping this live group names. Quarantine retains the
                // graph; this flag makes the required processor-stop boundary
                // reachable from the next device query/submission instead of
                // leaving the remaining clients permanently capacity-poisoned.
                if crate::g17_config::QUARANTINE_SCOPE & 2 == 0 {
                    note_session_recovery_pending(
                        &mut self.session_recovery_pending,
                        self.pdev.as_ref(),
                        "render-submission-lost",
                    );
                } else if let Some(pdev) = self.pdev.as_ref() {
                    dev_err!(
                        pdev.as_ref(),
                        "G17PMARK quarantine-scoped render slot {} -- the losing client's VM is still poisoned and the slot is retired, but the session stays up for every other client; render capacity is now reduced for the rest of this boot\n",
                        render_slot,
                    );
                }
                self.quarantine_failed_render_completion(signal, error);
                while let Some(queued) = queued_signals.take() {
                    self.quarantine_failed_render_completion(queued, error);
                }
            }
            if spared {
                crate::g17_recovery_isolation::count(&crate::g17_recovery_isolation::FANOUT_SKIPPED);
                match head_started {
                    Some(false) => crate::g17_recovery_isolation::count(
                        &crate::g17_recovery_isolation::SPARED_RENDER_UNSTARTED),
                    Some(true) => crate::g17_recovery_isolation::count(
                        &crate::g17_recovery_isolation::SPARED_RENDER_STARTED),
                    None => {}
                }
                if let Some(pdev) = self.pdev.as_ref() {
                    dev_info!(pdev.as_ref(),
                        "G17P recovery isolation: spared render slot {} failed ({:?}, firmware-retired={}, pass-started={:?}, queued={}); its VM keeps running\n",
                        render_slot, error, retired, head_started, queued_count);
                }
            } else {
                self.fail_outstanding_jobs_for_vm(&status, error);
            }
        }
    }

    /// Release a timed-out render.
    ///
    /// Returns `true` when the firmware had already retired the group without
    /// executing it: the slot's host state has then been settled exactly as a
    /// completion settles it and the slot stays usable. `false` means the
    /// firmware may still own the group, which is left pinned as before.
    pub(crate) fn abandon_deferred_render(
        &mut self,
        render_slot: u8,
    ) -> Result<bool> {
        self.retire_failed_deferred_render(render_slot, true)
    }

    fn retire_failed_deferred_render(
        &mut self,
        render_slot: u8,
        timed_out: bool,
    ) -> Result<bool> {
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .ensure_render_slot(render_slot)?;
        // Run the classifier on BOTH settle paths.
        //
        // It was gated on `timed_out`, which is `submission_id.is_some()`, and
        // the recovery-blamed path passes `None` -- so the one route that
        // actually kills the desktop was the one route that produced no
        // explanation at all. Measured on the 236s soak wedge: `settled lost
        // submission None recovery-stamp Some(0) ... firmware-retired=false`
        // with no render state logged anywhere in the boot. The body is a plain
        // read of driver-allocated DRAM plus one line, and it makes no claim
        // about why the settle happened, so it is correct on either path.
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .report_render_wait_timeout(render_slot, if timed_out {
                "inline-wait-expired"
            } else {
                "recovery-blamed"
            });
        let retired = match self
            .manager
            .as_mut()
            .ok_or(ENODEV)?
            // Only the recovery-blamed path may accept an executed group, and
            // only under the probe. An inline wait that simply expired proves
            // nothing about firmware ownership, so it keeps the strict rule.
            .retire_abandoned_render_if_firmware_retired(render_slot,
                !timed_out
                    && crate::g17_config::RENDER_RECOVERY_RETIRE != 0,
            )
        {
            Ok(retired) => retired,
            Err(error) => {
                if let Some(pdev) = self.pdev.as_ref() {
                    dev_warn!(
                        pdev.as_ref(),
                        "G17P render: could not classify the timed-out group ({:?}); treating it as still owned by firmware\n",
                        error
                    );
                }
                false
            }
        };
        if retired {
            // The firmware is finished with the group: drain its notifications
            // and retire the FList references the way a completion does, so the
            // device-global FList refcount does not leak two references per
            // abandoned render. Neither failure changes the slot's state.
            let _ = self.service_firmware_events_inline();
            self.manager
                .as_mut()
                .ok_or(ENODEV)?
                .ensure_render_slot(render_slot)?;
            match self
                .manager
                .as_mut()
                .ok_or(ENODEV)?
                .retire_render_usc_freelist_at_completion(render_slot)
            {
                Ok(Some(release)) => {
                    if let Err(error) = self.service_render_usc_freelist_release(release) {
                        if error != EAGAIN {
                            if let Some(pdev) = self.pdev.as_ref() {
                                dev_warn!(
                                    pdev.as_ref(),
                                    "G17P render: FList release after a firmware-retired group failed ({:?})\n",
                                    error
                                );
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(pdev) = self.pdev.as_ref() {
                        dev_warn!(
                            pdev.as_ref(),
                            "G17P render: FList retire after a firmware-retired group failed ({:?})\n",
                            error
                        );
                    }
                }
            }
            return Ok(true);
        }
        if timed_out {
            self.abandon_render_submission_after_timeout();
        }
        Ok(false)
    }

    /// Wait only for the two TA-owned timestamps used by the split-launch
    /// diagnostic. The sample path is retained DRAM; it performs no SGX MMIO,
    /// completion acknowledgement, recovery service, or fragment publication.
    /// The bound is deliberately below the measured ~84ms fragment recovery,
    /// so a failed TA phase cannot leave firmware halted at recovery state 1
    /// while this diagnostic keeps polling.
    /// Release a timed-out render without tearing down the runtime.
    ///
    /// Drops this submission's retained render state so the next submission
    /// builds its own, and leaves the session accepting work.
    fn abandon_render_submission_after_timeout(&mut self) {
        if let Some(pdev) = self.pdev.as_ref() {
            dev_warn!(
                pdev.as_ref(),
                "G17P render: abandoning the timed-out submission; runtime stays up\n"
            );
        }
        // Deliberately release nothing here. The firmware may still hold
        // references to this submission's graph, and freeing it under a live
        // pair of firmware CPUs is how you turn a stalled render into a DMA
        // fault. The next submission reuses or rebuilds through the retained
        // paths it already has; the only thing this path must guarantee is
        // that the session keeps accepting work.
    }

    fn wait_split_render_tiling_completion(&mut self, render_slot: u8) -> Result<[u64; 2]> {
        const POLLS: usize = 50;
        for _ in 0..POLLS {
            if let Some(timestamps) = self
                .manager
                .as_mut()
                .ok_or(ENODEV)?
                .split_render_tiling_completion(render_slot)?
            {
                return Ok(timestamps);
            }
            fsleep(Delta::from_millis(1));
        }
        if let Some(pdev) = self.pdev.as_ref() {
            dev_err!(
                pdev.as_ref(),
                "G17P render split-launch: TA did not complete within {} DRAM polls; 3D remains unpublished\n",
                POLLS
            );
        }
        Err(ETIMEDOUT)
    }

    /// Construct one of a logical queue's physical render pairs while the
    /// queue has no live kick, so it is warm before any kick can overlap it.
    ///
    /// This is the native model: AGXCommandQueue creates both AGX3DWorkQueues
    /// at command-queue creation, each with its pools populated, so a work
    /// queue is never built while its sibling executes. Everything a first
    /// render would otherwise build cold happens here instead -- the VM
    /// handoff (a same-VM graph is retargeted in place; a closed foreign
    /// owner's graph is moved, whole, into that owner's teardown and freed
    /// when the firmware acknowledges its scheduler-state release; nothing
    /// is ever reused across VMs), then a fresh graph at this VM's own
    /// reserved addresses: render storage, PM generation, FList repopulation
    /// and UMA owner staging, SKSM queue objects and the direct KSMFE
    /// configuration of both queue objects under registration power. Only
    /// the per-kick work stays with the first kick: the tag-15 install
    /// record, SKSM entries, AddKicks and the doorbell.
    ///
    /// Idempotent and cheap for a pair that is already warm.
    pub(crate) fn warm_render_pair(
        &mut self,
        vm: &mmu::Vm,
        render_slot: u8,
        execution_context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
        command: &g17_uapi::TranslatedRenderCommand,
    ) -> Result {
        if !self.accepting_submissions {
            return Err(ENOTSUPP);
        }
        if self.is_crashed() {
            self.rebuild_session_or_defer()?;
        }
        let queue_pair = self.manager.as_mut().ok_or(ENODEV)?.render_queue_pair(render_slot)?;
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        manager.ensure_render_slot(render_slot)?;
        if manager.render_pair_is_warm(render_slot) {
            return Ok(());
        }
        // Firmware-visible tables are written below; never while a recovery
        // is open (same rule as publication).
        self.settle_open_recovery_before_publication()?;
        let descriptor_context_id = execution_context.context_id() as u16;
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        manager.ensure_render_slot(render_slot)?;
        // The exact handoff the first kick used to take cold beside live
        // work: retarget a same-VM graph in place, or displace a closed
        // foreign owner's graph into its teardown (EBUSY while that owner's
        // last kicks are still retiring -- the caller defers, it does not
        // fail its job) and fall through to a fresh graph.
        manager.handoff_retained_render_vm(render_slot, vm, execution_context.clone())?;
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        manager.ensure_render_slot(render_slot)?;
        if manager.render_pair_is_warm(render_slot) {
            return Ok(());
        }
        if manager.retained_render_needs_recycle(render_slot, vm, descriptor_context_id) {
            // A session recycle is the publication path's decision.
            return Ok(());
        }
        let drm = self.drm.as_ref().ok_or(ENODEV)?.clone();
        let mut manager = self.manager.take().ok_or(ENODEV)?;
        let registers = self.registers.take().ok_or(ENODEV)?;
        let mut registration_powered = false;
        let result = manager.warm_render_pair_storage(
            &drm,
            vm,
            queue_pair,
            execution_context,
            command,
            &registers,
            &mut |manager| {
                registration_powered = false;
                self.ensure_g17p_render_registration_power(manager, &registers)?;
                registration_powered = true;
                Ok(())
            },
        );
        if registration_powered {
            if let Err(error) =
                self.release_g17p_render_registration_power(&mut manager, &registers)
            {
                if let Some(pdev) = self.pdev.as_ref() {
                    dev_warn!(
                        pdev.as_ref(),
                        "G17P render warm-up: failed to restore ordinary idle policy ({:?}); idle descent remains disabled\n",
                        error,
                    );
                }
            }
        }
        self.manager = Some(manager);
        self.registers = Some(registers);
        result
    }

    /// Reserve one idle graph generation. Its allocation and private VM
    /// mappings are built with Runtime released, under a preparation epoch.
    pub(crate) fn take_warm_render_graph(
        &mut self, vm: &mmu::Vm, render_slot: u8,
        context: Arc<g17_resources::G17PLogicalQueueContext>,
        completion: &crate::g17_drm::G17PRenderCompletion,
        command: &g17_uapi::TranslatedRenderCommand,
        settled: &mut bool,
    ) -> Result<Option<(crate::driver::AsahiDevRef, KBox<g17_manager::G17PWarmRenderGraph>)>> {
        if !self.accepting_submissions { return Err(EAGAIN); }
        if self.is_crashed() { self.rebuild_session_or_defer()?; }
        self.settle_open_recovery_once(settled)?;
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        manager.ensure_render_slot(render_slot)?;
        if manager.render_pair_preparing(render_slot) { return Err(EAGAIN); }
        manager.handoff_retained_render_vm(render_slot, vm, context.clone())?;
        if manager.retained_render_needs_recycle(render_slot, vm, context.context_id() as u16) {
            self.rebuild_session_or_defer()?;
            return Err(EAGAIN);
        }
        let drm = self.drm.as_ref().ok_or(ENODEV)?.clone();
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        let pair = manager.render_queue_pair(render_slot)
            .inspect_err(|error| crate::g17_queue_slots::note_error(
                crate::g17_queue_slots::Pool::QidRenderPair, *error))?;
        Ok(manager.take_warm_render_graph(vm, pair, context, completion.work_state()?, command)?
            .map(|request| (drm, request)))
    }

    /// Ownership must return even after the epoch closes. Do not test
    /// accepting_submissions here: recovery waits for this exact transaction.
    #[inline(never)]
    pub(crate) fn return_warm_render_graph(
        &mut self, request: &mut g17_manager::G17PWarmRenderGraph, commit: bool,
    ) -> Result {
        let drm = self.drm.as_ref().expect("preparation pins DRM device").clone();
        let mut manager = self.manager.take().expect("preparation pins manager");
        let registers = self.registers.take().expect("preparation pins registers");
        let mut powered = false;
        let commit = commit && self.accepting_submissions && !self.is_crashed();
        let result = manager.return_warm_render_graph(&drm, request, commit, &registers,
            &mut |manager| {
                self.ensure_g17p_render_registration_power(manager, &registers)?;
                powered = true;
                Ok(())
            });
        if powered {
            if let Err(error) = self.release_g17p_render_registration_power(&mut manager, &registers) {
                dev_warn!(drm.as_ref(), "G17P render warm-up: idle policy restore failed ({:?})\n", error);
            }
        }
        self.manager = Some(manager);
        self.registers = Some(registers);
        self.resolve_deferred_render_timeouts();
        if result.is_err() && request.installed {
            // Registration can expose queue addresses before returning an
            // error. Retain the installed graph and reserved IDs until stop;
            // recovery joins this caller's epoch before replacing the manager.
            self.request_session_rebuild(result.as_ref().err().copied().unwrap_or(EIO));
        }
        result
    }

    /// Reserve an installed pair under Runtime; its next command generation
    /// can then be prepared without excluding other pairs or event service.
    pub(crate) fn take_render_restage(
        &mut self, vm: &mmu::Vm, render_slot: u8,
        context: Arc<g17_resources::G17PLogicalQueueContext>,
        completion: &crate::g17_drm::G17PRenderCompletion,
        command: &g17_uapi::TranslatedRenderCommand,
        settled: &mut bool,
    ) -> Result<Option<(crate::driver::AsahiDevRef, g17_manager::G17PRenderRestage)>> {
        let mut fine = [crate::g17_e2e::fine(), 0, 0, 0];
        if !self.accepting_submissions || self.is_crashed() { return Err(EAGAIN); }
        self.settle_open_recovery_once(settled)?;
        if fine[0] != 0 { fine[1] = crate::g17_e2e::now(); }
        let drm = self.drm.as_ref().ok_or(ENODEV)?.clone();
        let mut dependencies = self.take_render_dependency_scratch()?;
        let taken = (|| {
        self.render_submission_dependencies(completion, command, &mut dependencies)?;
        if fine[0] != 0 { fine[2] = crate::g17_e2e::now(); }
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        if manager.render_pair_preparing(render_slot) { return Err(EAGAIN); }
        manager.ensure_render_slot(render_slot)?;
        manager.handoff_retained_render_vm(render_slot, vm, context.clone())?;
        if fine[0] != 0 { fine[3] = crate::g17_e2e::now(); }
        let pair = manager.render_queue_pair(render_slot)
            .inspect_err(|error| crate::g17_queue_slots::note_error(
                crate::g17_queue_slots::Pool::QidRenderPair, *error))?;
        manager.take_render_restage(&drm, vm, pair, context, completion.work_state()?, command, &dependencies)
        })();
        self.render_dependency_scratch = Some(dependencies);
        let taken = taken?;
        if fine[0] != 0 {
            crate::g17_e2e::record(crate::g17_e2e::K_FINE_RESERVE, 0, u64::from(render_slot),
                [fine[0], fine[1], fine[2], fine[3], crate::g17_e2e::now()]);
        }
        Ok(taken.map(|prepared| (drm, prepared)))
    }

    pub(crate) fn return_render_restage(&mut self, prepared: g17_manager::G17PRenderRestage) -> Result {
        // The preparation epoch excludes manager reset/removal until return.
        let manager = self.manager.as_mut().expect("preparation pins manager");
        let (slot, result) = manager.return_render_restage(prepared);
        let needs_wake = manager.preparation_grow_pending()
            || self.render_event_masks[usize::from(slot)] != [0; 2];
        self.resolve_deferred_render_timeouts();
        // A consumed completion or grow may be waiting for this exact graph.
        // Return supplies its wake even when firmware sends no further IRQ.
        if needs_wake {
            if let Some(state) = self.gfx_state.as_ref() {
                crate::driver::queue_g17p_firmware_event_worker(state.dev.clone());
            }
        }
        result.map_err(|error| self.request_session_rebuild(error))
    }

    pub(crate) fn cancel_render_restage(&mut self, render_slot: u8) -> Result {
        self.manager.as_mut().ok_or(ENODEV)?.cancel_render_restage(render_slot)
    }

    /// Take the preallocated dependency scratch (reallocated only if a
    /// previous failure path lost it).
    fn take_render_dependency_scratch(&mut self) -> Result<KBox<g17_manager::G17PRenderDependencies>> {
        match self.render_dependency_scratch.take() {
            Some(scratch) => Ok(scratch),
            None => Ok(KBox::new(g17_manager::G17PRenderDependencies::empty(), GFP_KERNEL)?),
        }
    }

    /// Build this render's explicit dependency set into `out` (heap scratch).
    /// `out` is valid only when this returns Ok.
    fn render_submission_dependencies(&mut self,
        completion: &crate::g17_drm::G17PRenderCompletion,
        command: &g17_uapi::TranslatedRenderCommand,
        out: &mut g17_manager::G17PRenderDependencies,
    ) -> Result {
        out.count = 0;
        out.entries.fill(g17_manager::G17PRenderDependencies::EMPTY_ENTRY);
        let render_dependencies = &mut out.entries;
        let mut dependency_sequences = [0; g17_submission::G17P_CL_EXPLICIT_DEPENDENCIES_MAX];
        let mut dependency_count = 0;
        if command.render_order.wait_through.is_some_and(|limit| limit != 0) {
            for predecessor in Iterator::chain(
                self.pending_render.iter().flatten(),
                self.pending_render_queued.iter().flatten(),
            )
            {
                if let Some(dependency) = completion.predecessor_dependency(predecessor)? {
                    g17_submission::g17p_insert_prefix_dependency(
                        render_dependencies, &mut dependency_sequences, &mut dependency_count,
                        predecessor.render_sequence(), dependency,
                    ).map_err(|_| EOVERFLOW)?;
                }
            }
        }
        // Only the captured compute prefix orders execution across engines.
        // Its scheduled fences guarantee those tokens have been published;
        // later independent kicks must not become parents of this render.
        if let Some(dependency) = self.async_compute_native_dependency(
            completion.owner(), &completion.vm_status(),
            command.compute_wait_through,
        )? {
            *render_dependencies.get_mut(dependency_count).ok_or(EOVERFLOW)? = dependency;
            dependency_count += 1;
        }
        out.count = dependency_count;
        Ok(())
    }

    /// A real eligible render job may announce work before CPU preparation.
    /// This is the native notifyFirmware(4, 1, 0) note, not an idle hold.
    pub(crate) fn kick_pending_render_submission(&mut self) -> Result {
        if self.is_crashed() { return Err(EIO); }
        let rtkit = self.gfx_rtkit.as_mut().ok_or(ENODEV)?;
        Pin::new(rtkit).send_message_logged(
            g17_rtkit::ENDPOINT_GFX_INTERRUPTS, 0x0087_0000_0000_0010)
    }

    /// Request the normal work-ring scan for an eligible render job.
    /// The firmware calls its wake routine before checking the ring cursors.
    /// This neither publishes unfinished work nor replaces the final doorbell.
    pub(crate) fn wake_pending_render_submission(&mut self) -> Result {
        if self.is_crashed() { return Err(EIO); }
        if crate::g17_timing_buffer::diag_knob(crate::g17_timing_buffer::DIAG_IDLE_RENDER_DOORBELL)
            && (self.pending_compute.is_some()
                || self.pending_render.iter().any(Option::is_some)
                || self.pending_render_queued.iter().flatten().next().is_some()
                || self.async_compute_queues.iter().any(|queue| queue.inflight.is_some())
                || !self.quarantined_render_completions.is_empty())
        {
            // These are submitted-work witnesses under Runtime, not an
            // assertion about firmware power state. Keep the final doorbell
            // unconditional; this merely suppresses an unnecessary early scan.
            return Ok(());
        }
        let rtkit = self.gfx_rtkit.as_mut().ok_or(ENODEV)?;
        Pin::new(rtkit).send_message_logged(
            g17_rtkit::ENDPOINT_GFX_INTERRUPTS, 0x0083_0000_0000_0000)
    }

    /// Open the native submit-in-progress bracket (diag knob bit6), or the
    /// opt-in `g17p_dvfs_parity` bundle. With every knob and parity bit zero
    /// this performs no RegionC write and sends no message, as in v68.
    fn submit_accounting_begin(&mut self) {
        use crate::g17_dvfs_policy as policy;
        self.gpu_power_sync();
        let parity = crate::g17_dvfs_parity::config();
        if parity.mask & policy::PARITY_CE != 0 {
            // Native CLPC-driven RegionC+0xe4, approximated from real work:
            // raised by this submission, cleared by the idle re-check.
            let now = crate::g17_timing_buffer::now();
            let want = self.dvfs_ce.on_submit(now, &parity);
            self.dvfs_publish_effort(want, now);
        } else {
            if self.dvfs_ce.streaming() {
                self.dvfs_ce.reset();
                self.dvfs_publish_effort(0, crate::g17_timing_buffer::now());
            }
            let effort = if crate::g17_timing_buffer::diag_knob(crate::g17_timing_buffer::DIAG_CLPC_EFFORT) {
                0x10000
            } else {
                0
            };
            if effort != self.clpc_effort {
                if let Some(manager) = self.manager.as_mut() {
                    if manager.publish_clpc_effort(effort).is_ok() {
                        self.clpc_effort = effort;
                    }
                }
            }
        }
        let native_bracket = parity.mask & policy::PARITY_BRACKET != 0;
        if !native_bracket
            && !crate::g17_timing_buffer::diag_knob(crate::g17_timing_buffer::DIAG_SUBMIT_ACCOUNTING)
        {
            // Restore the legacy static words once after an accounting phase.
            if self.submit_count != 1 {
                if let Some(manager) = self.manager.as_mut() {
                    if manager.publish_submit_in_progress(1, 1).is_ok() {
                        self.submit_count = 1;
                    }
                }
            }
            // The kick diagnostic is independent of submit accounting.
            // Previously bit7 alone returned here without sending a note,
            // invalidating the claimed kick-only negative experiment.
            if crate::g17_timing_buffer::diag_knob(crate::g17_timing_buffer::DIAG_SUBMIT_KICK) {
                let _ = self.kick_pending_render_submission();
            }
            return;
        }
        let submits = self.submit_count.wrapping_add(1).max(2);
        if let Some(manager) = self.manager.as_mut() {
            let published = if native_bracket {
                manager.publish_native_submit_bracket(true, submits)
            } else {
                manager.publish_submit_in_progress(1, submits)
            };
            if published.is_ok() {
                self.submit_count = submits;
                self.submit_open = true;
                self.dvfs_native_bracket = native_bracket;
                if native_bracket {
                    crate::g17_dvfs_parity::BRACKET_OPENS.fetch_add(1, Ordering::Relaxed);
                }
            } else if native_bracket {
                crate::g17_dvfs_parity::PUBLISH_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
        }
        if !self.submit_open {
            return;
        }
        if native_bracket && parity.mask & policy::PARITY_KICK != 0 {
            // Native notifyFirmware(4, 1, 0) from inside
            // updateSubmitInProgress(true): only for priority != 2 and when
            // GFX1 published stats+0x66c != 0 (kext 88a7f40..88a7f60).
            let stats = self.manager.as_mut()
                .and_then(|manager| manager.read_gfx1_stats_u32(policy::GFX1_STATS_KICK_GATE).ok())
                .unwrap_or(0);
            crate::g17_dvfs_parity::STATS_66C_LAST.store(stats, Ordering::Relaxed);
            if stats != 0 {
                crate::g17_dvfs_parity::STATS_66C_NONZERO.fetch_add(1, Ordering::Relaxed);
            }
            if policy::kick_gate(stats, policy::NATIVE_DEFAULT_PRIORITY) {
                if let Some(rtkit) = self.gfx_rtkit.as_mut() {
                    if Pin::new(rtkit).send_message_logged(
                        g17_rtkit::ENDPOINT_GFX_INTERRUPTS, 0x0087_0000_0000_0010).is_ok()
                    {
                        crate::g17_dvfs_parity::KICKS_SENT.fetch_add(1, Ordering::Relaxed);
                    }
                }
            } else {
                crate::g17_dvfs_parity::KICKS_GATED.fetch_add(1, Ordering::Relaxed);
            }
        } else if crate::g17_timing_buffer::diag_knob(crate::g17_timing_buffer::DIAG_SUBMIT_KICK) {
            if let Some(rtkit) = self.gfx_rtkit.as_mut() {
                let _ = Pin::new(rtkit).send_message_logged(
                    g17_rtkit::ENDPOINT_GFX_INTERRUPTS, 0x0087_0000_0000_0010);
            }
        }
    }

    /// GPU power budget (native MaxGPUAbsolutePower): register with the
    /// optional thermal shim once, then publish its latched budget to
    /// RegionC+0x84/+0x9c. The live word is read back so a session
    /// re-encode (which restores the encoded default) is republished.
    fn gpu_power_sync(&mut self) {
        use crate::g17_gpu_power as power;
        let Some(manager) = self.manager.as_mut() else { return; };
        if !power::attempted() {
            if let Ok(default_mw) = manager.read_power_target() {
                power::register_once(default_mw);
            }
        }
        let want = power::TARGET_MW.load(Ordering::Acquire);
        if want == 0 {
            return;
        }
        if self.power_target_mw == want && manager.read_power_target().ok() == Some(want) {
            return;
        }
        let reason = power::REASON.load(Ordering::Relaxed);
        if manager.publish_power_target(want, reason).is_ok() {
            self.power_target_mw = want;
            power::PUBLISHES.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Close the bracket opened by [`Self::submit_accounting_begin`].
    fn submit_accounting_end(&mut self) {
        if !self.submit_open {
            return;
        }
        self.submit_open = false;
        let submits = self.submit_count;
        let native_bracket = core::mem::replace(&mut self.dvfs_native_bracket, false);
        if let Some(manager) = self.manager.as_mut() {
            let _ = if native_bracket {
                manager.publish_native_submit_bracket(false, submits)
            } else {
                manager.publish_submit_in_progress(0, submits)
            };
        }
    }

    /// Publish RegionC+0xe4 for the opt-in parity effort. The live word is
    /// read back so a session re-encode (which writes 0) is republished.
    fn dvfs_publish_effort(&mut self, want: u32, now: u64) {
        let Some(manager) = self.manager.as_mut() else { return; };
        let live = manager.read_clpc_effort().ok();
        if live == Some(want) && self.clpc_effort == want {
            return;
        }
        if manager.publish_clpc_effort(want).is_ok() {
            self.clpc_effort = want;
            crate::g17_dvfs_parity::CE_LIVE.store(want, Ordering::Relaxed);
            if want != 0 {
                crate::g17_dvfs_parity::CE_PUBLISHES.fetch_add(1, Ordering::Relaxed);
                crate::g17_dvfs_parity::CE_LAST_SET_NS.store(now, Ordering::Relaxed);
            } else {
                crate::g17_dvfs_parity::CE_CLEARS.fetch_add(1, Ordering::Relaxed);
                crate::g17_dvfs_parity::CE_LAST_CLEAR_NS.store(now, Ordering::Relaxed);
            }
        } else {
            crate::g17_dvfs_parity::PUBLISH_ERRORS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Submitted, unretired work the host tracks. Quarantined completions are
    /// deliberately excluded: the effort must never outlive real work.
    fn dvfs_host_tracks_submitted_work(&self) -> bool {
        self.pending_compute.is_some()
            || self.pending_render.iter().any(Option::is_some)
            || self.pending_render_queued.iter().flatten().next().is_some()
            || self.async_compute_queues.iter().any(|queue| queue.inflight.is_some())
    }

    /// Opt-in `g17p_dvfs_parity` bit0 idle evaluation after completion
    /// service and from the delayed re-check. Returns the re-check delay (ns)
    /// while idle inside the hold window.
    pub(crate) fn dvfs_parity_idle_check(&mut self) -> Option<u64> {
        use crate::g17_dvfs_policy as policy;
        let parity = crate::g17_dvfs_parity::config();
        let now = crate::g17_timing_buffer::now();
        if parity.mask & policy::PARITY_CE == 0 {
            if self.dvfs_ce.streaming() {
                self.dvfs_ce.reset();
                if !crate::g17_timing_buffer::diag_knob(crate::g17_timing_buffer::DIAG_CLPC_EFFORT) {
                    self.dvfs_publish_effort(0, now);
                }
            }
            return None;
        }
        let in_flight = self.dvfs_host_tracks_submitted_work();
        match self.dvfs_ce.on_idle_check(now, in_flight, &parity) {
            policy::IdleAction::None => None,
            policy::IdleAction::Clear => {
                self.dvfs_publish_effort(0, now);
                None
            }
            policy::IdleAction::Recheck(delay) => Some(delay),
        }
    }

    pub(crate) fn submit_translated_render(
        &mut self,
        vm: &mmu::Vm,
        render_slot: u8,
        defer_completion: bool,
        execution_context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
        command: &g17_uapi::TranslatedRenderCommand,
        completion: Arc<crate::g17_drm::G17PRenderCompletion>,
        stages: &mut crate::g17_perf::RenderStageTimes,
    ) -> Result {
        crate::g17_timing_buffer::msglog(crate::g17_timing_buffer::MSG_SUBMIT,
            render_slot as u64, 0, 0);
        self.submit_accounting_begin();
        let result = self.submit_translated_render_inner(vm, render_slot, defer_completion,
            execution_context, command, completion, stages);
        self.submit_accounting_end();
        crate::g17_timing_buffer::msglog(crate::g17_timing_buffer::MSG_SUBMIT,
            render_slot as u64, 1, result.is_ok() as u64);
        result
    }

    fn submit_translated_render_inner(
        &mut self,
        vm: &mmu::Vm,
        render_slot: u8,
        defer_completion: bool,
        execution_context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
        command: &g17_uapi::TranslatedRenderCommand,
        completion: Arc<crate::g17_drm::G17PRenderCompletion>,
        stages: &mut crate::g17_perf::RenderStageTimes,
    ) -> Result {
        // Acquire before taking manager/register/dependency ownership so an
        // already-completed aggregate cannot bypass their restoration paths.
        let work_state = completion.work_state()?;
        stages.lap_start();
        if !self.accepting_submissions {
            return Err(ENOTSUPP);
        }
        let queue_pair = self.manager.as_mut().ok_or(ENODEV)?.render_queue_pair(render_slot)?;
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .ensure_render_slot(render_slot)?;
        let native_doorbell_mode =
            crate::g17_config::RENDER_NATIVE_DOORBELLS;
        let native_doorbells = native_doorbell_mode != 0;
        let split_launch = crate::g17_config::RENDER_SPLIT_LAUNCH != 0;
        if native_doorbell_mode > 3 || (native_doorbells && split_launch) {
            if let Some(pdev) = self.pdev.as_ref() {
                dev_err!(
                    pdev.as_ref(),
                    "G17P render: native doorbell pair and split launch are mutually exclusive\n"
                );
            }
            return Err(EINVAL);
        }
        if self.is_crashed() {
            self.rebuild_session_or_defer()?;
        }
        // Never publish while a firmware recovery is open. The compute path
        // has done this since the recovery work; render never did, and the
        // firmware behaviour it guards against is exactly render's symptom:
        // while `status_b[0x4900] != 0` the scheduler dispatches only the
        // unhalt event and STASHES EVERY WORK-SCAN BIT, so nothing published
        // can run -- and the unhalt's 128-queue restore rewrites
        // 0x21010/0x21018/0x21020 for every queue from the halt-time
        // snapshot, erasing any tag-14 AddKicks published while it was open.
        // A render that is admitted, armed on its pipe and never launched is
        // what that produces.
        self.settle_open_recovery_before_publication()?;
        // Inline event servicing scans every pending render slot and leaves
        // the final scanned slot selected. Restore this submission's slot
        // before handoff reads or replaces the active graph, bind, tracker,
        // hardware-buffer owner, retirement state, and SKSM backing.
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .ensure_render_slot(render_slot)?;
        // The legacy synchronous compute path keeps its client resources in
        // fixed contexts 2/3; this render owns a distinct application-GART
        // context and embeds its complete low-view closure there.  They no
        // longer alias, so neither an idle nor an in-flight QID4 binding is a
        // reason to hand the compute graph back before render publication.
        let descriptor_context_id = execution_context.context_id() as u16;
        stages.lap(crate::g17_perf::SUB_FRONT);
        let handoff_started = stages.enabled.then(
            kernel::time::Instant::<kernel::time::Monotonic>::now);
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .handoff_retained_render_vm(render_slot, vm, execution_context.clone())?;
        if let Some(started) = handoff_started {
            stages.handoff_ns = started.elapsed().as_nanos().max(0) as u64;
        }
        stages.skip();
        let recycle = self
            .manager
            .as_ref()
            .ok_or(ENODEV)?
            .retained_render_needs_recycle(render_slot, vm, descriptor_context_id);
        if recycle {
            self.rebuild_session_or_defer()?;
        }
        // A recovery reconstructs the manager with slot 0 selected. Restore
        // the caller's physical pair before asking whether this pair is fresh.
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .ensure_render_slot(render_slot)?;
        let drm = self.drm.as_ref().ok_or(ENODEV)?.clone();
        // Temporarily own the manager and register aperture separately so the
        // manager can ask the live runtime to perform ensurePoweredHardware at
        // the exact configureHardware boundary.  The previous preamble did the
        // power transaction before graph construction; verbose construction
        // alone was long enough for firmware to close the aperture again.
        // Render entity FIFO has published every earlier logical render by
        // now. The runtime mutex excludes retirement while we snapshot the
        // live prefix; successfully retired pairs need no dependency. Keep
        // each remaining pair's exact native token, including independent
        // earlier renders which the last render alone does not cover.
        if completion.vm_status().get() != 0 { return Err(EIO); }
        // Entity FIFO and publication fences made the captured prefix final
        // before the pair was borrowed. Retirements during preparation only
        // satisfy dependencies already encoded in those entries. A failed
        // predecessor poisons the VM checked above. Do not rescan the device's
        // pending queues or retake fence locks for an already prepared entry.
        let sksm_prepared = self.manager.as_ref().ok_or(ENODEV)?
            .render_sksm_prepared(render_slot);
        // Heap scratch, returned right after begin_translated_render consumes
        // the slice below.
        let mut dependency_scratch = self.take_render_dependency_scratch()?;
        if !sksm_prepared {
            if let Err(error) = self.render_submission_dependencies(
                &completion, command, &mut dependency_scratch)
            {
                self.render_dependency_scratch = Some(dependency_scratch);
                return Err(error);
            }
        }
        let render_dependencies = if sksm_prepared { &[][..] } else {
            &dependency_scratch.entries[..dependency_scratch.count]
        };
        let dependency_count = render_dependencies.len();
        // Diagnostic modes 2/3 construct TA later and do not accept this
        // dependency slice. Refuse an unresolved prefix instead of losing it.
        if dependency_count != 0 && (native_doorbell_mode == 2 || native_doorbell_mode == 3) {
            self.render_dependency_scratch = Some(dependency_scratch);
            return Err(ENOTSUPP);
        }
        let mut manager = self.manager.take().ok_or(ENODEV)?;
        let registers = self.registers.take().ok_or(ENODEV)?;
        let mut registration_powered = false;
        let prepare_started = stages.enabled.then(
            kernel::time::Instant::<kernel::time::Monotonic>::now);
        let prepared = manager.begin_translated_render(
            &drm,
            vm,
            queue_pair,
            execution_context,
            work_state,
            command,
            &render_dependencies[..dependency_count],
            &registers,
            &mut |manager| {
                registration_powered = false;
                self.ensure_g17p_render_registration_power(manager, &registers)?;
                registration_powered = true;
                Ok(())
            },
            stages,
        );
        self.render_dependency_scratch = Some(dependency_scratch);
        if let Some(started) = prepare_started {
            stages.prepare_ns = started.elapsed().as_nanos().max(0) as u64;
        }
        // `ensurePoweredHardware(true)` is a scoped native prerequisite. The
        // IdlePowerOff(0) disables firmware idle descent around the direct
        // KSMFE registration. Restore ordinary idle policy before any work
        // doorbell.
        if registration_powered {
            if let Err(error) = self.release_g17p_render_registration_power(
                &mut manager,
                &registers,
            ) {
                if let Some(pdev) = self.pdev.as_ref() {
                    dev_warn!(
                        pdev.as_ref(),
                        "G17P render registration: failed to restore ordinary idle policy ({:?}); idle descent remains disabled\n",
                        error,
                    );
                }
            }
        }
        self.manager = Some(manager);
        self.registers = Some(registers);
        let prepared = prepared?;
        completion.set_fragment_dependency(
            self.manager.as_ref().ok_or(ENODEV)?.staged_render_fragment_dependency(render_slot)?,
        );
        // Arm the exact fence after the tracker has been reset for this group,
        // but before any control or work doorbell can report its retirement.
        self.install_render_completion(render_slot, completion)?;
        // Copy only before the FList wake and while both outers are deferred.
        // Never insert this scan between producer publication and doorbell.
        if let Some(manager) = self.manager.as_mut() {
            if let Err(error) = manager.capture_prepared_render_trace(render_slot,
                native_doorbell_mode == 0 && !split_launch,
            ) {
                if let Some(pdev) = self.pdev.as_ref() {
                    dev_warn!(pdev.as_ref(), "G17P prepared binary capture failed: {:?}\n", error);
                }
            }
        }
        // Retain and announce cold FList growth before exposing render outer
        // producers. A consumed-control snapshot does not imply a CPU wait:
        // native keeps the grow outstanding while publishing dependent work.
        stages.lap(crate::g17_perf::SUB_POST);
        let flist_started = stages.enabled.then(
            kernel::time::Instant::<kernel::time::Monotonic>::now);
        if let Err(error) = self.publish_render_usc_freelist_request(render_slot) {
            return Err(self.request_session_rebuild(error));
        }
        if let Some(started) = flist_started {
            stages.flist_request_ns = started.elapsed().as_nanos().max(0) as u64;
        }
        stages.skip();
        // Working Metal traces publish no device-control record before their
        // render kicks.  Keep the RE-derived IdlePowerOff transaction strictly
        // diagnostic and obey the same mode contract as compute: mode 1 before
        // the kick, mode 2 after it, and the native/default mode 0 publishes
        // nothing.  This used to call the helper unconditionally, which made a
        // supposedly clean mode-0 render materially different from Metal.
        self.sync_g17p_idle_hold();
        if crate::g17_config::POWER_WIRE == 1 {
            if let Err(error) = self.wire_g17p_submit_gpu_power() {
                return Err(self.request_session_rebuild(error));
            }
        }
        // Status-A is a plain DRAM read of the host-owned primary state grid --
        // no sgx MMIO, so it is safe with the GPU cores still gated. `scan-active`
        // going 0 -> 1 across the doorbell is one of the three signals that
        // flipped when compute started executing.
        if let (Some(pdev), Some(manager)) = (self.pdev.as_ref(), self.manager.as_mut()) {
            log_g17p_status_a(pdev.as_ref(), manager, "render-pre-doorbell");
            // The render path never drained the KTrace ring, so an absent code
            // said nothing about the firmware -- only that nobody had looked.
            // Consume whatever is already there, so every record the post-wait
            // drain reports is attributable to THIS submission. Both helpers
            // are DRAM reads of host-allocated objects; unlike `gpc-state` they
            // carry no sgx/SError hazard on a path where the cores may never
            // have come up.
            probe_g17p_ktrace(pdev.as_ref(), manager, "render-pre-doorbell");
            probe_g17p_reports(pdev.as_ref(), manager, "render-pre-doorbell");
        }
        if let Some(manager) = self.manager.as_mut() {
            manager.log_render_status_pages(render_slot, "pre-doorbell");
            manager.log_render_sksm_entries(render_slot, "pre-doorbell");
            if native_doorbell_mode == 2 || native_doorbell_mode == 3 {
                manager.log_render_outer_channels("pre-fragment-doorbell");
            }
        }
        // Opt-in causal-window diagnostic: one read-only observation before
        // the first render launch for each numeric context in this module's
        // lifetime. Never select/clear a requestor, wake the GPU, or poll here.
        // A gated-off or failed read is unavailable evidence, not a clear latch.
        if cfg!(g17p_gupm_preflight_probe) && descriptor_context_id < 64 {
            static SEEN: AtomicU64 = AtomicU64::new(0);
            let bit = 1u64 << descriptor_context_id;
            if SEEN.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                if let (Some(pdev), Some(registers)) = (self.pdev.as_ref(), self.registers.as_ref()) {
                    match registers.g17p_render_fault_irq_snapshot() {
                        Ok(Some(snapshot)) => dev_info!(pdev.as_ref(),
                            "G17P first-context prelaunch fault: context={} gpc={:#x} info={:#018x} addr-word={:#018x} gate={:#x} sub-status={:#x} irq={:#x} (selected requestor only)\n",
                            descriptor_context_id, snapshot.gpc_state, snapshot.fault_info,
                            snapshot.fault_addr_word, snapshot.requestor_gate,
                            snapshot.sub_status, snapshot.irq_status),
                        Ok(None) => dev_info!(pdev.as_ref(),
                            "G17P first-context prelaunch fault: context={} unavailable (GPC off)\n",
                            descriptor_context_id),
                        Err(error) => dev_info!(pdev.as_ref(),
                            "G17P first-context prelaunch fault: context={} unavailable ({:?})\n",
                            descriptor_context_id, error),
                    }
                }
            }
        }
        if crate::g17_config::RENDER_KSM_PROBE != 0 {
            // Reacquire after the mutable power/control calls above so this
            // diagnostic borrow does not artificially span them.
            let registers = self.registers.as_ref().ok_or(ENODEV)?;
            match registers.g17p_render_fault_irq_snapshot() {
                Ok(Some(snapshot)) => dev_info!(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    "G17P fault IRQ @render-pre-fragment-doorbell: gpc-state={:#x} info={:#018x} addr-word={:#018x} gate={:#018x} sub-status={:#018x} irq-status={:#018x} bit35={}\n",
                    snapshot.gpc_state,
                    snapshot.fault_info,
                    snapshot.fault_addr_word,
                    snapshot.requestor_gate,
                    snapshot.sub_status,
                    snapshot.irq_status,
                    (snapshot.irq_status >> 35) & 1,
                ),
                Ok(None) => dev_warn!(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    "G17P fault IRQ @render-pre-fragment-doorbell: skipped because gpc-state=0\n"
                ),
                Err(error) => dev_warn!(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    "G17P fault IRQ @render-pre-fragment-doorbell: snapshot failed ({:?})\n",
                    error,
                ),
            }
            if let Err(error) = registers
                .g17p_log_fault_requestors_pre_ack("render-pre-fragment-doorbell")
            {
                dev_warn!(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    "G17P fault requestor @render-pre-fragment-doorbell: sweep failed ({:?})\n",
                    error,
                );
            }
        }
        // Modes 2/3 reproduce the priority-2 userspace chronology: only
        // the complete 3D group is visible for the first kick;
        // mode 3 additionally waits for Fragment outer consumption before TA
        // is constructed and made wholly visible for the second kick.
        // Mode 1 preserves the earlier diagnostic (both groups visible, then
        // both native doorbells), and mode 0 is byte-for-byte legacy behavior.
        //
        // The full 25G83 boot trace also contains earlier priority-0 system
        // queues. Those are a distinct initial-bring-up profile; this Vulkan
        // client queue matches the later native priority-2 class.
        // A dtrace probe over every function of the RTBuddy kext plus every
        // IOMappedWrite64 for one Metal render pass shows the COMPLETE host to
        // GPU surface:
        //
        //   ensurePoweredHardware(0)
        //   sendAsyncNoteToFirmware(0x84000000000011)   device-control announce
        //   3D: writeChannelCommandPointer x3, willSubmit(qid 8, dm 1)
        //   submit3DChannel -> ensurePoweredHardware(1) -> DM ring nextEntry
        //   TA: writeChannelCommandPointer x3, willSubmit(qid 7, dm 0)
        //   submitTAChannel -> ensurePoweredHardware(1) -> DM ring nextEntry
        //   sendAsyncNoteToFirmware(0x83000000000008)   <- the ONLY doorbell
        //
        // Zero MMIO writes, and exactly one doorbell -- Vertex, priority 2 --
        // rung once, after BOTH groups and BOTH data-master ring publishes.
        // Native never sends the Fragment doorbell 0x83..09 at all, so the one
        // this driver rang between the two groups has no counterpart in the
        // oracle. Publish the tiling group first, then ring the single
        // doorbell below.
        if crate::g17_config::SEED_WORK_CHANNEL != 0 {
            if let Some(registers) = self.registers.as_ref() {
                let value = u64::from(crate::g17_config::SEED_WORK_CHANNEL);
                match registers.g17p_seed_work_channel(value) {
                    Ok(()) => dev_info!(
                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                        "G17P render: DIAGNOSTIC seeded KSM work channel 0x21168 = {:#x}\n",
                        value,
                    ),
                    Err(error) => dev_warn!(
                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                        "G17P render: DIAGNOSTIC work-channel seed failed ({:?})\n",
                        error,
                    ),
                }
            }
        }
        // v93r: boot parameter `g17p_fw_trace_classes` (bit n = KTrace class
        // n) OR-ed into the firmware class bytes at 0x17780c/0x17780d, with
        // the line cleaned so the coprocessor observes it. Checked on the
        // first render and every 256th after (a recovery or rebuild may
        // re-initialize the firmware DATA); a missing bit is re-armed.
        let trace_classes = module_parameters::g17p_fw_trace_classes.value() & 0xffff;
        if trace_classes != 0 {
            static CHECKS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            if CHECKS.fetch_add(1, Ordering::Relaxed) % 256 == 0 {
                const CLASS_QWORD_LINK: u64 = 0xffff_fc00_0017_7808;
                let bits = u64::from(trace_classes) << 32;
                let phys = g17_manager::g17p_gfx_link_to_physical(CLASS_QWORD_LINK);
                match crate::pgtable::LiveFirmwareU64Probe::new(phys)
                    .and_then(|probe| probe.set_bits_visible(bits))
                {
                    Ok((before, after)) if before & bits != bits => dev_info!(
                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                        "G17P fw trace classes {:#06x} armed: class qword {:#018x} -> {:#018x}\n",
                        trace_classes, before, after,
                    ),
                    Ok(_) => {}
                    Err(error) => dev_warn!(
                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                        "G17P fw trace classes {:#06x}: arm failed ({:?})\n",
                        trace_classes, error,
                    ),
                }
            }
        }
        if crate::g17_config::FW_TRACE_CLASSES != 0 {
            // 0x17780c bits 1,2 and 0x17780d bit 1, inside the qword at
            // 0x177808: byte 4 bits 1,2 -> qword bits 33,34; byte 5 bit 1 ->
            // qword bit 41.
            const CLASS_QWORD_LINK: u64 = 0xffff_fc00_0017_7808;
            // Mode 1 arms the classes the firmware RE named (0x17780c bits
            // 1-2, 0x17780d bit 1). Mode 2 arms EVERY class in both bytes:
            // mode 1 produced ch=1 but no ch=9, so the one-bit-per-class
            // reading is unconfirmed and the cheapest way to find 0x909 is to
            // stop guessing the mapping.
            let class_bits: u64 = if crate::g17_config::FW_TRACE_CLASSES >= 2 {
                0x0000_ffff_0000_0000
            } else {
                (1u64 << 33) | (1u64 << 34) | (1u64 << 41)
            };
            const CLASS_BITS_UNUSED: u64 = 0;
            let phys = g17_manager::g17p_gfx_link_to_physical(CLASS_QWORD_LINK);
            match crate::pgtable::LiveFirmwareU64Probe::new(phys)
                .and_then(|p| p.set_bits(class_bits))
            {
                Ok((before, after)) => dev_info!(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    "G17P render: DIAGNOSTIC armed fw trace classes {:#018x} -> {:#018x}\n",
                    before,
                    after,
                ),
                Err(error) => dev_warn!(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    "G17P render: DIAGNOSTIC fw trace class arm failed ({:?})\n",
                    error,
                ),
            }
        }
        if crate::g17_config::KSM_COLD_ARM != 0 {
            if let Some(registers) = self.registers.as_ref() {
                match registers.g17p_ksm_cold_arm() {
                    Ok(()) => dev_info!(
                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                        "G17P render: DIAGNOSTIC host KSM cold arm written (0x21140 = 1<<63)\n"
                    ),
                    Err(error) => dev_warn!(
                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                        "G17P render: DIAGNOSTIC host KSM cold arm failed ({:?})\n",
                        error,
                    ),
                }
            }
        }
        if crate::g17_config::ADMIT_ARM != 0 {
            if let Some(registers) = self.registers.as_ref() {
                let both = crate::g17_config::ADMIT_ARM >= 2;
                let mut queues = KVec::new();
                queues.push(queue_pair.tiling as u8, GFP_KERNEL)?;
                if both {
                    queues.push(queue_pair.fragment as u8, GFP_KERNEL)?;
                }
                for qid in queues {
                    match registers.g17p_arm_queue_admit(qid) {
                        Ok((before, after)) => dev_info!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P render: DIAGNOSTIC admit-arm qid {} descriptor {:#018x} -> {:#018x}\n",
                            qid,
                            before,
                            after,
                        ),
                        Err(error) => dev_warn!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P render: DIAGNOSTIC admit-arm qid {} failed ({:?})\n",
                            qid,
                            error,
                        ),
                    }
                }
            }
        }
        if crate::g17_config::RENDER_SEED_TS != 0 {
            if let Some(registers) = self.registers.as_ref() {
                for qid in [
                    queue_pair.tiling as u8,
                    queue_pair.fragment as u8,
                ] {
                    let seed =
                        u64::from(crate::g17_config::RENDER_SEED_TS) - 1;
                    match registers.g17p_seed_queue_completed_timestamp(qid, seed) {
                        Ok(()) => dev_info!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P render: seeded qid {} completed-timestamp to {}\n",
                            qid,
                            seed,
                        ),
                        Err(error) => dev_warn!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P render: seeding qid {} failed ({:?})\n",
                            qid,
                            error,
                        ),
                    }
                }
            }
        }
        if (native_doorbell_mode == 2 || native_doorbell_mode == 3)
            && crate::g17_config::RENDER_SLOT_TEMPLATE != 0
        {
            if let Err(error) = Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
                .send_message_logged(
                    g17_rtkit::ENDPOINT_GFX_INTERRUPTS,
                    g17_submission::G17P_NATIVE_3D_DOORBELL,
                )
            {
                return Err(self.request_session_rebuild(error));
            }
        }
        if native_doorbell_mode == 2 || native_doorbell_mode == 3 {
            if native_doorbell_mode == 3 {
                if let Err(error) = self
                    .manager
                    .as_mut()
                    .ok_or(ENODEV)?
                    .wait_native_render_fragment_outer_consumed(
                        prepared.fragment.next_producer,
                    )
                {
                    return Err(self.request_session_rebuild(error));
                }

                // This is the only interval in the native chronology where
                // 3D has been kicked but TA is still completely unpublished.
                // Sample every read-only fault bank here so a single run can
                // distinguish a fragment-created dIPP condition from one
                // introduced by the later TA ConfigUpdate/AddKicks group.
                if crate::g17_config::RENDER_KSM_PROBE != 0 {
                    let registers = self.registers.as_ref().ok_or(ENODEV)?;
                    match registers.g17p_render_fault_irq_snapshot() {
                        Ok(Some(snapshot)) => dev_info!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P fault IRQ @render-post-fragment-pre-ta: gpc-state={:#x} info={:#018x} addr-word={:#018x} gate={:#018x} sub-status={:#018x} irq-status={:#018x} bit35={}\n",
                            snapshot.gpc_state,
                            snapshot.fault_info,
                            snapshot.fault_addr_word,
                            snapshot.requestor_gate,
                            snapshot.sub_status,
                            snapshot.irq_status,
                            (snapshot.irq_status >> 35) & 1,
                        ),
                        Ok(None) => dev_warn!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P fault IRQ @render-post-fragment-pre-ta: skipped because gpc-state=0\n"
                        ),
                        Err(error) => dev_warn!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P fault IRQ @render-post-fragment-pre-ta: snapshot failed ({:?})\n",
                            error,
                        ),
                    }
                    if let Err(error) = registers
                        .g17p_log_fault_requestors_pre_ack("render-post-fragment-pre-ta")
                    {
                        dev_warn!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P fault requestor @render-post-fragment-pre-ta: sweep failed ({:?})\n",
                            error,
                        );
                    }
                }
            }
            let registers = self.registers.as_ref().ok_or(ENODEV)?;
            if let Err(error) = self
                .manager
                .as_mut()
                .ok_or(ENODEV)?
                .publish_native_render_tiling_group(render_slot, &drm, registers, &prepared.tiling)
            {
                return Err(self.request_session_rebuild(error));
            }

            if let Err(error) = Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
                .send_message_logged(
                    g17_rtkit::ENDPOINT_GFX_INTERRUPTS,
                    g17_submission::G17P_NATIVE_TA_DOORBELL,
                )
            {
                return Err(self.request_session_rebuild(error));
            }
            if crate::g17_config::IRQ_BURST != 0 {
                if let Some(registers) = self.registers.as_ref() {
                    let mut seen = [(0u64, 0u64, 0u64); 12];
                    match registers.g17p_burst_sample_irq(&mut seen) {
                        Ok(n) => dev_info!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P render IRQBURST distinct={} [{:#x}/{:#x}/{:#x}] [{:#x}/{:#x}/{:#x}] [{:#x}/{:#x}/{:#x}] [{:#x}/{:#x}/{:#x}]\n",
                            n,
                            seen[0].0, seen[0].1, seen[0].2,
                            seen[1].0, seen[1].1, seen[1].2,
                            seen[2].0, seen[2].1, seen[2].2,
                            seen[3].0, seen[3].1, seen[3].2,
                        ),
                        Err(error) => dev_warn!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P render IRQBURST failed ({:?})\n",
                            error,
                        ),
                    }
                }
            }
            if crate::g17_config::SLOT_BURST != 0 {
                if let Some(registers) = self.registers.as_ref() {
                    let mut seen = [0u64; 16];
                    match registers.g17p_burst_sample_slot0(&mut seen) {
                        Ok(n) => dev_info!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P render SLOTBURST distinct={} [{:#018x},{:#018x},{:#018x},{:#018x},{:#018x},{:#018x},{:#018x},{:#018x}]\n",
                            n,
                            seen[0], seen[1], seen[2], seen[3],
                            seen[4], seen[5], seen[6], seen[7],
                        ),
                        Err(error) => dev_warn!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P render SLOTBURST failed ({:?})\n",
                            error,
                        ),
                    }
                }
            }
            // Same words as the macOS fwconsume.d oracle watches (the DM ring
            // {read, consumed, write} triple per stage). Native bumps the write
            // index for BOTH stages before its single doorbell and the firmware
            // then advances read+consumed on both. The pre-fragment-doorbell
            // snapshot is taken before the late tiling publication, so it cannot
            // say whether the TA producer is ever bumped at all -- this one can.
            if crate::g17_config::DEP_RECORDS != 0 {
                if let Some(manager) = self.manager.as_mut() {
                    manager.log_render_outer_channels("post-doorbells");
                }
            }
            // Work-scan entry gate. FW RE of b1: FUN_261a4 is the pass that
            // consumes DM work-ring entries, and it advances the read index
            // and sets coherency=write on ALL TWELVE rings on every pass,
            // whatever they contain. So render tables 6/7 sitting at
            // {read 0, coherency 0, producer 1} after the doorbell does NOT
            // mean the scan looked at our entry and rejected it -- a rejected
            // or type-mismatched entry is skipped WITH the read index
            // advanced. It means the scan did not run at all.
            //
            // Its entry gate at 0x263b8..0x263dc is
            //     (DAT_1771f8 & 0x18) == 0
            //  && ((DAT_1771f8 & 0x60) == 0 || DAT_124ba4 == 1)
            // so read both. If 0x18 or 0x60 is set here, the firmware is
            // skipping the work scan and that is the whole bug.
            if crate::g17_config::SCAN_GATE != 0 {
                let polls = 64usize;
                for (label, link, mask) in [
                    ("run-state 0x1771f8", 0xffff_fc00_0017_71f8u64, 0x78u64),
                    ("DAT_124ba0", 0xffff_fc00_0012_4ba0u64, 0xffff_ffff_0000_0000u64),
                    // The scan's OTHER precondition: wakeup bits 0/1/2 in
                    // DAT_177190, posted by the 0x83 doorbell handler. The
                    // run-state gate above measured OPEN while our cursors
                    // stayed [0,0,1], and compute's ring reads [1,1,1] -- one
                    // scan pass advances EVERY ring, so the scan ran for
                    // compute's doorbell and never again for the render's.
                    // That makes the wakeup the remaining suspect.
                    ("wakeup 0x177190", 0xffff_fc00_0017_7190u64, 0x7u64),
                ] {
                    let phys = g17_manager::g17p_gfx_link_to_physical(link);
                    match crate::pgtable::LiveFirmwareU64Probe::new(phys)
                        .and_then(|p| p.observe_mask(mask, polls))
                    {
                        Ok(o) => dev_info!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P scan-gate[{}]: first={:#018x} last={:#018x} or={:#018x} mask={:#x} seen={} polls={}\n",
                            label,
                            o.first,
                            o.last,
                            o.observed_or,
                            mask,
                            o.mask_seen,
                            o.polls,
                        ),
                        Err(error) => dev_warn!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P scan-gate[{}]: read failed ({:?})\n",
                            label,
                            error,
                        ),
                    }
                }
            }
            // Configurable GPU register-window dump for the pre-firmware
            // hardware-state audit. Software is exhausted -- submit protocol,
            // initdata and the kext's one-time bring-up are all byte-exact or
            // equivalent to native -- so the remaining variable is the register
            // state m1n1 and this driver leave the GPU in, which initdata does
            // not describe. Dumped here so it can be diffed against a macOS
            // read of the same physical addresses.
            //
            // Placed at this checkpoint deliberately: it is the one window
            // where sgx reads are known safe, because the cores are powered
            // after the doorbell. An sgx read with the cores gated raises an
            // asynchronous SError and panics the machine, which is why this is
            // opt-in and 0x100-granular rather than an arbitrary address.
            let dump = crate::g17_config::REG_DUMP;
            if dump != 0 {
                if let Some(registers) = self.registers.as_ref() {
                    let base = (dump as usize) << 8;
                    let mut words = [0u32; 64];
                    match registers.g17p_dump_sgx_window(base, &mut words) {
                        Ok(()) => {
                            for row in 0..16usize {
                                dev_info!(
                                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                                    "G17P regdump {:#08x}: {:08x} {:08x} {:08x} {:08x}\n",
                                    base + row * 16,
                                    words[row * 4],
                                    words[row * 4 + 1],
                                    words[row * 4 + 2],
                                    words[row * 4 + 3],
                                );
                            }
                        }
                        Err(error) => dev_warn!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P regdump {:#08x}: read failed ({:?})\n",
                            base,
                            error,
                        ),
                    }
                }
            }
            self.probe_render_ksm_admission_after_render_doorbells(Some(render_slot));
            // LATE sample. The post-doorbells checkpoint above is far too early
            // to conclude anything: native's own DM ring cursors do not move
            // until ~420us after its doorbell (fwconsume-gfx.txt, doorbell at
            // 22881us, DMTA2R/DM3D2R -> 0x35 at 23302us), and the KSM admission
            // probe just above waits up to 40ms. Sample here instead.
            //
            // Also read the recovery stash. FW RE of b1: while DAT_1771e8 != 0
            // a recovery is open and every scheduler event except 0x2000000 is
            // OR-ed into DAT_1771e0 rather than dispatched. Both the normal
            // work scan (FUN_261a4) and the recovery replay (FUN_eb7c) advance
            // read and coherency, so the cursors alone cannot say WHICH path
            // consumed our rings -- these two words can.
            if crate::g17_config::SCAN_GATE != 0 {
                if let Some(manager) = self.manager.as_mut() {
                    manager.log_render_outer_channels("post-wait");
                }
                for (label, link) in [
                    ("recovery-open 0x1771e8", 0xffff_fc00_0017_71e8u64),
                    ("event-stash 0x1771e0", 0xffff_fc00_0017_71e0u64),
                ] {
                    let phys = g17_manager::g17p_gfx_link_to_physical(link);
                    match crate::pgtable::LiveFirmwareU64Probe::new(phys)
                        .and_then(|p| p.observe_mask(!0u64, 2))
                    {
                        Ok(o) => dev_info!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P scan-late[{}]: {:#018x}\n",
                            label,
                            o.last,
                        ),
                        Err(error) => dev_warn!(
                            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                            "G17P scan-late[{}]: read failed ({:?})\n",
                            label,
                            error,
                        ),
                    }
                }
            }
        } else {
            if native_doorbells {
                for doorbell in [
                    g17_submission::G17P_NATIVE_3D_DOORBELL,
                    g17_submission::G17P_NATIVE_TA_DOORBELL,
                ] {
                    if let Err(error) = Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
                        .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, doorbell)
                    {
                        return Err(self.request_session_rebuild(error));
                    }
                }
            } else {
                // Keep the producer store(s) and the initial work doorbell
                // adjacent. Default publishes the complete pair; split launch
                // publishes only TA and retains Fragment until TA completion.
                let publish = match self.manager.as_mut() {
                    Some(manager) if split_launch => manager
                        .publish_split_render_tiling_after_freelist(render_slot, &prepared.tiling),
                    Some(manager) => {
                        manager.publish_default_render_pair_after_freelist(render_slot, &prepared)
                    }
                    None => Err(ENODEV),
                };
                if let Err(error) = publish {
                    return Err(self.request_session_rebuild(error));
                }
                fence(Ordering::SeqCst);
                let send = match self.gfx_rtkit.as_mut() {
                    Some(rtkit) => Pin::new(rtkit).send_message_logged(
                        g17_rtkit::ENDPOINT_GFX_INTERRUPTS,
                        prepared.paired_doorbell,
                    ),
                    None => Err(ENODEV),
                };
                if let Err(error) = send {
                    return Err(self.request_session_rebuild(error));
                }
                self.arm_render_stall_watch();
                if stages.enabled {
                    stages.doorbell_cntvct = crate::g17_timing_buffer::arch_counter();
                }
                stages.lap(crate::g17_perf::SUB_DOORBELL);
                crate::g17_e2e::power_sample(1, render_slot);
                if let Some(pdev) = self.pdev.as_ref().filter(|_| crate::util::g17p_verbose()) {
                    dev_info!(
                        pdev.as_ref(),
                        "G17P render: deferred {} outer publication completed after USC FList retirement and initial doorbell sent\n",
                        if split_launch { "TA-only" } else { "paired" },
                    );
                }
            }
            // The default path uses the valid cold-native paired priority-0
            // doorbell. Keep the read-only admission/IRQ probe available on
            // that path too: gating it on the obsolete split-doorbell
            // diagnostic hid the decisive 0x10a10 bit-4 / 0x10a60 new-work
            // predicate during otherwise native-shaped submissions.
            self.probe_render_ksm_admission_after_render_doorbells(Some(render_slot));
        }
        if !split_launch {
            self.manager.as_mut().ok_or(ENODEV)?.commit_render_publication(render_slot)?;
        }
        if crate::g17_config::POWER_WIRE >= 2 {
            if let Err(error) = self.wire_g17p_submit_gpu_power() {
                return Err(self.request_session_rebuild(error));
            }
        }
        // gpc-state probe -- OPT-IN, and deliberately confined to the window
        // immediately after the doorbell.
        //
        // `log_gate` reads the sgx register file (regs.rs
        // `g17p_scheduler_gate_registers` -> `self.sgx.try_access()`). On this
        // chip an sgx read taken with the GPU cores power-gated raises an
        // asynchronous SError and panics the machine, which rules out the two
        // points a probe would otherwise naturally go:
        //
        //   * before publish -- `render-pre-doorbell` is DRAM-only precisely
        //     because the cores may still be gated there;
        //   * at the wait timeout -- by then the cores have powered back down.
        //     `abandon_compute_submission` records that doing this panicked the
        //     box on every timeout it was tried on.
        //
        // The compute path's one sample is safe only because it runs after a
        // completion woke the caller, so the GPU is provably alive. Render never
        // reaches that moment, so the closest safe approximation is a dense
        // burst right after the kick, while the firmware is still handling it.
        // Compute's signature is a transient 0x0 -> 0x1 -> 0x0, which a single
        // read would very likely miss -- hence a burst rather than one sample.
        let gate_probe = crate::g17_config::RENDER_GATE_PROBE;
        if gate_probe != 0 && split_launch {
            if let Some(pdev) = self.pdev.as_ref() {
                dev_warn!(
                    pdev.as_ref(),
                    "G17P render split-launch: suppressing the SGX gate probe so the TA timestamp gate stays below the recovery window\n"
                );
            }
        } else if gate_probe != 0 {
            let samples = if gate_probe > 1 { 40 } else { 12 };
            for _ in 0..samples {
                self.log_gate("render-post-doorbell");
                fsleep(Delta::from_millis(1));
            }
        }
        // Do not scan the multi-MiB TVB/RT allocations while the render is
        // running. This call owns the runtime mutex, so that scan also delays
        // the firmware-event worker; observed scans exceeded the firmware's
        // watchdog interval before the first inline event service. Preserve
        // full memory dumps at recovery/failure or after completion instead.
        if split_launch {
            let tiling_timestamps = match self.wait_split_render_tiling_completion(render_slot) {
                Ok(timestamps) => timestamps,
                Err(error) => {
                    return Err(self.request_session_rebuild(error))
                }
            };
            if let Some(pdev) = self.pdev.as_ref() {
                dev_info!(
                    pdev.as_ref(),
                    "G17P render split-launch: TA completed [{:#x},{:#x}]; publishing deferred 3D outer producer\n",
                    tiling_timestamps[0],
                    tiling_timestamps[1]
                );
            }
            let registers = self.registers.as_ref().ok_or(ENODEV)?;
            if let Err(error) = self
                .manager
                .as_mut()
                .ok_or(ENODEV)?
                .publish_split_render_fragment_group(render_slot, &drm, registers, &prepared.fragment)
            {
                // See the completion-wait path: a post-publication render
                // failure must not reboot the firmware CPUs. That reboot times
                // out and the failure path leaves the session refusing every
                // later submission.
                self.abandon_render_submission_after_timeout();
                return Err(error);
            }
            if let Err(error) = Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
                .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, prepared.paired_doorbell)
            {
                // See the completion-wait path: a post-publication render
                // failure must not reboot the firmware CPUs. That reboot times
                // out and the failure path leaves the session refusing every
                // later submission.
                self.abandon_render_submission_after_timeout();
                return Err(error);
            }
            self.arm_render_stall_watch();
            self.manager.as_mut().ok_or(ENODEV)?.commit_render_publication(render_slot)?;
            // As for paired publication, enter the servicing wait before any
            // expensive host-memory dump of the now-live fragment resources.
        }
        if defer_completion {
            stages.lap(crate::g17_perf::SUB_COMMIT);
            return Ok(());
        }
        let completion = match self.wait_render_completion_servicing_events(render_slot) {
            Ok(completion) => completion,
            Err(error) => {
                if let (Some(pdev), Some(manager)) = (self.pdev.as_ref(), self.manager.as_mut()) {
                    log_g17p_status_a(pdev.as_ref(), manager, "render-wait-failed");
                    // 0x40 tag15-programmed, 0x41 tag14-addkicks, 0x46
                    // pause-reason: the three codes that diagnosed compute.
                    probe_g17p_ktrace(pdev.as_ref(), manager, "render-wait-failed");
                    probe_g17p_reports(pdev.as_ref(), manager, "render-wait-failed");

                    dump_g17p_firmware_log(pdev.as_ref(), manager, "render-wait-failed");
                }
                if let Some(manager) = self.manager.as_mut() {
                    manager.log_render_status_pages(render_slot, "wait-failed");
                    manager.log_render_sksm_entries(render_slot, "wait-failed");
                    // Render completes into ordinal 2, which nothing had ever
                    // scanned. DRAM only, so safe on the timeout path.
                    let _ = manager.dump_completion_records("render-wait-failed");
                }
                // A render that does not complete is NOT a reason to tear the
                // runtime down. `recover_persistent_runtime` stops and reboots
                // both firmware CPUs; on this part that reboot regularly times
                // out in apple_rtkit_boot (~1s), and the failure path then
                // leaves `accepting_submissions` false forever, so every later
                // submission -- render OR compute, from any client -- returns
                // ENOTSUPP. One unfinished render bricked the device for the
                // rest of the boot.
                //
                // The firmware is demonstrably alive here: it has already
                // raised and serviced its own stamp-blamed recovery, answers
                // KSM register reads and keeps draining its rings. So release
                // this submission's own state and stay open for business. A
                // genuine crash still reaches the full path through
                // `is_crashed()` on the next submit.
                self.abandon_render_submission_after_timeout();
                return Err(error);
            }
        };
        if let (Some(pdev), Some(manager)) = (self.pdev.as_ref(), self.manager.as_mut()) {
            log_g17p_status_a(pdev.as_ref(), manager, "render-post-wait");
            probe_g17p_ktrace(pdev.as_ref(), manager, "render-post-wait");
            dump_g17p_firmware_log(pdev.as_ref(), manager, "render-post-wait");
            manager.log_render_status_pages(render_slot, "post-completion");
        }
        if !completion.complete {
            return Err(self.request_session_rebuild(EIO));
        }
        Ok(())
    }

    /// Publish one translated compute command and wait for its execution
    /// timestamps. The direct first-bind path retains the mapping and VM bind
    /// beyond this return until correlated ordinal-0 completion or stopped-CPU
    /// recovery, so timestamp visibility alone cannot release firmware state.
    /// Drive any open firmware recovery to completion before publishing.
    ///
    /// Returns once the shared handshake state reads 0. Answering every
    /// recovery is mandatory -- a capped or skipped answer leaves the KSM
    /// front end halted forever, because the host write of 2 is what makes the
    /// firmware schedule the unhalt.
    /// v87 SETTLE_ONCE: the open-recovery gate for the publish reservation.
    /// `settled` belongs to one uninterrupted Runtime hold of the caller and
    /// is true once this gate passed during that hold; the caller clears it
    /// whenever it reacquires Runtime. Only the reservation helpers use it;
    /// the gate before the doorbell (submit_translated_render) always runs.
    fn settle_open_recovery_once(&mut self, settled: &mut bool) -> Result {
        if *settled && crate::g17_host_path::enabled(crate::g17_host_path::SETTLE_ONCE) {
            crate::g17_host_path::count(&crate::g17_host_path::SETTLE_SKIPPED);
            return Ok(());
        }
        self.settle_open_recovery_before_publication()?;
        *settled = true;
        Ok(())
    }

    fn settle_open_recovery_before_publication(&mut self) -> Result {
        const SETTLE_POLLS: usize = 200;
        for attempt in 0..SETTLE_POLLS {
            let manager = self.manager.as_mut().ok_or(ENODEV)?;
            // v87 STATE_WORD: this gate tests the state word only.
            let state = if crate::g17_host_path::enabled(crate::g17_host_path::STATE_WORD) {
                manager.firmware_recovery_state()?
            } else {
                manager.firmware_recovery_handshake_snapshot()?.state
            };
            if state == 0 {
                if attempt != 0 {
                    dev_info!(
                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                        "G17P submit: recovery settled after {} polls\n",
                        attempt
                    );
                }
                return Ok(());
            }
            let _ = self.service_firmware_events_inline();
            // A requested session rebuild is performed by the recovery worker
            // at its own top level, never inside a publication (kernel stack;
            // `service_primary_recovery_nested`). Nothing can be published on
            // this session before it: refuse now rather than poll out.
            if self.session_rebuild_requested {
                return Err(EAGAIN);
            }
            if !self.preparations.is_drained() && self.primary_recovery_pending() {
                return Err(EAGAIN);
            }
            fsleep(Delta::from_millis(1));
        }
        dev_err!(
            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
            "G17P submit: firmware recovery still open after {} polls; refusing to publish\n",
            SETTLE_POLLS
        );
        Err(EBUSY)
    }

    /// Establish the power prerequisites for direct registration of a fresh
    /// non-bootstrap render pair.
    ///
    /// Native `ensurePoweredHardware(true)` first ensures that both retained
    /// RTBuddy roles are at target 1. In this driver, ownership of the started
    /// CPU pair, both live RTKit transports and both initdata ACKs is the same
    /// retained-state witness. Usually that is all: if the GPC/KSMFE aperture
    /// is already powered, no message is sent, matching the captured native
    /// steady-state path.
    ///
    /// The GPU cores may nevertheless have independently entered firmware
    /// idle-off while the two processors remain alive. Linux has no IOGPU PM
    /// callback to hold that gate across queue construction, so publish the
    /// firmware-proven IdlePowerOff(0) request and keep that zero policy in
    /// force through the direct pair transaction. A cold wake can refresh the
    /// policy from Region-C, whose native initial value enables idle descent,
    /// so a second retired zero is required after the powered witness. This is
    /// the closest firmware-level equivalent of native's retained power
    /// provider. The records run before render storage, QoS, tag-15, either
    /// outer command, or a completion fence is staged.
    fn ensure_g17p_render_registration_power(
        &mut self,
        manager: &mut g17_manager::G17PManagerConstruction,
        registers: &regs::Resources,
    ) -> Result {
        let pdev = self.pdev.as_ref().cloned().ok_or(ENODEV)?;
        let providers_retained = self.cpus.is_some()
            && self.gfx_rtkit.is_some()
            && self.gfx1_rtkit.is_some()
            && self.gfx_state.as_ref().is_some_and(|state| {
                state.acked.load(Ordering::Acquire)
                    && !state.crashed.load(Ordering::Acquire)
            })
            && self.gfx1_state.as_ref().is_some_and(|state| {
                state.acked.load(Ordering::Acquire)
                    && !state.crashed.load(Ordering::Acquire)
            });
        if !providers_retained || !self.accepting_submissions {
            dev_err!(
                pdev.as_ref(),
                "G17P render registration: retained GFX/GFX1 provider or RTKit prerequisite missing; no pair publication attempted\n",
            );
            return Err(EIO);
        }
        let acquire = (|| -> Result {
            for attempt in 0..3 {
                let was_powered = registers.g17p_sksm_configuration_port_powered();
                let wake = self.publish_g17p_render_registration_power_record(
                    manager,
                    g17_initdata::DEVICE_CONTROL_IDLE_POWER_OFF_ARG_DISABLE_IDLE_AND_WAKE,
                )?;
                self.wait_g17p_render_registration_power_record(
                    manager,
                    registers,
                    wake.producer_after,
                    true,
                    "disable-idle-and-wake",
                )?;

                // A cold wake runs the configuration refresh path, which may
                // reload Region-C +0x30 after the opcode handler stored zero.
                // Reassert zero only after the powered witness; on an already
                // powered steady-state path the first record is sufficient.
                let hold_target = if !was_powered {
                    let hold = self.publish_g17p_render_registration_power_record(
                        manager,
                        g17_initdata::DEVICE_CONTROL_IDLE_POWER_OFF_ARG_DISABLE_IDLE_AND_WAKE,
                    )?;
                    self.wait_g17p_render_registration_power_record(
                        manager,
                        registers,
                        hold.producer_after,
                        true,
                        "disable-idle-hold",
                    )?;
                    hold.producer_after
                } else {
                    wake.producer_after
                };
                if registers.g17p_sksm_configuration_port_powered() {
                    dev_info!(
                        pdev.as_ref(),
                        "G17P render registration: idle descent disabled through fresh pair construction (control target {}, attempt {})\n",
                        hold_target,
                        attempt + 1,
                    );
                    return Ok(());
                }
            }
            dev_err!(
                pdev.as_ref(),
                "G17P render registration: core did not remain powered with idle descent disabled; no pair publication attempted\n",
            );
            Err(EAGAIN)
        })();
        if acquire.is_err() {
            // Balance a partial acquisition. Preserve the original error: the
            // release is best-effort because the control path itself may be
            // what failed.
            let _ = self.release_g17p_render_registration_power(manager, registers);
        }
        acquire
    }

    /// Opt-in fault diagnostic, after the original DRAM evidence is captured.
    /// Waking/restoring the GPU can change its scoreboard. These are explicitly
    /// POST-WAKE observations, never a claim about the unmodified timeout.
    fn log_timeout_scoreboard_after_power_hold(&mut self, qids: u128) {
        if module_parameters::g17p_fault_report.value() & 8 == 0 { return; }
        let Some(pdev) = self.pdev.as_ref().cloned() else { return; };
        let Some(mut manager) = self.manager.take() else { return; };
        let Some(registers) = self.registers.take() else {
            self.manager = Some(manager);
            return;
        };
        // Never queue a power request behind a halted firmware recovery.
        let running = manager.firmware_recovery_handshake_snapshot()
            .is_ok_and(|state| state.state == 0);
        if running {
            manager.log_timeout_halt_masks("pre-hold");
            let was_powered = registers.g17p_sksm_configuration_port_powered();
            match self.ensure_g17p_render_registration_power(&mut manager, &registers) {
                Ok(()) => {
                    dev_err!(pdev.as_ref(),
                        "G17P timeout scoreboard POST-WAKE: previously-powered={} qids={:#x} idle-hold-acquired=true\n",
                        was_powered, qids);
                    manager.log_timeout_halt_masks("post-hold");
                    // Native EnabledQueuesHalt waits for these status bits
                    // after writing 210a8/210b0. Do not infer halted state
                    // from the undocumented top bits of per-QID 21010.
                    let stopped = [0x21058, 0x21060]
                        .map(|reg| registers.g17p_sksm_port_read(reg, 0));
                    dev_err!(pdev.as_ref(),
                        "G17P timeout scoreboard POST-WAKE: halted-status(21058,21060)={:x?}\n",
                        stopped);
                    for qid in 0..128u64 {
                        if qids & (1u128 << qid) == 0 { continue; }
                        let values = [0x21008, 0x21010, 0x21020, 0x21028]
                            .map(|reg| registers.g17p_sksm_port_read(reg, qid));
                        dev_err!(pdev.as_ref(),
                            "G17P timeout scoreboard POST-WAKE: qid={} regs(21008,21010,21020,21028)={:x?}\n",
                            qid, values);
                    }
                    // Release even when an individual register read failed.
                    let release = self.release_g17p_render_registration_power(
                        &mut manager, &registers);
                    dev_err!(pdev.as_ref(),
                        "G17P timeout scoreboard POST-WAKE: idle-release={:?}\n", release);
                }
                Err(error) => dev_err!(pdev.as_ref(),
                    "G17P timeout scoreboard skipped: power hold failed {:?}\n", error),
            }
        } else {
            dev_err!(pdev.as_ref(),
                "G17P timeout scoreboard skipped: recovery open or unreadable\n");
        }
        self.registers = Some(registers);
        self.manager = Some(manager);
    }

    fn publish_g17p_render_registration_power_record(
        &mut self,
        manager: &mut g17_manager::G17PManagerConstruction,
        arg: u32,
    ) -> Result<g17_resources::G17PDeviceControlPublication> {
        let publication = manager.publish_primary_device_control_record(
                g17_initdata::DEVICE_CONTROL_OPCODE_IDLE_POWER_OFF,
                arg,
            )?;
        fence(Ordering::SeqCst);
        Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
            .send_message_logged(
                g17_rtkit::ENDPOINT_GFX_INTERRUPTS,
                g17_rtkit::encode_primary_device_control(),
            )?;
        Ok(publication)
    }

    fn wait_g17p_render_registration_power_record(
        &mut self,
        manager: &mut g17_manager::G17PManagerConstruction,
        registers: &regs::Resources,
        target: u32,
        require_powered: bool,
        label: &'static str,
    ) -> Result {
        let pdev = self.pdev.as_ref().cloned().ok_or(ENODEV)?;

        // Two independent events, so two independent budgets. The record must
        // retire within G17P_POWER_WIRE_POLLS; only once it has does the power
        // sequencer get its own, longer window. Charging the sequencer's
        // latency against the retirement budget failed wakes whose record had
        // already retired.
        let mut retired_at: Option<usize> = None;
        for elapsed_ms in 0..(G17P_POWER_WIRE_POLLS + G17P_POWER_WIRE_POWERED_POLLS) {
            if self.is_crashed() {
                return Err(EIO);
            }
            if retired_at.is_none() {
                // Keep channel 12 single-producer until this exact target
                // retires. Firmware-event servicing can publish an FList
                // release record; doing that here would advance the same
                // producer past `target` and destroy the exact retirement
                // witness.
                if manager.control_counters()?.primary == [target; 3] {
                    retired_at = Some(elapsed_ms);
                } else if elapsed_ms + 1 >= G17P_POWER_WIRE_POLLS {
                    break;
                }
            }
            if let Some(retired_ms) = retired_at {
                let powered = registers.g17p_sksm_configuration_port_powered();
                if !require_powered || powered {
                    dev_info!(
                        pdev.as_ref(),
                        "G17P render registration: {} retired after {}ms, powered after {}ms (control target {}, powered={})\n",
                        label,
                        retired_ms,
                        elapsed_ms,
                        target,
                        powered,
                    );
                    return Ok(());
                }
            }
            fsleep(Delta::from_millis(1));
        }
        let counters = manager.control_counters()?;
        let powered = registers.g17p_sksm_configuration_port_powered();
        dev_err!(
            pdev.as_ref(),
            "G17P render registration: {} timed out (target={} counters={:?} retired={:?} powered={}); no pair publication attempted\n",
            label,
            target,
            counters.primary,
            retired_at,
            powered,
        );
        Err(ETIMEDOUT)
    }

    fn release_g17p_render_registration_power(
        &mut self,
        manager: &mut g17_manager::G17PManagerConstruction,
        registers: &regs::Resources,
    ) -> Result {
        // The idle-hold diagnostic owns the policy while it is in force.
        if self.idle_hold_published {
            return Ok(());
        }
        let release = self.publish_g17p_render_registration_power_record(
            manager,
            g17_initdata::DEVICE_CONTROL_IDLE_POWER_OFF_ARG_ENABLE_IDLE,
        )?;
        // Arg one restores the ordinary firmware idle policy. Do not make an
        // already prepared submission depend on the aperture remaining
        // powered after its direct registration completed.
        self.wait_g17p_render_registration_power_record(
            manager,
            registers,
            release.producer_after,
            false,
            "idle-release",
        )
    }

    /// Publish the host lever that releases KSM pause reason bit 1, then
    /// announce it, then sample the result.
    ///
    /// ## The G17P KSM pause-reason mask (`0x128a68`) -- FW RE of b1, 2026-08-31
    ///
    /// Read this before attempting to re-power the GPU cores from the host.
    /// It was expensive to derive and it retires several plausible-looking
    /// approaches. Method: enumerate every writer of `0x128a68` in
    /// `/tmp/g17p-re/fwb1/b1.disasm` (22 of them) and resolve each bit.
    ///
    /// | bit    | set at             | cleared at          | meaning              |
    /// |--------|--------------------|---------------------|----------------------|
    /// | `0x2`  | `0xe534`           | `0xe488`            | GPU core power       |
    /// | `0x4`  | `0x23e10`,`0x25ef8`| `0x25f88`,`0x25dd8` |                      |
    /// | `0x8`  | `0x259bc`          | `0x25dd8` ONLY      | scheduler idle       |
    /// | `0x10` | `0x12cc8`,`0x1671c`| `0x132cc`           |                      |
    /// | `0x40` | `0xec3c`,`0xfb08`  | `0xf8d8`            |                      |
    /// | `0x80` | `0x118c0`          | `0x11948`           | tag-16 handler fault |
    /// | `0x100`| `0x24380`,`0xa1c4` | `0xa1c4`            | generic helper       |
    /// | `0x400`| --                 | `0xa1c4`            | generic helper       |
    /// | `0x800`| `0x20dc4`          |                     |                      |
    /// | `0x1000`| `0x5bfc`          | `0x5b14`,`0x6594`   |                      |
    /// | `0x4000`| `0x163fc`         |                     |                      |
    ///
    /// ### THE RULE: the core power sequencer runs only on the mask reaching 0
    ///
    /// Every clear site has the identical shape --
    /// `ands <new>, <mask>, #~bit` / `str` / `b.ne <skip>` -- so `0x20a70`,
    /// the core power sequencer, is invoked ONLY on the transition to
    /// `mask == 0`. Clearing one bit of a multi-bit mask therefore cannot
    /// re-power the cores, by construction and regardless of which bit or when.
    ///
    /// This single fact explains three separate measured null results, which
    /// were each mistaken for a broken lever at the time:
    ///  * `g17p_power_wire=1` (preamble IdlePowerOff(0)) -- no effect;
    ///  * the old `g17p_power_wire=1 g17p_power_hold=1` implementation
    ///    (`IdlePowerOff(0)` followed by `IdlePowerOff(1)`) -- no effect;
    ///  * `g17p_power_wire=2` (the record deferred until after the kick) -- no
    ///    effect.
    /// None of them failed. With the post-fault mask of `0xa` (bits 3|1) all
    /// three clear bit 1 correctly and leave `0x8`, so the power-on is skipped
    /// exactly as written.
    ///
    /// ### Why a GPU fault costs a reboot (known limitation, not an open bug)
    ///
    ///  * Bit 3 has exactly ONE clear site image-wide, `0x25dd8`, inside the
    ///    scheduler thread's idle descent -- reachable only from `0x25c00` and
    ///    the `cbnz -> 0x25dc0` routes at `0x25a04`/`0x25a50`/`0x25a70`.
    ///  * The generic pause helper `0xa1c4(set: w0, bit: w1)` has five callers
    ///    image-wide and NONE of them passes bit `0x8`: `0xc98c`, `0x1be10`,
    ///    `0x1be40` pass `0x400`; `0x24544`, `0x2458c` pass `0x100`. So no host
    ///    message, doorbell or device-control opcode can clear bit 3 directly.
    ///  * The descent's guards at `0x25940` ARE queue state the driver drives
    ///    (queue producer `[x8+416]` == consumer `[x8+432]`, none of mask
    ///    `0x8_032c_0007` set in `[x19]`, `[x19+96]` zero), so the indirect
    ///    route -- go quiet, let the thread idle, let its EXIT clear bit 3 --
    ///    is at least conceivable. TESTED 2026-08-31 with
    ///    `g17p_gate_quiesce_ms=500 g17p_power_wire=2`: no clear-bit-8 KTrace
    ///    record ever appears, so the thread cannot be made to idle from this
    ///    state by going quiet either.
    ///  * The scheduler is a single main loop entered once from firmware entry
    ///    (`0x27a7c`, which has NO callers -> `0x21f54` -> `0x22198` ->
    ///    `0x25644`), not a per-tick handler. After a fault it stays pinned to
    ///    the work path at `0x25e2c` -- alive, and still retiring our kicks,
    ///    which is why zero-duration retires keep arriving. Our own continuous
    ///    submission is what prevents the only bit-3 clear from ever running.
    ///
    /// CONCLUSION: once bit 3 is set and the scheduler leaves the idle region
    /// without clearing it, no host action can re-power the cores. Do not go
    /// looking for a lever for bit 3; there isn't one. A GPU fault costs a
    /// reboot. Detect it instead -- see `g17p_zero_duration`, which fails the
    /// submission honestly rather than reporting a false PASS for work the GPU
    /// never executed.
    ///
    /// ### Caveat on this lever itself
    ///
    /// `0x21960` is NOT ungated: it early-exits at `0x21980`
    /// (`ldrb w8, [0x1771f8]; tbnz w8, #1, 0x21a04`) when the nap/sleep flag is
    /// set -- the same word whose `& 0x18` the `0x83` work doorbell's handler
    /// `0x261a4` tests before returning silently.
    ///
    /// Why this exists (all FW_PROVEN against b1, `/tmp/g17p-re/fwb1`):
    ///
    ///  * Our residual KSM pause mask `0x2` *is* the firmware's own record that
    ///    the GPU cores are unpowered. `gpu_core_power` `0xfffffc000000e004`
    ///    clears reason bit 1 at `0xe488` on its power-on arm and sets it at
    ///    `0xe4f8` on its power-off arm; nothing else writes it.
    ///  * With `hw_data + 0x2600 == 1` (ours and m1n1's value) the two
    ///    "full init" call forms `(1,1,0)` and `(1,0,0)` fall straight through
    ///    to `retab`, so neither the scheduler thread starting nor an RTKit
    ///    SLEEP -> ON transition releases the bit. In steady state exactly one
    ///    routine does: `0xfffffc0000021960`.
    ///  * Six call sites reach `0x21960`, and only two are host-drivable, both
    ///    on this ring. Device-control opcode `0x0a` with arg `0`
    ///    (`0xfffffc0000003fd8`) does an unconditional `bl 0x21960` with no
    ///    state gate whatsoever -- unlike the EP-0x21 `0x83` work doorbell we
    ///    already send, whose handler `0x261a4` returns silently, doing
    ///    nothing at all, while `[0xfffffc00001771f8] & 0x18` says the ASC has
    ///    napped or slept.
    ///  * It is idempotent (`0x21960` short-circuits on the powered latch) and
    ///    it clears the idle-power-off enable, so the cluster will not be
    ///    napped back down between submits. Native fronts all five of its
    ///    submit entry points with the equivalent `ensurePoweredHardware`, so
    ///    per-submit is faithful, and per-submit is also *required*: the
    ///    idle-off is timing-dependent, which is why m1n1 gets away with an
    ///    identical initdata and our longer bring-up does not.
    ///
    /// Nothing here is synchronous. `0x21960` latches, panics on its own if the
    /// latch fails, and tail-calls the core sequencer, which polls
    /// `sgx+0x2_11d0` on its own budget. The observables are KTrace `0x46` with
    /// `arg0 = 0, arg1 = 0x2` (class 0, drained below) and `sgx+0xe01000 & 0xf`
    /// leaving zero. Both are sampled, neither is waited on: a failure to power
    /// must not fail the submit, so every error here is a warning.
    /// Opt-in `g17p_diag_knobs` bit4 diagnostic. Publishes IdlePowerOff(0)
    /// once when the knob is set and IdlePowerOff(1) once after it is cleared,
    /// each at the next submission, without waiting for retirement. The
    /// production path (knob clear, nothing held) publishes nothing.
    fn sync_g17p_idle_hold(&mut self) {
        if crate::g17_timing_buffer::diag_knob(crate::g17_timing_buffer::DIAG_HWDATA_SNAPSHOT) {
            crate::g17_timing_buffer::clear_diag_knob(crate::g17_timing_buffer::DIAG_HWDATA_SNAPSHOT);
            if let Some(manager) = self.manager.as_mut() {
                let result = manager.snapshot_hw_data(&crate::g17_timing_buffer::GRID);
                if let Some(pdev) = self.pdev.as_ref() {
                    dev_info!(pdev.as_ref(), "G17P hwdata snapshot diagnostic: {:?}\n", result);
                }
            }
        }
        if crate::g17_timing_buffer::diag_knob(crate::g17_timing_buffer::DIAG_GRID_SNAPSHOT) {
            crate::g17_timing_buffer::clear_diag_knob(crate::g17_timing_buffer::DIAG_GRID_SNAPSHOT);
            if let Some(manager) = self.manager.as_mut() {
                let result = manager.snapshot_primary_state_prefix(&crate::g17_timing_buffer::GRID);
                if let Some(pdev) = self.pdev.as_ref() {
                    dev_info!(pdev.as_ref(), "G17P grid snapshot diagnostic: {:?}\n", result);
                }
            }
        }
        let want = crate::g17_timing_buffer::diag_knob(crate::g17_timing_buffer::DIAG_IDLE_HOLD);
        if want == self.idle_hold_published {
            return;
        }
        let arg = if want {
            g17_initdata::DEVICE_CONTROL_IDLE_POWER_OFF_ARG_DISABLE_IDLE_AND_WAKE
        } else {
            g17_initdata::DEVICE_CONTROL_IDLE_POWER_OFF_ARG_ENABLE_IDLE
        };
        let Some(pdev) = self.pdev.as_ref().cloned() else { return; };
        let Some(manager) = self.manager.as_mut() else { return; };
        let published = manager.publish_primary_device_control_record(
            g17_initdata::DEVICE_CONTROL_OPCODE_IDLE_POWER_OFF, arg);
        let result = published.and_then(|_| {
            fence(Ordering::SeqCst);
            Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?).send_message_logged(
                g17_rtkit::ENDPOINT_GFX_INTERRUPTS,
                g17_rtkit::encode_primary_device_control(),
            )
        });
        match result {
            Ok(()) => {
                self.idle_hold_published = want;
                dev_info!(pdev.as_ref(), "G17P idle hold diagnostic: IdlePowerOff({}) published\n", arg);
            }
            Err(error) => dev_warn!(pdev.as_ref(),
                "G17P idle hold diagnostic: IdlePowerOff({}) not published ({:?})\n", arg, error),
        }
    }

    fn wire_g17p_submit_gpu_power(&mut self) -> Result {
        let pdev = self.pdev.as_ref().cloned().ok_or(ENODEV)?;
        let publication = match self
            .manager
            .as_mut()
            .ok_or(ENODEV)?
            .publish_primary_device_control_record(
                g17_initdata::DEVICE_CONTROL_OPCODE_IDLE_POWER_OFF,
                g17_initdata::DEVICE_CONTROL_IDLE_POWER_OFF_ARG_DISABLE_IDLE_AND_WAKE,
            ) {
            Ok(publication) => publication,
            Err(error) => {
                dev_warn!(
                    pdev.as_ref(),
                    "G17P submit power: IdlePowerOff(0) record not staged ({:?}); submitting with the cores as they are\n",
                    error
                );
                return Ok(());
            }
        };
        fence(Ordering::SeqCst);
        let announcement = g17_rtkit::encode_primary_device_control();
        dev_info!(
            pdev.as_ref(),
            "G17P submit power: IdlePowerOff(0) staged opcode={:#x} arg={} at {:#x} slot={} producer {}->{} consumer={}; announcing {:#x}\n",
            publication.opcode,
            publication.arg,
            publication.record_address,
            publication.slot,
            publication.producer_before,
            publication.producer_after,
            publication.consumer_before,
            announcement
        );
        if let Err(error) = Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
            .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, announcement)
        {
            dev_warn!(
                pdev.as_ref(),
                "G17P submit power: device-control announcement failed ({:?}); the record stays staged for the next drain\n",
                error
            );
            return Ok(());
        }
        // A zero policy already disables idle descent. If this diagnostic asks
        // for an explicit hold, publish another zero after wake retirement so
        // a cold configuration refresh cannot restore Region-C's default one.
        if crate::g17_config::POWER_HOLD != 0 {
            match self
                .manager
                .as_mut()
                .ok_or(ENODEV)?
                .publish_primary_device_control_record(
                    g17_initdata::DEVICE_CONTROL_OPCODE_IDLE_POWER_OFF,
                    g17_initdata::DEVICE_CONTROL_IDLE_POWER_OFF_ARG_DISABLE_IDLE_AND_WAKE,
                ) {
                Ok(hold) => {
                    fence(Ordering::SeqCst);
                    dev_info!(
                        pdev.as_ref(),
                        "G17P submit power: post-wake idle-disable staged arg={} slot={} producer {}->{}\n",
                        hold.arg,
                        hold.slot,
                        hold.producer_before,
                        hold.producer_after
                    );
                    if let Err(error) = Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
                        .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, announcement)
                    {
                        dev_warn!(
                            pdev.as_ref(),
                            "G17P submit power: post-wake idle-disable announcement failed ({:?})\n",
                            error
                        );
                    }
                }
                Err(error) => dev_warn!(
                    pdev.as_ref(),
                    "G17P submit power: post-wake idle-disable not staged ({:?})\n",
                    error
                ),
            }
        }
        // Did the firmware actually drain both records? The consumer cursor is
        // the only direct evidence; a stalled consumer means the second record
        // is sitting unread and any conclusion drawn from it is void.
        for attempt in 0..8 {
            fsleep(Delta::from_millis(1));
            if let Ok(counters) = self.manager.as_mut().ok_or(ENODEV)?.control_counters() {
                dev_info!(
                    pdev.as_ref(),
                    "G17P submit power: control cursors after {}ms primary={:?} secondary={:?}\n",
                    attempt + 1,
                    counters.primary,
                    counters.secondary
                );
                if counters.primary[0] == counters.primary[2] {
                    break;
                }
            }
        }
        let mut gpc_state = 0u32;
        let mut powered = false;
        for _ in 0..G17P_POWER_WIRE_POLLS {
            match self
                .registers
                .as_ref()
                .ok_or(ENODEV)?
                .g17p_scheduler_gate_registers()
            {
                Ok(snapshot) => {
                    // Already masked to the low four bits by the accessor.
                    gpc_state = snapshot.host_irq_summary;
                    if gpc_state != 0 {
                        powered = true;
                        break;
                    }
                }
                Err(error) => {
                    dev_warn!(
                        pdev.as_ref(),
                        "G17P submit power: gpc-state read failed ({:?})\n",
                        error
                    );
                    break;
                }
            }
            fsleep(Delta::from_millis(1));
        }
        if powered {
            dev_info!(
                pdev.as_ref(),
                "G17P submit power: gpc-state={:#x} after IdlePowerOff(0); cores report powered (ceiling 10)\n",
                gpc_state
            );
        } else {
            dev_warn!(
                pdev.as_ref(),
                "G17P submit power: gpc-state still {:#x} after {} polls of IdlePowerOff(0)\n",
                gpc_state,
                G17P_POWER_WIRE_POLLS
            );
        }
        {
            let manager = self.manager.as_mut().ok_or(ENODEV)?;
            probe_g17p_ktrace(pdev.as_ref(), manager, "post-idle-power-off");
        }
        Ok(())
    }

    /// Publish one translated compute command on behalf of DRM queue `owner`.
    ///
    /// `owner` is the driver-side id of the client queue object. It is what
    /// makes the retained graph *per client*: the steady-state fast path below
    /// is taken only when the same client resubmits, and any other client goes
    /// through a recycle first.
    pub(crate) fn submit_translated_compute(
        &mut self,
        owner: u64,
        vm: &mmu::Vm,
        command: &g17_uapi::TranslatedComputeCommand,
        owned_timestamps: mmu::KernelMapping,
    ) -> Result<G17PSubmitOutcome> {
        if !self.test_stage.enables_compute_uapi() {
            return Err(ENOTSUPP);
        }
        if !self.accepting_submissions {
            return Err(ENOTSUPP);
        }
        self.settle_open_recovery_before_publication()?;
        // Compute owns disjoint QID4+ state and an independent application-
        // GART root.  Render's two job-private roots may stay live; reclaiming
        // either here would turn this publication back into a cross-engine
        // scheduling barrier.
        self.manager.as_mut().ok_or(ENODEV)?.stage_compute_flist_state()?;
        // Allocate the compute graph on actual compute use, never as a
        // prerequisite for render. This is a no-op once a graph exists.
        self.ensure_compute_queue()?;
        if !self.accepting_submissions {
            return Err(ENOTSUPP);
        }
        // Hand the graph over if a different client owned it last. This is the
        // step that makes a second process work at all: without it the retained
        // binding of a client that has already exited still pinned UAT slot 1,
        // and the new client's first submit tore down the whole session.
        self.acquire_compute_owner(owner)?;
        // A handoff may have rebuilt the whole session; re-read the gate.
        if !self.accepting_submissions {
            return Err(ENOTSUPP);
        }
        // QUIESCE the firmware after a closed gate, before publishing anything.
        //
        // FW RE (b1): pause bit 3 has exactly ONE clear site image-wide --
        // `0x25dd8`, inside the scheduler thread's idle descent, reached only
        // from `0x25c00` and the `cbnz -> 0x25dc0` routes at `0x25a04`/
        // `0x25a50`/`0x25a70`. The generic pause helper `0xa1c4(set, bit)` is
        // never called with bit 8 by anything in the image, so no host message
        // can clear it directly. But the descent's own guards at `0x25940` are
        // queue state we DO drive: it descends only when the producer at
        // `[x8+416]` equals the consumer at `[x8+432]`, none of mask
        // `0x8_032c_0007` is set in `[x19]`, and `[x19+96]` is zero. Submitting
        // continuously is what keeps the thread on the work path at `0x25e2c`,
        // which is why no 0x46 record appears after a fault at all.
        //
        // So: go quiet and let the thread reach a genuine idle. Its descent is
        // idempotent on an already-set bit 3, and its EXIT clears bit 3 --
        // leaving mask 0x2, from which the existing IdlePowerOff(0) lever
        // (`g17p_power_wire`, which reaches `0x21960`) is the clear that takes
        // the mask to zero and therefore actually invokes `0x20a70`.
        // Pair this with `g17p_power_wire=2`.
        if self.compute_gate_suspect {
            let quiesce = crate::g17_config::GATE_QUIESCE_MS;
            self.compute_gate_suspect = false;
            if quiesce != 0 {
                if let Some(pdev) = self.pdev.as_ref() {
                    dev_info!(
                        pdev.as_ref(),
                        "G17PMARK gate-quiesce {}ms before publishing; letting the scheduler reach a genuine idle so its exit can clear pause bit 3\n",
                        quiesce
                    );
                }
                fsleep(Delta::from_millis(quiesce as i64));
            }
        }
        // Same-owner retry after one of this client's own submissions was
        // abandoned. No handoff ran, so nothing has repaired the accounting
        // the abandoned kick leaked -- and `producer.submitted` is capped at
        // the geometry's 0x100 stamps, so a client that retries after failures
        // would otherwise wedge its own queue. Its binding is still live and
        // still owned by this client, so nothing needs quarantining here.
        if self.compute_abandoned
            && crate::g17_config::ABANDON_RECOVERY != 0
            && !self.is_crashed()
        {
            if let (Some(manager), Some(queue)) = (self.manager.as_mut(), self.queue.as_mut()) {
                manager.resynchronise_compute_queue_after_abandon(queue)?;
            }
            self.compute_abandoned = false;
        }
        // A repeat submission on a live queue must NOT rebuild the session.
        // The retained binding and timestamp mapping left behind by the last
        // submission are exactly what makes a steady-state resubmit possible;
        // tearing the firmware session down and re-running the first-bind
        // chronology instead is the behaviour that made repeats fail. Reuse
        // the binding when the queue is installed and the VM is unchanged.
        let steady_state_queue = !self.is_crashed()
            && self
                .queue
                .as_ref()
                .is_some_and(|queue| self.manager.as_ref().is_some_and(|manager| {
                    manager.compute_queue_is_installed(queue)
                }));
        let repeat_ready = steady_state_queue
            && self.compute_binding.as_ref().is_some_and(|binding| binding.matches(vm));
        // A handoff (`rebind_compute_queue`) released the previous owner's
        // binding and KEPT the installed QID 4 graph. This submission must
        // therefore bind the incoming VM and then take the SAME steady-state
        // chronology a repeat takes -- never the first-bind one, whose tag-15
        // record carries the queue-install flag and would make the firmware
        // disable/reprogram/enable a KSMFE queue it already has installed. The
        // manager routes on the queue alone (`hardware_state` plus
        // `submission_ordinal`), so retaining the queue is what selects the
        // right path; all this flag does is stop the host-side first-bind
        // preconditions from rejecting it.
        let rebind_ready =
            steady_state_queue && !repeat_ready && self.compute_binding.is_none();
        if repeat_ready || rebind_ready {
            // Only the previous submission's owned timestamp page is dropped;
            // on a repeat the user-VM binding stays, and on a rebind it is
            // already gone.
            drop(self.compute_timestamps.take());
        } else if self.is_crashed()
            || self.compute_binding.is_some()
            || self.compute_timestamps.is_some()
        {
            self.rebuild_session_or_defer()?;
        }
        // Never publish while a firmware recovery is open. Two independent
        // reasons, both firmware-proven:
        //  * while `status_b[0x4900] != 0` the scheduler dispatches only the
        //    unhalt event and stashes every work-scan bit, so nothing we
        //    publish can run; and
        //  * the unhalt's 128-queue restore rewrites 0x21010/0x21018/0x21020
        //    for every queue from the halt-time snapshot, which *erases* any
        //    tag-14 AddKicks published while the recovery was open.
        // Answering is also the only thing that closes a recovery: the host
        // writing 2 into status_b[0x4900] is what schedules the unhalt.
        self.settle_open_recovery_before_publication()?;
        // Any recovery above rebuilt the session, and a rebuilt session comes
        // back with no queue: `boot_session` defers QID 4 to the event worker.
        // Put one back synchronously, or every clause below reads `None` and
        // the submit fails ENODEV for a reason that has nothing to do with it.
        self.manager.as_mut().ok_or(ENODEV)?.stage_compute_flist_state()?;
        self.ensure_compute_queue()?;
        // The prepublication contract describes a pristine queue -- ordinal 0,
        // seed timestamp, nothing outstanding -- so it only applies to a first
        // bind. A steady-state queue legitimately violates every clause, and a
        // queue handed to a new client through `rebind_compute_queue` is a
        // steady-state queue: it keeps its ordinal, its stamp sequence and its
        // ring cursor precisely so the firmware never sees the install flag a
        // second time.
        if !repeat_ready && !rebind_ready {
            self.manager
                .as_ref()
                .ok_or(ENODEV)?
                .validate_prepublication_compute_queue(self.queue.as_ref().ok_or(ENODEV)?)?;
        }
        if g17_submission::require_g17p_running_client_power_transition(
            g17_submission::G17P_RUNNING_CLIENT_POWER_CONTRACT,
            self.cpus.is_some(),
            self.gfx1_rtkit.is_some(),
        )
        .is_err()
        {
            dev_err!(
                self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                "G17P compute power: retained target1 CPU/RTKit owner missing before VM bind\n"
            );
            return Err(ENOTSUPP);
        }
        // The one host lever that releases KSM pause reason bit 1.
        //
        // ORDER IS LOAD-BEARING when the mask is 0xa (bits 3|1). FW RE of b1:
        // every pause-reason clear site -- bit 1 at 0xe488, bit 3 at 0x25dd4 --
        // is `ands` followed by `b.ne <skip>`, so the core power sequencer
        // 0x20a70 is invoked ONLY on the transition to mask == 0. Clearing bit
        // 1 while bit 3 is still set therefore cannot re-power the cores by
        // construction, which is exactly why this lever measured as a no-op.
        // Mode 2 defers it until AFTER the kick is published, so the scheduler
        // has a chance to clear bit 3 first (0xdeb8 reports outstanding work ->
        // 0x25c00 -> 0x25dc0) and this record's bit-1 clear becomes the
        // transition to zero.
        self.sync_g17p_idle_hold();
        if crate::g17_config::POWER_WIRE == 1 {
            self.wire_g17p_submit_gpu_power()?;
        }
        {
            // Checkpoint B: the opening control handshake has retired by now,
            // so an armed KTrace must already have moved the producer even
            // though nothing has been submitted yet.
            let pdev_probe = self.pdev.as_ref().cloned().ok_or(ENODEV)?;
            let manager = self.manager.as_mut().ok_or(ENODEV)?;
            probe_g17p_ktrace(pdev_probe.as_ref(), manager, "pre-first-submit");
        }
        if owned_timestamps.size() < 16 {
            return Err(ERANGE);
        }
        let timestamp_start = owned_timestamps.iova();
        let timestamp_end = timestamp_start.checked_add(8).ok_or(EOVERFLOW)?;
        if !repeat_ready {
            let binding = {
                let manager = self.manager.as_mut().ok_or(ENODEV)?;
                let queue = self.queue.as_mut().ok_or(ENODEV)?;
                manager.bind_compute_user_vm(queue, vm)?
            };
            self.compute_binding = Some(binding);
        }
        let submit_result = {
            let registers = self.registers.as_ref().ok_or(ENODEV)?;
            let manager = self.manager.as_mut().ok_or(ENODEV)?;
            let queue = self.queue.as_mut().ok_or(ENODEV)?;
            let binding = self.compute_binding.as_ref().ok_or(ENODEV)?;
            let gfx_state = self.gfx_state.as_ref().ok_or(ENODEV)?;
            gfx_state
                .submission_phase
                .store(g17_manager::G17P_SUBMIT_PHASE_IDLE, Ordering::Release);
            manager.submit_translated_compute(
                registers,
                queue,
                &gfx_state.submission_phase,
                binding,
                command,
                g17_manager::G17PComputeSubmissionContext {
                    context_id: 2,
                    // Legacy synchronous context has only one live command.
                    work_state_word: 0,
                    work_state: None,
                    dispatch_identity: 0x0100_01d7_0200_01dc,
                    timestamps: g17_uapi::ComputeTimestampAddresses {
                        start: timestamp_start,
                        end: timestamp_end,
                    },
                    descriptor_flag_4c: false,
                    converted_command_timestamp: 1,
                    barriers: &[],
                    mcache: None,
                    rce_kind: 0,
                    auxiliary: 0x003f_ffff_ffff_ffff,
                },
            )
        };
        let notifications = match submit_result {
            Ok(notifications) => notifications,
            Err(error) => {
                let phase = self
                    .gfx_state
                    .as_ref()
                    .map(|state| state.submission_phase.load(Ordering::Acquire))
                    .unwrap_or(g17_manager::G17P_SUBMIT_PHASE_IDLE);
                dev_err!(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    "G17P compute: host submission failed before notification phase={} err={:?}\n",
                    phase,
                    error
                );
                // See the completion-wait path: a post-publication render
                // failure must not reboot the firmware CPUs. That reboot times
                // out and the failure path leaves the session refusing every
                // later submission.
                self.abandon_render_submission_after_timeout();
                return Err(error);
            }
        };
        let channel_path = notifications.is_some();
        if let Some(notifications) = notifications {
            if let Some(activation) = notifications.activation {
                log_g17p_scheduler_gate(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    self.registers.as_ref().ok_or(ENODEV)?,
                    "native-first-pre-0x0a",
                );
                log_g17p_status_a(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    self.manager.as_mut().ok_or(ENODEV)?,
                    "native-first-pre-0x0a",
                );
                log_g17p_firmware_recovery_handshake(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    self.manager.as_mut().ok_or(ENODEV)?,
                    "native-first-pre-0x0a",
                );
                log_g17p_fault_report(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    self.manager.as_mut().ok_or(ENODEV)?,
                    "native-first-pre-0x0a",
                );
                log_g17p_a2i_control(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    self.cpus.as_ref().ok_or(ENODEV)?.gfx(),
                    "native-first-pre-0x0a",
                );
                dev_info!(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    "G17P compute: CL_2 published; sending priority-2 activation {:#x}\n",
                    activation
                );
                if let Err(error) = Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
                    .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, activation)
                {
                    return Err(self.request_compute_session_rebuild(error, owned_timestamps));
                }
                log_g17p_a2i_control(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    self.cpus.as_ref().ok_or(ENODEV)?.gfx(),
                    "native-first-post-0x0a-pre-0x10",
                );
                log_g17p_status_a(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    self.manager.as_mut().ok_or(ENODEV)?,
                    "native-first-post-0x0a-pre-0x10",
                );
            }
            if let Some(direct_kick) = notifications.direct_kick {
                fence(Ordering::SeqCst);
                dev_info!(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    "G17P compute: direct EP kick {:#x} after activation\n",
                    direct_kick
                );
                if let Err(error) = Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
                    .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, direct_kick)
                {
                    return Err(self.request_compute_session_rebuild(error, owned_timestamps));
                }
                log_g17p_scheduler_gate(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    self.registers.as_ref().ok_or(ENODEV)?,
                    "native-first-post-0x10",
                );
                log_g17p_a2i_control(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    self.cpus.as_ref().ok_or(ENODEV)?.gfx(),
                    "native-first-post-0x10",
                );
                log_g17p_status_a(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    self.manager.as_mut().ok_or(ENODEV)?,
                    "native-first-post-0x10",
                );
                log_g17p_firmware_recovery_handshake(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    self.manager.as_mut().ok_or(ENODEV)?,
                    "native-first-post-0x10",
                );
                log_g17p_fault_report(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    self.manager.as_mut().ok_or(ENODEV)?,
                    "native-first-post-0x10",
                );
            }
        }

        // Deferred power lever: the kick is published and AddKicks has been
        // seen, so `0xdeb8` can now report outstanding work and let the
        // scheduler's idle loop clear pause bit 3. Clearing bit 1 here is then
        // the clear that takes the mask to zero and calls the core power
        // sequencer.
        if crate::g17_config::POWER_WIRE >= 2 {
            if let Err(error) = self.wire_g17p_submit_gpu_power() {
                dev_warn!(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    "G17P compute power: deferred power lever failed ({:?})\n",
                    error
                );
            }
        }
        let compute_queue_id = u64::from(
            self.manager
                .as_ref()
                .ok_or(ENODEV)?
                .compute_queue_id(self.queue.as_ref().ok_or(ENODEV)?)?,
        );
        // Control sample for the render blocker: compute reaches dispatch on
        // this same driver, so reading the KSM slot bank and the bit-4
        // new-work path right after a compute kick shows what a working
        // notification looks like. Same probe, same registers, same gate.
        self.probe_render_ksm_admission_after_render_doorbells(None);
        let mut scan_observation = G17PWorkScanObservation::new();
        let mut serviced_notifications: u32 = 0;
        let mut last_ksm_sample: (u64, u64, u64) = (0, 0, 0);
        let mut add_kicks_published = false;
        let pdev_poll = self.pdev.as_ref().cloned().ok_or(ENODEV)?;
        let mut service_failures: u32 = 0;
        let ktrace_enabled = g17p_ktrace_enabled();
        let ktrace_verbose = module_parameters::g17p_fw_ktrace_verbose.value() != 0;
        let ktrace_budget = module_parameters::g17p_fw_ktrace_budget.value();
        let mut ktrace_primary = G17PKtraceTotals::default();
        let mut ktrace_secondary = G17PKtraceTotals::default();
        let mut ktrace_failures: u32 = 0;
        if crate::g17_config::IRQ_COMPLETION != 0 {
            // Publish and return. The wait happens in the caller with the
            // runtime mutex released, so the RTKit doorbell's event worker can
            // actually run and signal us -- holding the mutex across a wait is
            // what forced the old 625 ms timestamp poll and its "inline event
            // service" workaround.
            self.completion_signal.arm();
            self.pending_compute = Some((channel_path, owned_timestamps));
            return Ok(G17PSubmitOutcome::Pending(self.completion_signal()));
        }
        let completion = owned_timestamps.with_cpu_bytes(|raw| {
            let polls = crate::g17_config::SUBMIT_POLLS as usize;
            let mut low_probe_done = crate::g17_config::PROBE_LOW_RANGE == 0;
            for poll in 0..polls {
                // Every previous probe of the sgx GPU-core register file ran
                // with the cores unpowered, where it takes an asynchronous
                // SError. By this point the firmware has raised the GPC
                // performance state, so retry once under the condition native
                // requires (isGPUCorePowerEnabled). Survival is itself the
                // result: if the range is still unreachable the machine dies
                // here, which distinguishes the outcomes without needing the
                // log to survive.
                // Gate on the probe parameter, not on `low_probe_done`:
                // that flag starts *true* when the probe is disabled, which
                // made this sampler issue SKSM port MMIO before the GPU cores
                // are powered and take an asynchronous SError.
                if crate::g17_config::PROBE_LOW_RANGE != 0
                    && low_probe_done
                    && poll >= 50
                    && poll % 50 == 0
                {
                    if let Some(registers) = self.registers.as_ref() {
                        if let Ok(sample) = registers.g17p_sample_ksm(compute_queue_id) {
                            if sample != last_ksm_sample {
                                last_ksm_sample = sample;
                                dev_info!(
                                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                                    "G17P KSM sample poll={} descriptor={:#x} pause={:#x} addkicks={:#x}\n",
                                    poll,
                                    sample.0,
                                    sample.1,
                                    sample.2
                                );
                            }
                        }
                    }
                }
                if !low_probe_done && poll >= 50 {
                    low_probe_done = true;
                    if let Some(registers) = self.registers.as_ref() {
                        match registers.g17p_probe_fault_info() {
                            Ok(value) => {
                                dev_info!(
                                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                                    "G17P LOW RANGE REACHABLE while powered: 0x17030 = {:#x}\n",
                                    value
                                );
                                if let Err(error) = registers.g17p_probe_ksm_block() {
                                    dev_warn!(
                                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                                        "G17P KSM block probe failed ({:?})\n",
                                        error
                                    );
                                }
                                if let Err(error) =
                                    registers.g17p_dump_queue_descriptor(compute_queue_id)
                                {
                                    dev_warn!(
                                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                                        "G17P queue descriptor read failed ({:?})\n",
                                        error
                                    );
                                }
                                if crate::g17_config::LATE_SKSM != 0 {
                                    dev_warn!(
                                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                                        "G17P SKSM replay suppressed: queue registration must be firmware-mediated\n"
                                    );
                                }
                            }
                            Err(error) => dev_warn!(
                                self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                                "G17P low-range probe failed ({:?})\n",
                                error
                            ),
                        }
                    }
                }
                match self.service_firmware_events_inline() {
                    Ok(count) => serviced_notifications += count,
                    Err(error) => {
                        // Log the first failure: swallowing it hid which of the
                        // drain's several `ok_or(ENODEV)` guards (or the batch
                        // drain itself) is refusing, on the one poll where the
                        // firmware actually had something for us.
                        if service_failures == 0 {
                            dev_warn!(
                                pdev_poll.as_ref(),
                                "G17P inline event service failed on poll {} ({:?})\n",
                                poll,
                                error
                            );
                        }
                        service_failures += 1;
                    }
                }
                // The submit path holds the runtime mutex for the whole
                // wait, which is why this drains here rather than from an
                // RTKit handler or a workqueue.
                if ktrace_enabled {
                    for role in [
                        g17_initdata::InstanceRole::Primary,
                        g17_initdata::InstanceRole::Secondary,
                    ] {
                        match self
                            .manager
                            .as_mut()
                            .ok_or(ENODEV)
                            .and_then(|manager| {
                                manager.drain_ktrace_ring(role, ktrace_verbose, ktrace_budget)
                            }) {
                            Ok(stats) => match role {
                                g17_initdata::InstanceRole::Primary => {
                                    ktrace_primary.accumulate(&stats)
                                }
                                g17_initdata::InstanceRole::Secondary => {
                                    ktrace_secondary.accumulate(&stats)
                                }
                            },
                            Err(_) => ktrace_failures += 1,
                        }
                    }
                }
                // Native defers the AddKicks poke and replays it from a
                // power-state callback. Publish it here, once the firmware's
                // recovery/power-up has closed -- publishing earlier is erased
                // by the unhalt's 128-queue restore.
                if !add_kicks_published
                    && self
                        .manager
                        .as_ref()
                        .is_some_and(|manager| manager.has_pending_add_kicks())
                {
                    let settled = self
                        .manager
                        .as_mut()
                        .ok_or(ENODEV)
                        .and_then(|manager| manager.firmware_recovery_handshake_snapshot())
                        .map(|handshake| handshake.state == 0)
                        .unwrap_or(false);
                    if settled {
                        add_kicks_published = true;
                        if let (Some(queue), Some(manager)) =
                            (self.queue.as_mut(), self.manager.as_mut())
                        {
                            if let Err(error) = manager.publish_pending_add_kicks(queue, 3) {
                                dev_warn!(
                                    pdev_poll.as_ref(),
                                    "G17P deferred AddKicks publish failed ({:?})\n",
                                    error
                                );
                            }
                            fence(Ordering::SeqCst);
                        }
                    }
                }
                match self
                    .manager
                    .as_mut()
                    .ok_or(ENODEV)
                    .and_then(|manager| manager.status_a_snapshot())
                {
                    Ok(snapshot) => scan_observation.observe(snapshot.scan_active),
                    Err(_) => scan_observation.observe_failure(),
                }
                fence(Ordering::Acquire);
                // The firmware reports completion in the KSM completion record,
                // not by writing the user timestamp pair: the descriptor's start
                // pointer is null, so `raw[0..8]` can never become non-zero.
                // Check the real record first.
                if let Some(manager) = self.manager.as_mut() {
                    if let Some(completion) = manager.read_compute_completion()? {
                        return Ok(completion);
                    }
                }
                let start = u64::from_le_bytes(raw[0..8].try_into().map_err(|_| ERANGE)?);
                let end = u64::from_le_bytes(raw[8..16].try_into().map_err(|_| ERANGE)?);
                if let Some(completion) = g17_uapi::completed_compute_timestamps(start, end) {
                    return Ok(completion);
                }
                fsleep(Delta::from_millis(1));
            }
            Err(ETIMEDOUT)
        });
        dev_info!(
            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
            "G17P Status-A scan observation first={} last={} or={} transitions={} samples={} failures={}\n",
            scan_observation.first,
            scan_observation.last,
            scan_observation.observed_or,
            scan_observation.transitions,
            scan_observation.samples,
            scan_observation.failures,
        );
        dev_info!(
            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
            "G17P inline event service: notifications={} failures={}\n",
            serviced_notifications,
            service_failures,
        );
        if module_parameters::g17p_fw_log.value() != 0 {
            match self
                .manager
                .as_mut()
                .ok_or(ENODEV)
                .and_then(|manager| manager.dump_firmware_log(60))
            {
                Ok(count) => dev_info!(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    "G17P fwlog: {} records\n",
                    count
                ),
                Err(error) => dev_warn!(
                    self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                    "G17P fwlog dump failed ({:?})\n",
                    error
                ),
            }
        }
        if ktrace_enabled {
            for role in [
                g17_initdata::InstanceRole::Primary,
                g17_initdata::InstanceRole::Secondary,
            ] {
                match self
                    .manager
                    .as_mut()
                    .ok_or(ENODEV)
                    .and_then(|manager| manager.drain_ktrace_ring(role, ktrace_verbose, 0))
                {
                    Ok(stats) => match role {
                        g17_initdata::InstanceRole::Primary => ktrace_primary.accumulate(&stats),
                        g17_initdata::InstanceRole::Secondary => {
                            ktrace_secondary.accumulate(&stats)
                        }
                    },
                    Err(_) => ktrace_failures += 1,
                }
            }
            dev_info!(
                self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                "G17P ktrace summary: primary producer={} consumed={} printed={} skipped={} lost={}/{} addkicks={} pause-mask={:#x} | secondary producer={} consumed={} printed={} skipped={} lost={}/{} addkicks={} pause-mask={:#x} | failures={}\n",
                ktrace_primary.last_producer,
                ktrace_primary.consumed,
                ktrace_primary.printed,
                ktrace_primary.skipped,
                ktrace_primary.lost_records,
                ktrace_primary.lost_windows,
                ktrace_primary.saw_add_kicks,
                ktrace_primary.last_pause_mask,
                ktrace_secondary.last_producer,
                ktrace_secondary.consumed,
                ktrace_secondary.printed,
                ktrace_secondary.skipped,
                ktrace_secondary.lost_records,
                ktrace_secondary.lost_windows,
                ktrace_secondary.saw_add_kicks,
                ktrace_secondary.last_pause_mask,
                ktrace_failures,
            );
        }
        if let Some(manager) = self.manager.as_mut() {
            let _ = manager.dump_completion_stamps("post-submit");
        }
        let completion = match completion {
            Ok(completion) => completion,
            Err(error) => {
                if channel_path {
                    log_g17p_scheduler_gate(
                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                        self.registers.as_ref().ok_or(ENODEV)?,
                        "native-first-timeout",
                    );
                    log_g17p_status_a(
                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                        self.manager.as_mut().ok_or(ENODEV)?,
                        "native-first-timeout",
                    );
                    log_g17p_firmware_recovery_handshake(
                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                        self.manager.as_mut().ok_or(ENODEV)?,
                        "native-first-timeout",
                    );
                    log_g17p_fault_report(
                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                        self.manager.as_mut().ok_or(ENODEV)?,
                        "native-first-timeout",
                    );
                    log_g17p_a2i_control(
                        self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                        self.cpus.as_ref().ok_or(ENODEV)?.gfx(),
                        "native-first-timeout",
                    );
                    let user_timestamps = owned_timestamps.with_cpu_bytes(|raw| {
                        Ok([
                            u64::from_le_bytes(raw[0..8].try_into().map_err(|_| ERANGE)?),
                            u64::from_le_bytes(raw[8..16].try_into().map_err(|_| ERANGE)?),
                        ])
                    });
                    let snapshot = {
                        let manager = self.manager.as_mut().ok_or(ENODEV)?;
                        let queue = self.queue.as_mut().ok_or(ENODEV)?;
                        manager.snapshot_compute_timeout(queue)
                    };
                    let pdev = self.pdev.as_ref().ok_or(ENODEV)?;
                    match (snapshot, user_timestamps) {
                        (Ok(snapshot), Ok(user_timestamps)) => {
                            let control = snapshot.control;
                            let channel = snapshot.channel;
                            let channel_scan = snapshot.channel_scan;
                            let channel_control_prefix = snapshot.channel_control_prefix;
                            let graph = snapshot.graph;
                            dev_err!(
                                pdev.as_ref(),
                                "G17P compute timeout: control primary={:?} secondary={:?}\n",
                                control.primary,
                                control.secondary
                            );
                            dev_err!(
                                pdev.as_ref(),
                                "G17P compute timeout: CL_2 table={} ring={:#x} state-ptrs(actual,mirror,producer)={:#x?} runtime-pointers(+120,+130)={:#x?} cursors(actual,mirror,producer)={:?} slot={} queue={:#x} kind={} flags={:#x} doorbell={:#x}\n",
                                channel.table_index,
                                channel.ring,
                                channel.state_addresses,
                                channel.runtime_descriptor_pointers,
                                channel.cursors,
                                channel.slot_index,
                                channel.slot_queue,
                                channel.slot_kind,
                                channel.slot_flags,
                                g17_submission::G17P_COMPUTE_WORK_DOORBELL
                            );
                            dev_err!(
                                pdev.as_ref(),
                                "G17P compute timeout: inner queue={:#x} pointers={:#x} ring={:#x} pointer[+0,+30,+40]={:?} queue[+18,+1c,+20]={:?} uuid={:#x} context={:#x}\n",
                                graph.queue,
                                graph.pointers,
                                graph.item_ring,
                                graph.cursors,
                                graph.gpu_read_cursors,
                                graph.uuid,
                                graph.queue_context
                            );
                            for scan in channel_scan {
                                dev_err!(
                                    pdev.as_ref(),
                                    "G17P compute timeout: scan table={} ring={:#x} state-ptrs(actual,mirror,producer)={:#x?} cursors(actual,mirror,producer)={:?} current-slot={} queue={:#x} kind={}\n",
                                    scan.table_index,
                                    scan.ring,
                                    scan.state_addresses,
                                    scan.cursors,
                                    scan.current_slot_index,
                                    scan.current_slot_queue,
                                    scan.current_slot_kind,
                                );
                            }
                            dev_err!(
                                pdev.as_ref(),
                                "G17P compute timeout: channel-control[0..0x38]={:x?}\n",
                                channel_control_prefix,
                            );
                            dev_err!(
                                pdev.as_ref(),
                                "G17P compute timeout: items={:#x?} event={:#x?} status={:#x?}\n",
                                graph.items,
                                graph.event,
                                graph.status
                            );
                            dev_err!(
                                pdev.as_ref(),
                                "G17P compute timeout: descriptor={:#x} selector={:#x} context={} grid={} timestamp-pointers={:#x?} values={:#x?}\n",
                                graph.descriptor,
                                graph.descriptor_selector,
                                graph.descriptor_context,
                                graph.descriptor_grid,
                                graph.descriptor_timestamps,
                                user_timestamps
                            );
                            dev_err!(
                                pdev.as_ref(),
                                "G17P compute timeout: optional-contexts={:#x?} grid={} uuid={:#x} shared={:#x} channel={:#x}; high-context={:#x} record={:#x?}\n",
                                graph.optional_contexts,
                                graph.optional_grid,
                                graph.optional_uuid,
                                graph.optional_shared,
                                graph.optional_channel,
                                graph.context_high,
                                graph.context_record
                            );
                        }
                        (Err(snapshot_error), _) => dev_err!(
                            pdev.as_ref(),
                            "G17P compute timeout snapshot failed ({:?})\n",
                            snapshot_error
                        ),
                        (_, Err(timestamp_error)) => dev_err!(
                            pdev.as_ref(),
                            "G17P compute timeout timestamp read failed ({:?})\n",
                            timestamp_error
                        ),
                    }
                }
                // See the completion-wait path: a post-publication render
                // failure must not reboot the firmware CPUs. That reboot times
                // out and the failure path leaves the session refusing every
                // later submission.
                self.abandon_render_submission_after_timeout();
                return Err(error);
            }
        };
        if !channel_path {
            let retire_result = self
                .manager
                .as_ref()
                .ok_or(ENODEV)?
                .complete_compute_entries(self.queue.as_mut().ok_or(ENODEV)?, 1);
            if let Err(error) = retire_result {
                // See the completion-wait path: a post-publication render
                // failure must not reboot the firmware CPUs. That reboot times
                // out and the failure path leaves the session refusing every
                // later submission.
                self.abandon_render_submission_after_timeout();
                return Err(error);
            }
        }
        if channel_path {
            self.compute_timestamps = Some(owned_timestamps);
            dev_info!(
                self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                "G17P compute: first direct CL timestamps complete; retaining timestamp mapping, VM binding, and QID state until native ordinal-0 completion is wired\n"
            );
        } else {
            drop(self.compute_binding.take());
        }
        Ok(G17PSubmitOutcome::Completed(completion))
    }

    /// Complete a submission that was published with `Pending`.
    ///
    /// Runs with the runtime mutex re-acquired, after the caller has been woken
    /// by the firmware-event path. The completion itself is read from the KSM
    /// completion record, which is where the firmware actually reports finished
    /// work -- the firmware event ring carries telemetry, and the descriptor's
    /// user timestamp start pointer is null so that pair never becomes valid.
    pub(crate) fn finish_compute_submission(&mut self) -> Result<[u64; 2]> {
        let (channel_path, owned_timestamps) = self.pending_compute.take().ok_or(EINVAL)?;
        // Keep the DMA timestamp page owned even if a later observation or
        // retirement step fails. A local variable would unmap it on return,
        // before the deferred recovery worker can stop the processors.
        self.compute_timestamps = Some(owned_timestamps);
        // The submission is no longer tracked by `pending_compute` from here
        // on, so until it is proven retired it counts as outstanding: every
        // early return below leaves work the firmware may still be holding.
        self.compute_abandoned = true;
        let completion = self
            .manager
            .as_mut()
            .ok_or(ENODEV)?
            .read_compute_completion()?
            .ok_or(ETIMEDOUT)?;
        // Consume it: KSM completion ordinal 0 is one overwritten slot, so
        // without this the record stays "complete" forever and the next
        // submission's wait would return instantly with these timestamps.
        self.manager
            .as_mut()
            .ok_or(ENODEV)?
            .commit_compute_completion(completion);
        // A retire whose start and end GPU timestamps are EQUAL means the
        // firmware retired work the GPU never executed. Measured on J700: a
        // healthy add3 retires with duration 0xda-0xdd ticks, and every
        // submission after a fault-blamed recovery retires with duration 0 and
        // an untouched destination buffer. Reporting that as success is the
        // worst outcome available -- it is a silent wrong answer, and for a
        // conformance run a false PASS is worse than a FAIL -- so record the
        // closed gate and, by default, fail the submission honestly.
        if completion[1] == completion[0] {
            self.compute_gate_suspect = true;
            if let Some(pdev) = self.pdev.as_ref() {
                let (installs, _, rebuilds, retains) = g17_manager::g17p_mark_counts();
                let zeros = g17_manager::G17P_MARK_ZERO_DURATION
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                dev_err!(
                    pdev.as_ref(),
                    "G17PMARK zero-duration-retire i={} z={} r={} h={} start==end=={:#x}; the firmware retired work the GPU never executed -- core power / KSM pause gate is closed\n",
                    installs,
                    zeros,
                    rebuilds,
                    retains,
                    completion[0]
                );
            }
            // Mode 1 fails the submission honestly but does NOT rebuild.
            // MEASURED: a rebuild attempted from a gated session fails after
            // ~1.02s -- one `apple_rtkit_wait_for_completion` timeout inside
            // `apple_rtkit_boot` -- and a FAILED rebuild is destructive: it has
            // already taken `cpus`, `gfx_state` and `gfx_rtkit`, and
            // `resume_submissions` never runs, so every later ioctl is ENODEV
            // and the device is dead for the boot. A 1s stall that kills the
            // device is strictly worse than an immediate honest error, so
            // rebuilding is opt-in (mode 2) until a rebuild is known to work
            // from this state.
            if crate::g17_config::ZERO_DURATION != 0 {
                return Err(ETIMEDOUT);
            }
        }
        if let (Some(manager), Some(queue)) = (self.manager.as_mut(), self.queue.as_mut()) {
            // The decisive sample: taken after a submission the GPU has
            // demonstrably executed, so a consumer cursor that is ever going to
            // move has moved by now.
            manager.log_compute_ring_state(queue, "post-completion");
            let ordinal = manager.compute_queue_submission_ordinal(queue);
            let depth = module_parameters::g17p_trace_depth.value();
            let trace = depth != 0 && ordinal <= depth;
            if module_parameters::g17p_completion_trace.value() != 0 && trace {
                manager.log_compute_retire(queue, completion, ordinal);
            }
            if trace {
                manager.log_compute_outer_channel("post-completion");
            }
        }
        // Retire the queue entry on BOTH paths. This used to run only on the
        // non-channel path, with a comment saying the channel path would retain
        // its state "until native ordinal-0 completion is wired" -- it is wired
        // now. Leaving the entry outstanding is why a second submission never
        // completed: submission 2 retires from the doorbell and passes, and
        // submission 3 then waits forever because the queue was never advanced.
        let retire_result = self
            .manager
            .as_ref()
            .ok_or(ENODEV)?
            .complete_compute_entries(self.queue.as_mut().ok_or(ENODEV)?, 1);
        if let Err(error) = retire_result {
            return Err(self.request_session_rebuild(error));
        }
        if channel_path {
            dev_info!(
                self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                "G17P compute: submission retired from the firmware doorbell; queue entry completed\n"
            );
        } else {
            drop(self.compute_timestamps.take());
            drop(self.compute_binding.take());
        }
        // Proven retired: the firmware reported this kick complete and the
        // queue entry has been advanced. A handoff may now take the cheap path.
        self.compute_abandoned = false;
        Ok(completion)
    }

    /// Log the GPU MMU fault bank.
    ///
    /// The Mesa/Vulkan dispatch fails with fault `reason = 3`, which
    /// `FAULT_REPORT_REASON` documents as the GMMU page fault, but the fault
    /// report itself carries no faulting address -- its fields stop at `+0x4c`.
    /// The address is in the MMU fault bank, and this is the only thing that
    /// tells us *which* address Mesa touched that we never mapped.
    ///
    /// This read was pulled from the session path historically because it
    /// SErrored while the GPU cores were unpowered. A submit now runs with the
    /// cores up (`gpc-state != 0`), so it is safe here in a way it was not
    /// before -- but only call it after a submit, never during startup.
    pub(crate) fn log_fault_bank(&self, when: &'static str) {
        if let Some(registers) = self.registers.as_ref() {
            let _ = registers.g17p_log_fault_bank(when);
        }
    }

    /// Drop the state a `Pending` publish parked, after a failed wait.
    /// Sample the scheduler gate (gpc-state / pause) around a submission.
    ///
    /// At depth the failure changed shape: a repeat retires cleanly and the GPU
    /// writes nothing. If the cores have powered down while the firmware still
    /// reports a completion, that shows up here and nowhere else.
    /// Compute's status page, sampled exactly as render's is -- same GPU VA,
    /// same read. DRAM only, so safe at any point including a timeout.
    pub(crate) fn log_compute_sksm_entries(&mut self, label: &'static str) {
        if let (Some(manager), Some(queue)) = (self.manager.as_mut(), self.queue.as_mut()) {
            manager.log_compute_sksm_entries(queue, label);
        }
    }

    pub(crate) fn log_render_sksm_entries(&mut self, render_slot: u8, label: &'static str) {
        if let Some(manager) = self.manager.as_mut() {
            manager.log_render_sksm_entries(render_slot, label);
        }
    }

    pub(crate) fn log_compute_status_pages(&mut self, label: &'static str) {
        if let Some(manager) = self.manager.as_mut() {
            manager.log_compute_status_pages(label);
        }
    }

    pub(crate) fn log_gate(&mut self, label: &'static str) {
        if let (Some(pdev), Some(registers)) = (self.pdev.as_ref(), self.registers.as_ref()) {
            log_g17p_scheduler_gate(pdev.as_ref(), registers, label);
        }
    }

    /// Probe the TA/3D QID pair in the short interval after both native render doorbells.
    /// Every low-register access remains behind a non-zero GPC-state check in
    /// `regs`, because performing it later on the recovery/timeout path panics
    /// the machine instead of yielding a useful diagnostic.
    fn probe_render_ksm_admission_after_render_doorbells(&mut self, render_slot: Option<u8>) {
        // Called from BOTH the compute and the render submit paths, and
        // deliberately sampled before the ksm_probe gate below so one hook
        // covers both. The open question is whether the new-work interrupt
        // ever fires at all: the render window measures bit 4 clear and
        // 0x10a60 = 0, and compute is the workload that actually completes.
        if crate::g17_config::IRQ_BURST >= 2 {
            if let (Some(registers), Some(bpdev)) =
                (self.registers.as_ref(), self.pdev.as_ref())
            {
                let mut seen = [(0u64, 0u64, 0u64); 12];
                match registers.g17p_burst_sample_irq(&mut seen) {
                    Ok(n) => dev_info!(
                        bpdev.as_ref(),
                        "G17P PATHIRQ distinct={} [{:#x}/{:#x}/{:#x}] [{:#x}/{:#x}/{:#x}] [{:#x}/{:#x}/{:#x}] [{:#x}/{:#x}/{:#x}]\n",
                        n,
                        seen[0].0, seen[0].1, seen[0].2,
                        seen[1].0, seen[1].1, seen[1].2,
                        seen[2].0, seen[2].1, seen[2].2,
                        seen[3].0, seen[3].1, seen[3].2,
                    ),
                    Err(error) => dev_warn!(
                        bpdev.as_ref(),
                        "G17P PATHIRQ failed ({:?})\n",
                        error,
                    ),
                }
            }
        }
        if crate::g17_config::RENDER_KSM_PROBE == 0 {
            return;
        }
        // B1's new-work path is a two-stage latch. FUN_216f0 first masks the
        // hardware IRQ status with DAT_179328 and moves SGX+0x10a60 into
        // DAT_177130; FUN_1db24 then transfers it to DAT_177230 and sets bit 4
        // in DAT_177200. Sample those firmware globals before the slower SGX
        // window walk so a missing code-02 job allocation can be localized to
        // hardware generation, IRQ service, or the pending transfer.
        if let Some(pdev) = self.pdev.as_ref() {
            let read = |link: u64| -> Option<u64> {
                crate::pgtable::LiveFirmwareU64Probe::new(
                    g17_manager::g17p_gfx_link_to_physical(link),
                )
                .and_then(|probe| probe.observe_mask(u64::MAX, 1))
                .map(|observation| observation.first)
                .ok()
            };
            dev_info!(
                pdev.as_ref(),
                "G17P render new-work globals: irq-mask={:#018x} latched={:#018x} pending={:#018x} events={:#018x} selector-word={:#018x}\n",
                read(0xffff_fc00_0017_9328).unwrap_or(u64::MAX),
                read(0xffff_fc00_0017_7130).unwrap_or(u64::MAX),
                read(0xffff_fc00_0017_7230).unwrap_or(u64::MAX),
                read(0xffff_fc00_0017_7200).unwrap_or(u64::MAX),
                read(0xffff_fc00_0017_85f0).unwrap_or(u64::MAX),
            );
        }
        let (Some(pdev), Some(registers)) = (self.pdev.as_ref(), self.registers.as_ref()) else {
            return;
        };
        let ta_qid = g17_resources::g17p_render_queue_id(true) as u8;
        let fragment_qid = g17_resources::g17p_render_queue_id(false) as u8;

        const POLLS: usize = 40;
        let mut last = None;
        let mut last_fault_irq = None;
        for poll in 0..POLLS {
            if poll == 2 && crate::g17_config::RENDER_3D_STROBE != 0 {
                // b1's state 7 releases the fragment's entry and activates its
                // descriptor BEFORE state 4 strobes; a still-parented entry
                // cannot start, which is why a bare strobe did nothing.
                let fragment_qid = g17_resources::g17p_render_queue_id(false) as u8;
                if let (Some(manager), Some(pdev)) =
                    (self.manager.as_mut(), self.pdev.as_ref())
                {
                    match manager.release_fragment_parent(1) {
                        Ok((before, after)) => dev_info!(
                            pdev.as_ref(),
                            "G17P render: DIAGNOSTIC released fragment parent link {:#x} -> {:#x}\n",
                            before,
                            after,
                        ),
                        Err(error) => dev_warn!(
                            pdev.as_ref(),
                            "G17P render: DIAGNOSTIC parent release failed ({:?})\n",
                            error,
                        ),
                    }
                }
                if let (Some(registers), Some(pdev)) =
                    (self.registers.as_ref(), self.pdev.as_ref())
                {
                    match registers.g17p_activate_queue_descriptor(fragment_qid) {
                        Ok((before, after)) => dev_info!(
                            pdev.as_ref(),
                            "G17P render: DIAGNOSTIC activated qid {} descriptor {:#018x} -> {:#018x}\n",
                            fragment_qid,
                            before,
                            after,
                        ),
                        Err(error) => dev_warn!(
                            pdev.as_ref(),
                            "G17P render: DIAGNOSTIC descriptor activate failed ({:?})\n",
                            error,
                        ),
                    }
                }
                if let (Some(registers), Some(pdev)) =
                    (self.registers.as_ref(), self.pdev.as_ref())
                {
                    let selector = crate::g17_config::RENDER_3D_STROBE - 1;
                    match registers.g17p_strobe_3d_launch(selector) {
                        Ok((slot, mode, strobe)) => dev_info!(
                            pdev.as_ref(),
                            "G17P render: DIAGNOSTIC 3D strobe selector={} -> slot={:#x} mode={:#018x} strobe={:#x}\n",
                            selector,
                            slot,
                            mode,
                            strobe,
                        ),
                        Err(error) => dev_warn!(
                            pdev.as_ref(),
                            "G17P render: DIAGNOSTIC 3D strobe failed ({:?})\n",
                            error,
                        ),
                    }
                }
            }
            let engine_timestamps = match self
                .manager
                .as_mut()
                .ok_or(ENODEV)
                .and_then(|manager| manager.render_engine_timestamps(render_slot.ok_or(ENODEV)?))
            {
                Ok(timestamps) => timestamps,
                // A compute submission has no render storage. Sample the
                // hardware anyway: this probe's whole value on the compute
                // path is the KSM slot bank and the bit-4 new-work registers,
                // which are what a working dispatch has to move.
                Err(_) => [[0, 0]; 2],
            };
            match registers.g17p_render_ksm_admission_snapshot(ta_qid, fragment_qid) {
                Ok(Some(snapshot)) => {
                    let observation = (snapshot, engine_timestamps);
                    if last != Some(observation)
                        && (snapshot.pdm_qid0_state != 0
                            || snapshot.pdm_qid0_progress != 0
                            || snapshot.ta_state != 0
                            || snapshot.ta_progress != 0
                            || snapshot.fragment_state != 0
                            || snapshot.fragment_progress != 0
                            || snapshot.qid_counters != [[0; 3]; 3]
                            || snapshot.queue_enabled != [0; 2]
                            || snapshot.current_command != 0
                            || snapshot.current_context != 0
                            || snapshot.active_queue_mask != 0
                            || snapshot.active_queue_state != 0
                            || snapshot.fault_masks != [0; 3]
                            || snapshot.dm_masks != [0; 3]
                            || snapshot.usc.is_some()
                            || engine_timestamps != [[0, 0]; 2])
                    {
                        dev_info!(
                            pdev.as_ref(),
                            "G17P render KSM admission post-doorbells poll={} gpc-state={:#x} pdm-qid0=[state={:#018x},progress={:#018x}] ta-qid{}=[state={:#018x},low6={:#x},progress={:#018x},progress-low40={:#x}] 3d-qid{}=[state={:#018x},low6={:#x},progress={:#018x},progress-low40={:#x}] current-command={:#018x} current-context={:#018x} active-queue=[{:#018x},{:#018x}] stage-masks=[{:#018x},{:#018x},{:#018x}] dm-masks=[{:#018x},{:#018x},{:#018x}] ta-ts=[{:#x},{:#x}] 3d-ts=[{:#x},{:#x}]\n",
                            poll,
                            snapshot.gpc_state,
                            snapshot.pdm_qid0_state,
                            snapshot.pdm_qid0_progress,
                            snapshot.ta_qid,
                            snapshot.ta_state,
                            snapshot.ta_state & 0x3f,
                            snapshot.ta_progress,
                            snapshot.ta_progress & ((1u64 << 40) - 1),
                            snapshot.fragment_qid,
                            snapshot.fragment_state,
                            snapshot.fragment_state & 0x3f,
                            snapshot.fragment_progress,
                            snapshot.fragment_progress & ((1u64 << 40) - 1),
                            snapshot.current_command,
                            snapshot.current_context,
                            snapshot.active_queue_mask,
                            snapshot.active_queue_state,
                            snapshot.fault_masks[0],
                            snapshot.fault_masks[1],
                            snapshot.fault_masks[2],
                            snapshot.dm_masks[0],
                            snapshot.dm_masks[1],
                            snapshot.dm_masks[2],
                            engine_timestamps[0][0],
                            engine_timestamps[0][1],
                            engine_timestamps[1][0],
                            engine_timestamps[1][1],
                        );
                        dev_info!(
                            pdev.as_ref(),
                            "G17P render KSM scheduler post-doorbells poll={} pause=[request={:#018x},status={:#018x}] stop-request=[{:#018x},{:#018x}] stopped=[{:#018x},{:#018x}] resource=[clear={:#018x}:{:#018x},go={:#018x}:{:#018x},callback={:#018x}] scheduler-status={:#018x} work-channels=[{:#018x},{:#018x},{:#018x},{:#018x}] producers16=[{:#x},{:#x},{:#x},{:#x}]\n",
                            poll,
                            snapshot.scheduling_pause_request,
                            snapshot.scheduling_pause_status,
                            snapshot.queue_stop_request[0],
                            snapshot.queue_stop_request[1],
                            snapshot.queue_stopped[0],
                            snapshot.queue_stopped[1],
                            snapshot.resource_clear[1],
                            snapshot.resource_clear[0],
                            snapshot.resource_go[1],
                            snapshot.resource_go[0],
                            snapshot.resource_callback_state,
                            snapshot.scheduler_status,
                            snapshot.work_channel_status[0],
                            snapshot.work_channel_status[1],
                            snapshot.work_channel_status[2],
                            snapshot.work_channel_status[3],
                            (snapshot.work_channel_status[0] >> 16) & 0xffff,
                            (snapshot.work_channel_status[1] >> 16) & 0xffff,
                            (snapshot.work_channel_status[2] >> 16) & 0xffff,
                            (snapshot.work_channel_status[3] >> 16) & 0xffff,
                        );
                        // Sweep the pipe window while the cores are still up:
                        // by the wait-failed checkpoint the port is no longer
                        // drivable and the read returns EAGAIN.
                        if poll == 1 && crate::g17_config::PIPE_SWEEP != 0 {
                            if let Err(error) = registers.g17p_sweep_pipe_registers("poll1") {
                                dev_warn!(
                                    pdev.as_ref(),
                                    "G17P pipe sweep failed ({:?})\n",
                                    error,
                                );
                            }
                        }
                        dev_info!(
                            pdev.as_ref(),
                            "G17P render IRQ routing post-doorbells poll={} selector={:#010x} mode={:#018x} strobe={:#010x} irq=[enable={:#018x},status={:#018x},bit3={},bit4={}] completion-bitmap=[pending={:#018x},ack={:#018x}] new-work-bitmap=[pending={:#018x},ack={:#018x}]\n",
                            poll,
                            snapshot.launch_slot,
                            snapshot.launch_mode,
                            snapshot.launch_strobe,
                            snapshot.launch_irq_enable,
                            snapshot.launch_irq_status,
                            (snapshot.launch_irq_status >> 3) & 1,
                            (snapshot.launch_irq_status >> 4) & 1,
                            snapshot.launch_slot_pending,
                            snapshot.launch_slot_ack,
                            snapshot.new_work_pending,
                            snapshot.new_work_ack,
                        );
                        dev_info!(
                            pdev.as_ref(),
                            "G17P render KSM counters post-doorbells poll={} TA[outstanding={:#018x},submitted={:#018x},completed={:#018x}] 3D[outstanding={:#018x},submitted={:#018x},completed={:#018x}] enabled-mask={:#018x}:{:#018x} valid-mask={:#018x}:{:#018x} descriptor[TA]={:#018x} descriptor[3D]={:#018x} CL4[outstanding={:#018x},submitted={:#018x},completed={:#018x}] descriptor[CL4]={:#018x}\n",
                            poll,
                            snapshot.qid_counters[0][0],
                            snapshot.qid_counters[0][1],
                            snapshot.qid_counters[0][2],
                            snapshot.qid_counters[1][0],
                            snapshot.qid_counters[1][1],
                            snapshot.qid_counters[1][2],
                            snapshot.queue_enabled[1],
                            snapshot.queue_enabled[0],
                            snapshot.queue_valid_mask[1],
                            snapshot.queue_valid_mask[0],
                            snapshot.queue_descriptor[0],
                            snapshot.queue_descriptor[1],
                            snapshot.qid_counters[2][0],
                            snapshot.qid_counters[2][1],
                            snapshot.qid_counters[2][2],
                            snapshot.queue_descriptor[2],
                        );
                        dev_info!(
                            pdev.as_ref(),
                            "G17P render KSM launch poll={} dm-control={:#018x} dm-stopped={:#018x} descriptor=[TA={:#018x},3D={:#018x}] enabled=[{:#x},{:#x}] resume=[{:#x},{:#x}] pipe-vdm-stream=[{:#x},{:#x},{:#x},{:#x}] pipe-work-stamp=[{:#x},{:#x},{:#x},{:#x}] completed-by-qid=[{:#x},{:#x},{:#x},{:#x},{:#x},{:#x},{:#x},{:#x}]\n",
                            poll,
                            snapshot.dm_control,
                            snapshot.dm_stopped,
                            snapshot.queue_descriptor[0],
                            snapshot.queue_descriptor[1],
                            snapshot.queue_enabled_mask[0],
                            snapshot.queue_enabled_mask[1],
                            snapshot.queue_resume_mask[0],
                            snapshot.queue_resume_mask[1],
                            snapshot.pipe_vdm_stream[0],
                            snapshot.pipe_vdm_stream[1],
                            snapshot.pipe_vdm_stream[2],
                            snapshot.pipe_vdm_stream[3],
                            snapshot.pipe_work_stamp[0],
                            snapshot.pipe_work_stamp[1],
                            snapshot.pipe_work_stamp[2],
                            snapshot.pipe_work_stamp[3],
                            snapshot.completed_by_qid[0],
                            snapshot.completed_by_qid[1],
                            snapshot.completed_by_qid[2],
                            snapshot.completed_by_qid[3],
                            snapshot.completed_by_qid[4],
                            snapshot.completed_by_qid[5],
                            snapshot.completed_by_qid[6],
                            snapshot.completed_by_qid[7],
                        );
                        for slot in 0..4 {
                            let progress = snapshot.slot_progress[slot];
                            dev_info!(
                                pdev.as_ref(),
                                "G17P render KSM slot {} poll={} state={:#018x} progress={:#018x} -> qid={} ts={:#x} occupied={} phase={:#x} pipe={}\n",
                                slot,
                                poll,
                                snapshot.slot_state[slot],
                                progress,
                                (progress >> 40) & 0x7f,
                                progress & 0xff_ffff_ffff,
                                snapshot.slot_state[slot] & 1,
                                (snapshot.slot_state[slot] >> 1) & 0x1f,
                                ((snapshot.slot_state[slot] >> 24) & 3) as i64 - 1,
                            );
                        }
                        if let Some(usc) = snapshot.usc {
                            dev_info!(
                                pdev.as_ref(),
                                "G17P render USC engine admission post-doorbells poll={} pipe={} serv=[vdm={:#018x},pdm={:#018x},cdm={:#018x}] debug=[vdm={:#018x},pdm={:#018x},cdm={:#018x}] rce-targets[01739,15021,14080,16068,16461,16090,16098,16429]=[{:#018x},{:#018x},{:#018x},{:#018x},{:#018x},{:#018x},{:#018x},{:#018x}]\n",
                                poll,
                                usc.pipe,
                                usc.vdm_serv,
                                usc.pdm_serv,
                                usc.cdm_serv,
                                usc.vdm_debug_status,
                                usc.pdm_debug_status,
                                usc.cdm_debug_status,
                                usc.fragment_rce_targets[0],
                                usc.fragment_rce_targets[1],
                                usc.fragment_rce_targets[2],
                                usc.fragment_rce_targets[3],
                                usc.fragment_rce_targets[4],
                                usc.fragment_rce_targets[5],
                                usc.fragment_rce_targets[6],
                                usc.fragment_rce_targets[7],
                            );
                        }
                    }
                    last = Some(observation);
                    if engine_timestamps[1].iter().all(|value| *value != 0) {
                        return;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    dev_err!(
                        pdev.as_ref(),
                        "G17P render KSM admission post-doorbells read failed ({:?})\n",
                        error,
                    );
                    return;
                }
            }
            match registers.g17p_render_fault_irq_snapshot() {
                Ok(Some(snapshot)) => {
                    if last_fault_irq != Some(snapshot)
                        && (snapshot.sub_status != 0
                            || snapshot.irq_status & (1u64 << 35) != 0
                            || snapshot.fault_info & 1 != 0)
                    {
                        dev_info!(
                            pdev.as_ref(),
                            "G17P fault IRQ post-doorbells poll={}: gpc-state={:#x} info={:#018x} addr-word={:#018x} gate={:#018x} sub-status={:#018x} irq-status={:#018x} bit35={}\n",
                            poll,
                            snapshot.gpc_state,
                            snapshot.fault_info,
                            snapshot.fault_addr_word,
                            snapshot.requestor_gate,
                            snapshot.sub_status,
                            snapshot.irq_status,
                            (snapshot.irq_status >> 35) & 1,
                        );
                    }
                    last_fault_irq = Some(snapshot);
                }
                Ok(None) => {}
                Err(error) => {
                    dev_err!(
                        pdev.as_ref(),
                        "G17P fault IRQ post-doorbells poll={} read failed ({:?})\n",
                        poll,
                        error,
                    );
                    return;
                }
            }
            if poll + 1 != POLLS {
                fsleep(Delta::from_millis(1));
            }
        }
        dev_warn!(
            pdev.as_ref(),
            "G17P render KSM admission post-doorbells ended after {}ms without a 3D timestamp\n",
            POLLS,
        );
    }

    /// One optional hardware-count bracket. Errors are metadata: no wake or
    /// retry, and unavailable telemetry must not prevent the owned-RAM capture.
    fn capture_completion_pending_metadata(&self, metadata_base: usize) {
        let host = crate::g17_timing_buffer::arch_counter();
        let values = self.registers.as_ref().ok_or(ENODEV)
            .and_then(|registers| registers.g17p_completion_pending_snapshot());
        crate::g17_timing_buffer::completion_snapshot_meta(metadata_base, host);
        match values {
            Ok(words) => {
                crate::g17_timing_buffer::completion_snapshot_meta(metadata_base + 1, 0);
                crate::g17_timing_buffer::completion_snapshot_meta(metadata_base + 2, words[0]);
                crate::g17_timing_buffer::completion_snapshot_meta(metadata_base + 3, words[1]);
            }
            Err(error) => {
                crate::g17_timing_buffer::completion_snapshot_meta(
                    metadata_base + 1, error.to_errno() as i64 as u64);
            }
        }
    }

    /// Explicit bounded snapshot; no ordinary retirement-path ring scan.
    pub(crate) fn capture_completion_ring_snapshot(&mut self) -> Result {
        if self.platform_removed || self.is_crashed() || self.primary_recovery_pending() {
            return Err(ENODEV);
        }
        self.capture_completion_pending_metadata(41);
        let result = self.manager.as_mut().ok_or(ENODEV)?
            .capture_completion_ring_snapshot();
        self.capture_completion_pending_metadata(45);
        result
    }

    /// Decode the completion ring; called when a doorbell wait times out.

    pub(crate) fn dump_completion_records(&mut self, label: &'static str) {
        if let Some(manager) = self.manager.as_mut() {
            let _ = manager.dump_completion_records(label);
        }
    }

    pub(crate) fn abandon_compute_submission(&mut self) {
        // The firmware was never seen to retire this kick. Remember it, so a
        // later client handoff takes the CPU-stopping path rather than the
        // cheap one.
        self.compute_abandoned = true;
        drop(self.pending_compute.take());
        // Ring/channel state at the moment of failure. This is the sample that
        // was missing: the detailed timeout dump that already existed only ever
        // ran on the legacy in-submit poll path (`g17p_irq_completion=0`), and
        // the condvar timeout we actually hit printed nothing about the queue.
        //
        // DRAM only, deliberately, and this is the exact spot where that
        // matters: by the time a submit has timed out the GPU cores have
        // powered back down, so anything touching the sgx register file here
        // takes an asynchronous SError and panics instead of reporting. Every
        // field below is a plain read of an object the driver itself allocated.
        if let (Some(manager), Some(queue)) = (self.manager.as_mut(), self.queue.as_mut()) {
            manager.log_compute_ring_state(queue, "timeout");
        }
        if let Some(manager) = self.manager.as_mut() {
            manager.log_compute_outer_channel("timeout");
            // Scan BOTH KSM completion buffers, ordinal 0 and ordinal 2.
            // `read_compute_completion` only ever looks at ordinal 0, and only
            // ordinal 0 is cleared at publish -- so if the firmware routes some
            // completions to ordinal 2 we would see exactly this: a submission
            // that ran fine and a wait that never ends. This scan reports every
            // non-zero word in both, which settles it. Also DRAM only.
            // Decoded first: one line per populated completion record, plus
            // whether the descriptor this submission is waiting for is present
            // anywhere in the ring. That single fact splits "the firmware
            // reported nothing" from "it reported and the reader rejected it".
            let _ = manager.dump_compute_completion_records("timeout");
            // Raw words as a backstop; it truncates at 24 non-zero words, which
            // is about three records, so it is no longer the primary view.
            let _ = manager.dump_completion_stamps("timeout");
        }
    }

    /// Build and retain the exact T8140 dual-role runtime.
    pub(crate) fn start(
        pdev: &platform::Device<Core<'_>>,
        drm: ARef<AsahiDevice>,
        registers: regs::Resources,
        soc: &'static hw::agx3::SocConfig,
        id: &identity::GpuIdentity,
        observed: &hw::GpuIdConfig,
        _topology: T8140LiveTopologyAdmission,
    ) -> Result<Self> {
        if soc.chip_id != 0x8140
            || id.gpu_gen != hw::GpuGen::G17
            || id.gpu_variant != identity::GpuVariant::P
        {
            return Err(EINVAL);
        }
        let test_stage =
            G17PTestStage::from_parameter(module_parameters::g17p_test_stage.value())?;
        // `observed` was populated by the sole safe SGX GPU-ID probe before
        // either ASC starts. Retain its hardware-derived values rather than
        // re-reading SGX here after ownership has begun moving into runtime.
        let num_clusters = observed.num_clusters;

        dev_info!(
            pdev.as_ref(),
            "G17P live-boot: {:?} admitted before effects; selected {} stage\n",
            BootStage::ExactTopology,
            test_stage.name()
        );
        dev_info!(
            pdev.as_ref(),
            "G17P diagnostic: scheduler-root-checkpoints-v1 armed\n"
        );

        // Complete every fallible host-only allocation before either firmware
        // processor starts. A `?` in the later struct literal would otherwise
        // partially drop `manager` before `cpus`, violating the same stop-first
        // rule enforced by normal runtime teardown.
        let completion_signal = G17PCompletionSignal::new()?;
        // Render publication's dependency scratch: a host allocation, so it
        // also precedes the firmware boot.
        let render_dependency_scratch =
            KBox::new(g17_manager::G17PRenderDependencies::empty(), GFP_KERNEL)?;
        // Keep owner-capacity arrays out of the by-value runtime construction
        // and the probe caller's frame. Allocate before either ASC starts.
        let owner_count = g17_resources::G17PRenderQueuePair::SLOT_COUNT as usize;
        let mut pending_render = KVec::with_capacity(owner_count, GFP_KERNEL)?;
        let mut pending_render_queued = KVec::with_capacity(owner_count, GFP_KERNEL)?;
        let mut render_event_masks = KVec::with_capacity(owner_count, GFP_KERNEL)?;
        for _ in 0..owner_count {
            pending_render.push(None, GFP_KERNEL)?;
            pending_render_queued.push(g17_completion::PairFifo::new(), GFP_KERNEL)?;
            render_event_masks.push([0; 2], GFP_KERNEL)?;
        }
        let quarantined_render_completions = KVec::with_capacity(
            g17_resources::G17PRenderQueuePair::SLOT_COUNT as usize
                * g17_resources::G17P_RENDER_MAX_IN_FLIGHT,
            GFP_KERNEL,
        )?;
        let preparations = crate::g17_preparation::G17PPreparations::new()?;
        let pdev_ref: ARef<platform::Device> = pdev.into();
        // Before ANY processor starts. The firmware writes its own `__DATA`
        // for as long as it runs, and an ASC coming out of a real block reset
        // re-enters at IORVBAR over whatever its predecessor left there, so
        // this is the only moment a pristine copy can be taken.
        #[cfg(CONFIG_DEV_COREDUMP)]
        let firmware_data = snapshot_g17p_firmware_data(&pdev_ref);
        let (manager, session) = match boot_initial_session(
            &pdev_ref,
            &drm,
            &registers,
            soc,
            num_clusters,
            observed.gpc_perf_state_map,
            observed.gpc_perf_state_map_low,
            observed.gpc_perf_state_control,
            test_stage,
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                dev_err!(
                    pdev.as_ref(),
                    "G17P live-boot: initial session failed ({:?})\n",
                    error
                );
                return Err(error);
            }
        };
        // Render admission is independent of whether compute has ever been
        // used. The first compute submit ensures its own graph synchronously.
        let accepting_submissions = session.compute_uapi_enabled;

        Ok(Self {
            pdev: Some(pdev_ref),
            drm: Some(drm),
            registers: Some(registers),
            manager: Some(manager),
            cpus: Some(session.cpus),
            boot_stop_unproven: false,
            session_recovery_pending: false,
            session_rebuild_requested: false,
            preparations,
            runtime_pm_armed: false,
            #[cfg(CONFIG_DEV_COREDUMP)]
            firmware_data,
            gfx_state: Some(session.gfx_state),
            gfx_rtkit: Some(session.gfx_rtkit),
            gfx1_state: Some(session.gfx1_state),
            gfx1_rtkit: Some(session.gfx1_rtkit),
            queue: session.queue,
            compute_binding: None,
            compute_timestamps: None,
            completion_indices: g17_completion::QueueIndices {
                done: 0,
                read: 0,
                write: 0,
            },
            render_tracker: None,
            test_stage,
            accepting_submissions,
            platform_removed: false,
            queue_setup_pending: session.queue_setup_pending,
            compute_owner: None,
            compute_recycle_pending: false,
            compute_abandoned: false,
            retired_queues: KVec::new(),
            async_compute_queues: KVec::new(),
            quarantined_bindings: KVec::new(),
            crash_recoveries: 0,
            compute_gate_suspect: false,
            idle_hold_published: false,
            submit_count: 1,
            submit_open: false,
            clpc_effort: 0,
            power_target_mw: 0,
            dvfs_ce: crate::g17_dvfs_policy::CePolicy::new(),
            dvfs_native_bracket: false,
            completion_signal,
            pending_compute: None,
            pending_render,
            pending_render_queued,
            queue_registry: None,
            quarantined_render_completions,
            reclaim_pending_since: None,
            render_event_masks,
            render_dependency_scratch: Some(render_dependency_scratch),
            deferred_render_timeouts: 0,
            render_fence_state: g17_completion::RenderFenceState::default(),
            trace_service_entry_counter: 0,
            deferred_completions: KVec::new(),
            deferred_completion_drain_owned: false,
        })
    }

    /// Publish first graphics in the measured 3D/TA order and send its one
    /// pair-zero/channel-0 work notification through the retained primary
    /// RTKit endpoint.
    pub(crate) fn submit_partial_opening_render<F>(
        &mut self,
        submission: g17_manager::G17PPartialOpeningRenderSubmission,
        publish_late_tiling_state: F,
    ) -> Result
    where
        F: FnOnce(),
    {
        if self.render_tracker.is_some() {
            return Err(EBUSY);
        }
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        let gfx_rtkit = self.gfx_rtkit.as_mut().ok_or(ENODEV)?;
        let (tracker, prepared) =
            manager.submit_partial_opening_render(submission, publish_late_tiling_state)?;
        Pin::new(gfx_rtkit)
            .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, prepared.paired_doorbell)?;
        self.render_tracker = Some(tracker);
        Ok(())
    }

    /// Consume queue progress together with the scheduler job-list state and
    /// the owned start/end timestamps required for execution completion.
    pub(crate) fn observe_partial_opening_render_completion(
        &mut self,
        tiling_current: g17_completion::QueueIndices,
        fragment_current: g17_completion::QueueIndices,
        execution: g17_completion::RenderExecutionState,
    ) -> Result<g17_completion::PairedRenderCompletion> {
        let manager = self.manager.as_ref().ok_or(ENODEV)?;
        let tracker = self.render_tracker.as_mut().ok_or(ENODEV)?;
        let completion = manager.observe_partial_opening_render_completion(
            tracker,
            tiling_current,
            fragment_current,
            execution,
        )?;
        if completion.complete {
            self.render_tracker = None;
        }
        Ok(completion)
    }

    /// Apply one fully routed normal-CL completion to the retained queue.
    ///
    /// The physical IRQ, ordinal-0 descriptor, MMIO acknowledgement, and
    /// EP0x20 stamp-event route remain closed. This legacy boundary therefore
    /// rejects the incomplete selector-2-only input before queue mutation.
    pub(crate) fn observe_compute_completion(
        &mut self,
        raw: &[u8],
        current: g17_completion::QueueIndices,
        published_prefix: u32,
    ) -> Result<g17_completion::QueuePrefixState> {
        let _ = (raw, current, published_prefix);
        if g17_completion::require_g17p_normal_cl_completion_route(
            g17_completion::G17P_NORMAL_CL_COMPLETION_ROUTE,
        )
        .is_err()
        {
            dev_err!(
                self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
                "G17P completion: live ordinal0 IRQ/EP0x20 route unavailable; outstanding unchanged\n"
            );
            return Err(ENOTSUPP);
        }
        dev_err!(
            self.pdev.as_ref().ok_or(ENODEV)?.as_ref(),
            "G17P completion: legacy selector2 input lacks ordinal0 stamp correlation; outstanding unchanged\n"
        );
        Err(ENOTSUPP)
    }

    /// Commit only a transaction which already passed ordinal-0 entry and
    /// EP0x20 stamp correlation. The route gate remains closed until Linux
    /// owns the physical IRQ, firmware-event ring, and completion ACK writer.
    pub(crate) fn apply_prepared_normal_cl_completion(
        &mut self,
        prepared: g17_completion::PreparedG17PNormalClCompletion,
    ) -> Result {
        g17_completion::require_g17p_normal_cl_completion_route(
            g17_completion::G17P_NORMAL_CL_COMPLETION_ROUTE,
        )
        .map_err(|_| ENOTSUPP)?;
        let manager = self.manager.as_ref().ok_or(ENODEV)?;
        let queue = self.queue.as_mut().ok_or(ENODEV)?;
        if prepared.entry.queue_id != manager.compute_queue_id(queue)? {
            return Err(EINVAL);
        }
        manager.complete_compute_entries(queue, prepared.completed_generation)
    }
}

/// Mark one RTKit role's session permanently crashed, naming the step that did
/// it.
///
/// `crashed` makes `service_primary_firmware_events` and
/// `primary_firmware_event_pending` return early forever, so every later
/// doorbell is a silent no-op and no completion, timeout or recovery is ever
/// reported again. That makes this transition the single most important line in
/// the log, and it carried only an errno: the event chain has eight fallible
/// steps that can all surface as EIO, so a wedge from a real firmware fault and
/// a wedge from a duplicate recovery record read identically.
///
/// `reason=` matches `note_session_recovery_pending` deliberately, so one grep
/// -- `G17PMARK session-crashed` -- finds every route into `is_crashed()`.
fn note_state_crashed(
    pdev: &ARef<platform::Device>,
    state: &G17PRoleRtkitState,
    reason: &'static str,
    error: Error,
    generation: Option<u64>,
) {
    let repeat = state.crashed.swap(true, Ordering::AcqRel);
    dev_err!(
        pdev.as_ref(),
        "G17PMARK session-crashed reason={} role={} err={:?} generation={:?} repeat={} -- from here the event worker stops reporting completions, timeouts and recoveries\n",
        reason,
        state.processor.name(),
        error,
        generation,
        repeat,
    );
}

/// Raise `session_recovery_pending`, which is the third route into
/// `is_crashed()` and the only one that was entirely silent.
///
/// Both writers set it in passing, and neither said anything: a client whose
/// render was settled-lost, and the rebuild's own `pause_submissions`. From the
/// write on, `is_crashed()` is true, every later submit takes the EIO refusal
/// and the next device query triggers a session rebuild -- with nothing in the
/// log naming the moment. A wedge with no fault report and a wedge from a real
/// firmware fault therefore looked the same.
///
/// Logs the transition, not every write: the flag is set repeatedly inside
/// per-slot loops and a line per write would bury the one that matters.
pub(crate) fn note_session_recovery_pending(
    pending: &mut bool,
    pdev: Option<&ARef<platform::Device>>,
    reason: &'static str,
) {
    if *pending {
        return;
    }
    *pending = true;
    if let Some(pdev) = pdev {
        dev_err!(
            pdev.as_ref(),
            "G17PMARK session-crashed reason={} -- from here is_crashed() is true, the event worker stops reporting completions, timeouts and recoveries, and the next client open triggers a session rebuild\n",
            reason,
        );
    }
}

impl g17_lifecycle::PersistentRuntimeRecovery for G17PLiveRuntime {
    type Error = Error;

    fn pause_submissions(&mut self) {
        // Any full rebuild consumes the queued request. A worker already
        // queued for it then observes no request and performs no duplicate reset.
        self.session_rebuild_requested = false;
        self.accepting_submissions = false;
        note_session_recovery_pending(
            &mut self.session_recovery_pending,
            self.pdev.as_ref(),
            "pause-submissions-for-rebuild",
        );
    }

    fn stop_cpu_pair(&mut self) -> Result {
        if self.boot_stop_unproven {
            return Err(EIO);
        }
        // Bracket the stop with the ASC state it is supposed to change. All a
        // stop writes is CPU_CONTROL: RUN cleared, reread, bit 5 cleared. If
        // CPU_STATUS and the reset vector are unchanged either side of it, the
        // processor was gated, not reset, and the later HELLO wait is waiting
        // for a handshake that was never re-armed.
        let pdev = self.pdev.as_ref().cloned();
        if let (Some(pdev), Some(cpus)) = (pdev.as_ref(), self.cpus.as_ref()) {
            log_g17p_asc_state(pdev.as_ref(), cpus.gfx(), "gfx", "pre-cpu-stop");
            log_g17p_asc_state(pdev.as_ref(), cpus.gfx1(), "gfx1", "pre-cpu-stop");
        }
        let result = match self.cpus.as_mut() {
            Some(cpus) => cpus.stop(),
            None => Ok(()),
        };
        if let (Some(pdev), Some(cpus)) = (pdev.as_ref(), self.cpus.as_ref()) {
            log_g17p_asc_state(pdev.as_ref(), cpus.gfx(), "gfx", "post-cpu-stop");
            log_g17p_asc_state(pdev.as_ref(), cpus.gfx1(), "gfx1", "post-cpu-stop");
        }
        result
    }

    fn fail_outstanding_jobs_while_owned(&mut self) {
        // A failed processor stop is not a retirement witness. Signal every
        // accepted fence so syncobj waiters observe device loss, but leave the
        // completion packets, VM pins, render-slot leases, roots, and mappings
        // in their runtime owners for a later successful stop or final leak-
        // safe teardown.
        for completion in self.pending_render.iter().flatten() {
            completion.fail_while_owned(EIO);
        }
        for completion in self.pending_render_queued.iter().flatten() {
            completion.fail_while_owned(EIO);
        }
        for queue in &mut self.async_compute_queues {
            if queue.inflight.is_none() {
                continue;
            }
            queue.quarantined = true;
            for active in queue.inflight.iter() {
                active._lease.quarantine();
                if let Ok(completion) = active.packet.completion() {
                    completion.fail_while_owned(EIO);
                }
            }
        }
    }

    fn drop_gfx1_rtkit(&mut self) {
        drop(self.gfx1_rtkit.take());
        drop(self.gfx1_state.take());
    }

    fn drop_gfx_rtkit(&mut self) {
        drop(self.gfx_rtkit.take());
        drop(self.gfx_state.take());
    }

    fn disable_sksm_queue(&mut self) -> Result {
        if let Some(queue) = self.queue.as_mut() {
            self.manager
                .as_ref()
                .ok_or(ENODEV)?
                .unregister_compute_sksm_queue(self.registers.as_ref().ok_or(ENODEV)?, queue)?;
        }
        Ok(())
    }

    fn release_runtime_objects(&mut self) {
        self.signal_deferred_completions_in_place();
        drop(self.pending_compute.take());
        for completion in self.pending_render.iter_mut().filter_map(Option::take) {
            completion.complete(Err(EIO));
            // `complete` is exact-once and deliberately keeps guards when an
            // earlier failed-stop attempt already signalled the fence. This
            // successful stop is the independent release witness.
            completion.release_after_processor_stop();
        }
        for queued in self.pending_render_queued.iter_mut() {
            while let Some(completion) = queued.take() {
                completion.complete(Err(EIO));
                completion.release_after_processor_stop();
            }
        }
        self.release_quarantined_render_after_processor_stop();
        self.render_event_masks.fill([0; 2]);
        self.deferred_render_timeouts = 0;
        self.render_fence_state = g17_completion::RenderFenceState::default();
        drop(self.compute_timestamps.take());
        drop(self.compute_binding.take());
        drop(self.queue.take());
        // Both CPUs have stopped by the time this runs, so the graphs a
        // client handoff retired can finally be freed: nothing in the
        // firmware can still dereference them.
        self.retired_queues.clear();
        self.release_async_compute_after_processor_stop();
        // Both CPUs have stopped, so a quarantined client's page tables and
        // GEM objects can finally go too.
        self.quarantined_bindings.clear();
        self.compute_owner = None;
        self.compute_recycle_pending = false;
        self.compute_abandoned = false;
        self.compute_gate_suspect = false;
        if let Some(manager) = self.manager.as_mut() {
            manager.release_runtime_objects_after_processor_stop();
        }
        self.render_tracker = None;
        self.completion_indices = g17_completion::QueueIndices {
            done: 0,
            read: 0,
            write: 0,
        };
    }

    fn rebuild_session_in_place(&mut self) -> Result {
        if self.boot_stop_unproven {
            return Err(EIO);
        }
        drop(self.cpus.take());
        let pdev = self.pdev.as_ref().ok_or(ENODEV)?.clone();
        let drm = self.drm.as_ref().ok_or(ENODEV)?.clone();
        if crate::g17_config::ASC_RESTART >= 2 {
            let cycled = match self.cycle_gfx_power_domain(&pdev) {
                Ok(()) => true,
                Err(error) => {
                    dev_err!(
                        pdev.as_ref(),
                        "G17PMARK asc-powercycle failed ({:?}); continuing with the plain CPU_RUN restart\n",
                        error
                    );
                    false
                }
            };
            // Only restore when the domain actually cycled. Clearing CPU_RUN is
            // a gate on this ASCWrap v6, not a reset: if the gate failed the
            // firmware may still be live, and writing 1.2MB into a running
            // IOP's globals is not a diagnostic, it is corruption. Reporting
            // drift is a pure read and stays unconditional.
            #[cfg(CONFIG_DEV_COREDUMP)]
            self.report_and_restore_firmware_data(
                &pdev,
                cycled && crate::g17_config::ASC_RESTART >= 3,
            );
        }
        let (boot_result, stop_unproven) = {
            let registers = self.registers.as_ref().ok_or(ENODEV)?;
            let manager = self.manager.as_mut().ok_or(ENODEV)?;
            manager.reset_session_after_processor_stop()?;
            let mut stop_unproven = false;
            let result = boot_session(
                &pdev,
                &drm,
                registers,
                manager,
                self.test_stage,
                &mut stop_unproven,
            );
            (result, stop_unproven)
        };
        if stop_unproven {
            self.boot_stop_unproven = true;
        }
        let session = boot_result?;
        self.cpus = Some(session.cpus);
        self.gfx_state = Some(session.gfx_state);
        self.gfx_rtkit = Some(session.gfx_rtkit);
        self.gfx1_state = Some(session.gfx1_state);
        self.gfx1_rtkit = Some(session.gfx1_rtkit);
        self.queue = session.queue;
        self.queue_setup_pending = session.queue_setup_pending;
        self.arm_primary_firmware_event_worker()?;
        Ok(())
    }

    fn resume_submissions(&mut self) {
        // As on initial boot, admission must not depend on the lazy compute
        // graph. Render can be first after recovery; compute ensures its own
        // graph synchronously when submitted.
        self.accepting_submissions = self.test_stage.enables_compute_uapi()
            && self.preparations.reopen();
        self.session_recovery_pending = false;
    }
}

impl g17_lifecycle::PersistentRuntimeTeardown for G17PLiveRuntime {
    fn stop_cpu_pair(&mut self) -> bool {
        if self.boot_stop_unproven {
            if let Some(pdev) = self.pdev.as_ref() {
                dev_err!(
                    pdev.as_ref(),
                    "G17P teardown: failed boot left a CPU stop unproven; retaining firmware-owned state\n"
                );
            }
            return false;
        }
        let result = match self.cpus.as_mut() {
            Some(cpus) => cpus.stop(),
            None => Ok(()),
        };
        match result {
            Ok(()) => {
                // Everything below runs with both CPUs stopped and before any
                // owner is dropped, which is the only window where the ASC
                // cannot write and the domain is still up to be read.
                let flags = crate::g17_config::ASC_TEARDOWN;
                if let Some(pdev) = self.pdev.as_ref().map(|pdev| pdev.clone()) {
                    if flags & 1 != 0 {
                        wait_for_g17p_asc_idle(&pdev, "teardown");
                    }
                    #[cfg(CONFIG_DEV_COREDUMP)]
                    if flags & 2 != 0 {
                        self.report_and_restore_firmware_data(&pdev, true);
                    }
                }
                true
            }
            Err(error) => {
                if let Some(pdev) = self.pdev.as_ref() {
                    dev_err!(
                        pdev.as_ref(),
                        "G17P teardown: CPU stop failed ({:?}); retaining firmware-owned state\n",
                        error
                    );
                } else {
                    pr_err!(
                        "G17P teardown: CPU stop failed ({:?}); retaining firmware-owned state\n",
                        error
                    );
                }
                false
            }
        }
    }

    fn retain_after_stop_failure(&mut self) {
        self.accepting_submissions = false;
        // Firmware-retired completions are not owners to retain.
        self.signal_deferred_completions_in_place();
        core::mem::forget(self.pending_compute.take());
        core::mem::forget(self.compute_timestamps.take());
        core::mem::forget(self.compute_binding.take());
        core::mem::forget(self.queue.take());
        core::mem::forget(core::mem::take(&mut self.retired_queues));
        core::mem::forget(core::mem::take(&mut self.async_compute_queues));
        core::mem::forget(core::mem::take(&mut self.quarantined_bindings));
        core::mem::forget(core::mem::take(&mut self.pending_render));
        core::mem::forget(core::mem::take(&mut self.pending_render_queued));
        core::mem::forget(core::mem::take(
            &mut self.quarantined_render_completions,
        ));
        core::mem::forget(self.gfx1_rtkit.take());
        core::mem::forget(self.gfx1_state.take());
        core::mem::forget(self.gfx_rtkit.take());
        core::mem::forget(self.gfx_state.take());
        core::mem::forget(self.cpus.take());
        core::mem::forget(self.manager.take());
        core::mem::forget(self.drm.take());
        core::mem::forget(self.registers.take());
        core::mem::forget(self.pdev.take());
    }

    fn unregister_queue(&mut self) {
        if let (Some(manager), Some(registers), Some(queue)) = (
            self.manager.as_ref(),
            self.registers.as_ref(),
            self.queue.as_mut(),
        ) {
            let _ = manager.unregister_compute_sksm_queue(registers, queue);
        }
    }

    fn drop_queue(&mut self) {
        self.signal_deferred_completions_in_place();
        for completion in self.pending_render.iter_mut().filter_map(Option::take) {
            completion.complete(Err(EIO));
            completion.release_after_processor_stop();
        }
        for queued in self.pending_render_queued.iter_mut() {
            while let Some(completion) = queued.take() {
                completion.complete(Err(EIO));
                completion.release_after_processor_stop();
            }
        }
        self.release_quarantined_render_after_processor_stop();
        self.render_event_masks.fill([0; 2]);
        self.deferred_render_timeouts = 0;
        self.render_fence_state = g17_completion::RenderFenceState::default();
        drop(self.compute_timestamps.take());
        drop(self.compute_binding.take());
        drop(self.queue.take());
        self.retired_queues.clear();
        self.release_async_compute_after_processor_stop();
        self.quarantined_bindings.clear();
        self.compute_owner = None;
        self.compute_recycle_pending = false;
        self.compute_abandoned = false;
    }

    fn drop_gfx1_rtkit(&mut self) {
        drop(self.gfx1_rtkit.take());
        drop(self.gfx1_state.take());
    }

    fn drop_gfx_rtkit(&mut self) {
        drop(self.gfx_rtkit.take());
        drop(self.gfx_state.take());
    }

    fn drop_cpu_pair(&mut self) {
        drop(self.cpus.take());
    }

    fn drop_manager(&mut self) {
        if let Some(manager) = self.manager.as_ref() {
            manager.uat().mark_firmware_cache_stopped();
        }
        drop(self.manager.take());
        // This callback is reached only after teardown_persistent_runtime has
        // proven the CPU pair stopped and released its queue/transport owners.
        // manager == None is not a stop witness: failed-stop retention also
        // takes that handle while deliberately keeping firmware memory alive.
        g17_resources::g17p_fw_pool_release_all();
    }
}

impl Drop for G17PLiveRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

include!("g17_async_compute.rs");

/// v96: firmware TVB-grow (event 6) records drained from the event ring.
static GROW_ARRIVALS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// v92c: submit-path waits taken by the FList takeover guard (diagnostic).
pub(crate) static G17P_TAKEOVER_WAITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
