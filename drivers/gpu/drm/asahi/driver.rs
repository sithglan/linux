// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! Top-level GPU driver implementation.

use kernel::{
    bindings, c_str,
    device::Core,
    dma::{
        Device,
        DmaMask, //
    },
    drm,
    drm::ioctl,
    new_mutex,
    of,
    platform,
    prelude::*,
    sync::{
        aref::ARef,
        Arc, Mutex, //
    },
    time::{delay::fsleep, Delta},
    workqueue::{
        self, DelayedWork, HasDelayedWork, HasWork, impl_has_delayed_work, impl_has_work,
        new_delayed_work, new_work, Work, WorkItem, //
    },
};

use crate::{
    agx_power_recovery,
    debug,
    drm_gpu,
    file,
    g15_boot,
    g17_boot,
    g17_drm,
    g17_firmware,
    g17_lifecycle,
    g17_live_boot,
    g17_manager,
    g17_power_preflight,
    g18_live_boot,
    gem::AsahiObject,
    gpu,
    hw,
    identity,
    regs, //
};

use kernel::macros::vtable;

/// Holds a reference to the top-level `GpuManager` object.
#[pin_data]
pub(crate) struct AsahiData {
    #[pin]
    pub(crate) gpu: Arc<dyn drm_gpu::DrmGpu>,
    pub(crate) pdev: ARef<platform::Device>,
    pub(crate) resources: Option<regs::Resources>,
    pub(crate) g17_runtime: Option<g17_drm::G17PRuntimeRef>,
    #[pin]
    g17_event_work: Work<AsahiDevice, 1>,
    #[pin]
    g17_recovery_work: Work<AsahiDevice, 2>,
    #[pin]
    g17_reclaim_work: Work<AsahiDevice, 3>,
    #[pin]
    g17_tvb_grow_work: Work<AsahiDevice, 4>,
    /// `g17p_dvfs_parity` bit0 idle re-check (clears the effort after the
    /// hold window). Never queued while the parity mask is zero.
    #[pin]
    g17_dvfs_work: DelayedWork<AsahiDevice, 5>,
    /// v84 GPU-feed CPU performance actuator (`g17_host_path`).
    #[pin]
    g17_feed_perf_work: Work<AsahiDevice, 6>,
    /// v97d `g17p_recovery_path` bit4 3D stall watch. Never queued while the
    /// bit is clear.
    #[pin]
    g17_stall_work: DelayedWork<AsahiDevice, 7>,
    #[pin]
    g17_reclaims: Mutex<KVec<KVec<g17_manager::G17PLogicalQueueTeardown>>>,
}

unsafe impl Send for AsahiData {}
unsafe impl Sync for AsahiData {}

impl_has_work! {
    impl HasWork<AsahiDevice, 1> for AsahiData { self.g17_event_work }
    impl HasWork<AsahiDevice, 2> for AsahiData { self.g17_recovery_work }
    impl HasWork<AsahiDevice, 3> for AsahiData { self.g17_reclaim_work }
    impl HasWork<AsahiDevice, 4> for AsahiData { self.g17_tvb_grow_work }
    impl HasWork<AsahiDevice, 6> for AsahiData { self.g17_feed_perf_work }
}

impl_has_delayed_work! {
    impl HasDelayedWork<AsahiDevice, 5> for AsahiData { self.g17_dvfs_work }
    impl HasDelayedWork<AsahiDevice, 7> for AsahiData { self.g17_stall_work }
}

impl WorkItem<1> for AsahiData {
    type Pointer = ARef<AsahiDevice>;

    fn run(dev: ARef<AsahiDevice>) {
        let Some(runtime) = dev.g17_runtime.as_ref() else {
            return;
        };
        if let Err(error) = runtime.service_primary_firmware_events() {
            dev_err!(
                dev.as_ref(),
                "G17P event worker: serialized drain/recovery failed ({:?})\n",
                error
            );
            return;
        }
        if crate::g17_host_path::enabled(crate::g17_host_path::WORKER_TAIL) {
            g17p_event_worker_tail_merged(&dev, runtime);
            return;
        }
        // DVFS parity (v83 bit0): completions are the idle edge of real work.
        service_g17p_dvfs_parity(dev.clone(), runtime);
        // Firmware acknowledgements detach complete owner graphs. Reclaim
        // their mappings/backing on another worker: the event worker must
        // remain available for completions and firmware grow requests.
        g17p_hand_off_teardowns(&dev, runtime.service_logical_queue_teardowns());
        if runtime.primary_device_control_backpressured() {
            // Full-ring backpressure is exceptional. Release the runtime
            // mutex before yielding so submissions and firmware progress are
            // never serialized behind this retry.
            fsleep(Delta::from_millis(1));
        }
        if crate::g17_host_path::take_pending() {
            let _ = workqueue::system_unbound().enqueue::<ARef<AsahiDevice>, 6>(dev.clone());
        }
        if runtime.primary_recovery_pending() {
            queue_g17p_recovery_worker(dev);
        } else if runtime.logical_queue_teardown_polling() {
            // A young teardown waits for the control-ring consumer; the
            // firmware does not notify on consumption, so re-check at
            // millisecond cadence, off the mutex, until it is acknowledged
            // or ages past the poll budget (then ordinary events re-check).
            fsleep(Delta::from_millis(1));
            queue_g17p_firmware_event_worker(dev);
        } else {
            requeue_g17p_firmware_event_worker_if_pending(dev);
        }
    }
}

/// The event worker's single hand-off of acknowledged teardowns to the
/// reclaim worker (shared by the v84 and the v87 WORKER_TAIL sequences).
fn g17p_hand_off_teardowns(
    dev: &ARef<AsahiDevice>,
    settled: Result<KVec<g17_manager::G17PLogicalQueueTeardown>>,
) {
    match settled {
        Ok(settled) => queue_g17p_retired_graphs(dev.clone(), settled),
        Err(error) => dev_err!(
            dev.as_ref(),
            "G17P event worker: teardown service failed ({:?})\n",
            error
        ),
    }
}

/// Event-worker bookkeeping decisions taken under one Runtime hold.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum G17PWorkerTailNext {
    /// Control ring back-pressured: sleep 1 ms, then decide as before.
    Backpressured,
    /// Queue the recovery worker.
    Recovery,
    /// A young teardown polls the control consumer: sleep 1 ms and requeue.
    TeardownPoll,
    /// Nothing else; any pending self-requeue was issued under the hold.
    Done,
}

/// v87 WORKER_TAIL decision, in the exact order of the separate checks:
/// back-pressure first, then recovery, then teardown polling, else requeue.
pub(crate) const fn g17p_worker_tail_next(
    backpressured: bool, recovery_pending: bool, teardown_polling: bool,
) -> G17PWorkerTailNext {
    if backpressured {
        G17PWorkerTailNext::Backpressured
    } else if recovery_pending {
        G17PWorkerTailNext::Recovery
    } else if teardown_polling {
        G17PWorkerTailNext::TeardownPoll
    } else {
        G17PWorkerTailNext::Done
    }
}

/// v87 WORKER_TAIL: the event worker's post-drain bookkeeping with one
/// Runtime acquisition instead of six. Each step is the same call, in the
/// same order, as the separate-lock sequence in `WorkItem<1>::run`:
/// DVFS parity evaluation and its delayed re-check (enqueued under the hold,
/// as before), teardown service, the back-pressure test, then the recovery /
/// teardown-poll / self-requeue decision, with the self-requeue issued under
/// the hold exactly as `requeue_g17p_firmware_event_worker_if_pending` does.
/// Reclaim hand-off, sleeps and the feed-perf and recovery work items stay
/// outside Runtime. Back-pressure keeps the original sleep-then-recheck path.
fn g17p_event_worker_tail_merged(dev: &ARef<AsahiDevice>, runtime: &g17_drm::G17PRuntimeRef) {
    use core::sync::atomic::Ordering;
    crate::g17_host_path::count(&crate::g17_host_path::TAIL_MERGED);
    let (settled, next) = {
        let mut guard = runtime.mutex().lock();
        // DVFS parity (v83 bit0): identical to `service_g17p_dvfs_parity`.
        if (crate::g17_dvfs_parity::enabled(crate::g17_dvfs_policy::PARITY_CE)
            || crate::g17_dvfs_parity::CE_LIVE.load(Ordering::Relaxed) != 0)
            && guard.firmware_event_worker_armed()
        {
            if let Some(delay) = guard.dvfs_parity_idle_check() {
                let ms = crate::g17_dvfs_policy::recheck_ms(delay);
                crate::g17_dvfs_parity::RECHECKS.fetch_add(1, Ordering::Relaxed);
                let _ = workqueue::system().enqueue_delayed::<ARef<AsahiDevice>, 5>(
                    dev.clone(), kernel::time::msecs_to_jiffies(ms.min(1000) as u32));
            }
        }
        let settled = guard.service_logical_queue_teardowns();
        let backpressured = guard.primary_device_control_backpressured();
        let next = if backpressured {
            g17p_worker_tail_next(true, false, false)
        } else {
            let recovery = guard.primary_recovery_pending();
            let polling = !recovery && guard.logical_queue_teardown_polling();
            let next = g17p_worker_tail_next(false, recovery, polling);
            if next == G17PWorkerTailNext::Done
                && guard.firmware_event_worker_armed()
                && guard.primary_firmware_event_pending()
            {
                // Same mutex as disarm: unbind either cancels this work or
                // disarms first and makes this self-requeue impossible.
                let _ = workqueue::system_highpri().enqueue::<ARef<AsahiDevice>, 1>(dev.clone());
            }
            next
        };
        (settled, next)
    };
    g17p_hand_off_teardowns(dev, settled);
    if next == G17PWorkerTailNext::Backpressured {
        // Full-ring backpressure is exceptional. Release the runtime
        // mutex before yielding so submissions and firmware progress are
        // never serialized behind this retry.
        fsleep(Delta::from_millis(1));
    }
    if crate::g17_host_path::take_pending() {
        let _ = workqueue::system_unbound().enqueue::<ARef<AsahiDevice>, 6>(dev.clone());
    }
    match next {
        G17PWorkerTailNext::Backpressured => {
            // Unchanged sequence after the sleep.
            if runtime.primary_recovery_pending() {
                queue_g17p_recovery_worker(dev.clone());
            } else if runtime.logical_queue_teardown_polling() {
                fsleep(Delta::from_millis(1));
                queue_g17p_firmware_event_worker(dev.clone());
            } else {
                requeue_g17p_firmware_event_worker_if_pending(dev.clone());
            }
        }
        G17PWorkerTailNext::Recovery => queue_g17p_recovery_worker(dev.clone()),
        G17PWorkerTailNext::TeardownPoll => {
            fsleep(Delta::from_millis(1));
            queue_g17p_firmware_event_worker(dev.clone());
        }
        G17PWorkerTailNext::Done => {}
    }
}

impl WorkItem<2> for AsahiData {
    type Pointer = ARef<AsahiDevice>;

    fn run(dev: ARef<AsahiDevice>) {
        let Some(runtime) = dev.g17_runtime.as_ref() else {
            return;
        };
        if let Err(error) = runtime.service_primary_recovery() {
            dev_err!(
                dev.as_ref(),
                "G17P recovery worker: serialized recovery failed ({:?})\n",
                error
            );
            return;
        }
        if runtime.primary_firmware_event_pending() {
            requeue_g17p_firmware_event_worker_if_pending(dev);
        } else if runtime.primary_recovery_pending() {
            queue_g17p_recovery_worker(dev);
        }
    }
}

/// All entries have their exact firmware release acknowledgement before
/// entering this queue. No runtime, queue-registry or reclaim-queue mutex is
/// held while the VM aliases and backing are destroyed.
fn reclaim_g17p_graphs(retired: KVec<g17_manager::G17PLogicalQueueTeardown>) {
    for teardown in retired.iter() {
        teardown.release_retired_vm_mappings();
    }
    drop(retired);
}

fn drain_g17p_reclaims(dev: &AsahiDevice) {
    let batches = core::mem::take(&mut *dev.g17_reclaims.lock());
    for batch in batches {
        reclaim_g17p_graphs(batch);
    }
}

impl WorkItem<3> for AsahiData {
    type Pointer = ARef<AsahiDevice>;

    fn run(dev: ARef<AsahiDevice>) {
        drain_g17p_reclaims(&dev);
    }
}

impl WorkItem<4> for AsahiData {
    type Pointer = ARef<AsahiDevice>;

    fn run(dev: ARef<AsahiDevice>) {
        let Some(runtime) = dev.g17_runtime.as_ref() else { return; };
        if let Err(error) = runtime.service_render_grows() {
            runtime.mutex().lock().note_render_observation_error(error);
            queue_g17p_recovery_worker(dev);
        }
    }
}

/// v97d: 3D stall watch poll. Re-arms itself (under the Runtime mutex and
/// the armed check) while render work is outstanding and bit4 is set.
impl WorkItem<7> for AsahiData {
    type Pointer = ARef<AsahiDevice>;

    fn run(dev: ARef<AsahiDevice>) {
        let Some(runtime) = dev.g17_runtime.clone() else { return; };
        let mut guard = runtime.mutex().lock();
        if let Some(ms) = guard.service_render_stall_watch() {
            if guard.firmware_event_worker_armed() {
                let _ = workqueue::system_highpri().enqueue_delayed::<ARef<AsahiDevice>, 7>(
                    dev.clone(), kernel::time::msecs_to_jiffies(ms.min(1000) as u32));
            }
        }
        drop(guard);
    }
}

/// Start the stall watch; already pending is fine (that run re-arms).
pub(crate) fn arm_g17p_render_stall_watch(dev: ARef<AsahiDevice>, ms: u64) {
    let _ = workqueue::system_highpri().enqueue_delayed::<ARef<AsahiDevice>, 7>(
        dev, kernel::time::msecs_to_jiffies(ms.min(1000) as u32));
}

pub(crate) fn queue_g17p_tvb_grow_worker(dev: ARef<AsahiDevice>) {
    let _ = workqueue::system_unbound().enqueue::<ARef<AsahiDevice>, 4>(dev);
}

impl WorkItem<5> for AsahiData {
    type Pointer = ARef<AsahiDevice>;

    fn run(dev: ARef<AsahiDevice>) {
        let Some(runtime) = dev.g17_runtime.clone() else { return; };
        service_g17p_dvfs_parity(dev, &runtime);
    }
}

/// Evaluate the work-tied effort and arm its idle re-check. The lock is only
/// taken when the parity effort is enabled or still published. Enqueue happens
/// under the runtime mutex after the same armed check the event worker's
/// self-requeue uses; unbind disables this work after disarming.
fn service_g17p_dvfs_parity(dev: ARef<AsahiDevice>, runtime: &g17_drm::G17PRuntimeRef) {
    use core::sync::atomic::Ordering;
    if !crate::g17_dvfs_parity::enabled(crate::g17_dvfs_policy::PARITY_CE)
        && crate::g17_dvfs_parity::CE_LIVE.load(Ordering::Relaxed) == 0
    {
        return;
    }
    let mut guard = runtime.mutex().lock();
    if !guard.firmware_event_worker_armed() {
        return;
    }
    if let Some(delay) = guard.dvfs_parity_idle_check() {
        let ms = crate::g17_dvfs_policy::recheck_ms(delay);
        crate::g17_dvfs_parity::RECHECKS.fetch_add(1, Ordering::Relaxed);
        // Already pending is fine: that run re-evaluates and re-arms.
        let _ = workqueue::system().enqueue_delayed::<ARef<AsahiDevice>, 5>(
            dev, kernel::time::msecs_to_jiffies(ms.min(1000) as u32));
    }
    drop(guard);
}

/// v84: the GPU-feed CPU performance actuator. Sleeps while its request is
/// held (it polls the linger deadline), so it runs on the unbound workqueue.
impl WorkItem<6> for AsahiData {
    type Pointer = ARef<AsahiDevice>;

    fn run(_dev: ARef<AsahiDevice>) {
        crate::g17_host_path::run_actuator();
    }
}

/// Only the firmware-event worker produces batches. Unbind joins that
/// producer before cancelling this worker and draining any cancelled batch.
fn queue_g17p_retired_graphs(
    dev: ARef<AsahiDevice>,
    retired: KVec<g17_manager::G17PLogicalQueueTeardown>,
) {
    if retired.is_empty() { return; }
    let rejected = {
        let mut pending = dev.g17_reclaims.lock();
        if pending.reserve(1, GFP_KERNEL).is_err() {
            Some(retired)
        } else {
            pending.push_within_capacity(retired).err().map(|error| error.0)
        }
    };
    if let Some(retired) = rejected {
        // Memory pressure must neither leak acknowledged owners nor drop
        // them under the queue mutex. Only this allocation failure falls
        // back to synchronous cleanup outside every shared driver lock.
        reclaim_g17p_graphs(retired);
    } else {
        let _ = workqueue::system_unbound().enqueue::<ARef<AsahiDevice>, 3>(dev);
    }
}

pub(crate) fn queue_g17p_firmware_event_worker(dev: ARef<AsahiDevice>) {
    let _ = workqueue::system_highpri().enqueue::<ARef<AsahiDevice>, 1>(dev);
}

fn requeue_g17p_firmware_event_worker_if_pending(dev: ARef<AsahiDevice>) {
    let Some(runtime) = dev.g17_runtime.clone() else {
        return;
    };
    let runtime = runtime.mutex().lock();
    if runtime.firmware_event_worker_armed() && runtime.primary_firmware_event_pending() {
        // Hold the same mutex used by disarm before queueing. Unbind therefore
        // either observes this work in cancel_work_sync, or disarms first and
        // makes this final self-requeue impossible.
        let _ = workqueue::system_highpri().enqueue::<ARef<AsahiDevice>, 1>(dev);
    }
}

pub(crate) fn queue_g17p_recovery_worker(dev: ARef<AsahiDevice>) {
    let _ = workqueue::system_highpri().enqueue::<ARef<AsahiDevice>, 2>(dev);
}

struct G17PDriverUnbind<'a> {
    runtime: &'a g17_drm::SharedG17PRuntime,
    drm: ARef<AsahiDevice>,
}

impl g17_lifecycle::DriverUnbindQuiesce for G17PDriverUnbind<'_> {
    fn disarm_ingress(&self) {
        let preparations = {
            let mut runtime = self.runtime.lock();
            runtime.disarm_firmware_event_worker();
            runtime.preparation_control()
        };
        preparations.wait_for_preparers();
    }

    fn cancel_event_work(&self) {
        // Stop the feed-perf actuator's linger loop before joining it.
        crate::g17_host_path::begin_shutdown();
        disable_g17p_work::<1>(&self.drm);
        disable_g17p_work::<4>(&self.drm);
        // The DVFS re-check is produced by the event worker (joined above)
        // and by itself; disabling also rejects any later self-requeue.
        disable_g17p_delayed_work::<5>(&self.drm);
        disable_g17p_delayed_work::<7>(&self.drm);
        // The event worker is its only producer; it is disabled above.
        disable_g17p_work::<6>(&self.drm);
        crate::g17_host_path::shutdown();
    }

    fn cancel_recovery_work(&self) {
        disable_g17p_work::<2>(&self.drm);
    }

    fn cancel_reclaim_work(&self) {
        disable_g17p_work::<3>(&self.drm);
        // disable_work_sync may cancel a queued invocation before it takes
        // its batch. The sole producer has already been joined above.
        drain_g17p_reclaims(&self.drm);
    }
}

fn disable_g17p_work<const ID: u64>(dev: &AsahiDevice)
where
    AsahiDevice: HasWork<AsahiDevice, ID>,
{
    let dev_ptr = (dev as *const AsahiDevice).cast_mut();
    // SAFETY: `dev` remains strongly referenced by the unbind owner for this
    // whole call. The HasWork implementation returns its embedded work item,
    // and disable_work_sync waits until no callback owns that item.
    let work = unsafe {
        <AsahiDevice as HasWork<AsahiDevice, ID>>::raw_get_work(dev_ptr)
    };
    // SAFETY: `work` is the initialized embedded work item selected by ID.
    let work = unsafe { Work::raw_get(work) };
    // A late RTKit callback may have loaded event_worker_armed before unbind
    // cleared it, then try to enqueue after this join. Cancellation alone
    // permits that stale enqueue to run against the released runtime. Disable
    // the work permanently as well as joining it; removal never rearms it.
    // SAFETY: the containing device is strongly held throughout this sleepable
    // unbind call. No driver mutex needed by the worker is held here.
    unsafe { bindings::disable_work_sync(work) };
}

fn disable_g17p_delayed_work<const ID: u64>(dev: &AsahiDevice)
where
    AsahiDevice: HasDelayedWork<AsahiDevice, ID>,
{
    let dev_ptr = (dev as *const AsahiDevice).cast_mut();
    // SAFETY: as in `disable_g17p_work`; HasDelayedWork guarantees the
    // returned work_struct is the `work` field of an embedded delayed_work.
    let work = unsafe {
        <AsahiDevice as HasWork<AsahiDevice, ID>>::raw_get_work(dev_ptr)
    };
    // SAFETY: `work` is the initialized embedded work item selected by ID.
    let work = unsafe { Work::raw_get(work) };
    // SAFETY: HasDelayedWork: this work_struct is inside a delayed_work.
    let dwork = unsafe { kernel::container_of!(work, bindings::delayed_work, work) };
    // SAFETY: the device is strongly held for this sleepable unbind call; this
    // cancels a pending timer, waits for a running callback and disables it.
    unsafe { bindings::disable_delayed_work_sync(dwork) };
}

#[allow(dead_code)]
enum AsahiRuntime {
    Legacy(ARef<drm::Device<AsahiDriver>>),
    G17P(g17_drm::SharedG17PRuntime),
    M5(g17_live_boot::m5::M5Runtime),
    M5Drm(crate::m5_drm::SharedM5Runtime),
    /// T8152/G18G runtime returned by the G18 live-boot entry point.
    G18(Arc<dyn G18Runtime>),
}

/// Owner object returned by the G18 live-boot module.
///
/// The live-boot module implements this for its session type. The driver
/// keeps it for the device's lifetime and calls [`G18Runtime::shutdown`] on
/// unbind, before devres releases the SGX mapping and the mailbox links.
pub(crate) trait G18Runtime: Send + Sync {
    /// Quiesce the firmware, both ASC processors and every host mapping.
    fn shutdown(&self);
}

/// Everything the T8152 probe gate hands to the G18 live-boot entry point.
///
/// Built only by [`prepare_t8152_device`], and only when `g18_initdata_test`
/// is non-zero. Holding one means:
/// - the three GPU PSD services (sgx 3, gfx-asc 81, gfx1-asc 82) were read
///   once, read-only, and their samples are in [`T8152Probe::psd`];
/// - the SGX window is mapped in `registers`. Its ID block was read only if
///   the sgx PSD sample proved the block powered: `observed` is `Some`
///   exactly then;
/// - for the firmware stages 2..4 the loader's G18G firmware receipt matched
///   and the 42-bit DMA mask is set. Stages 1 and 90 require neither.
///
/// No firmware, ASC CPU, PSD request, DMA or UAT state has been touched.
#[allow(dead_code)]
pub(crate) struct T8152Probe {
    /// The matched SoC entry (`hw::agx3::T8152`).
    pub(crate) soc: &'static hw::agx3::SocConfig,
    /// Static identity decoded from the SoC entry (G18G, HAL400, SKSM).
    pub(crate) identity: identity::GpuIdentity,
    /// The `g18_initdata_test` stage, always non-zero here.
    pub(crate) stage: u32,
    /// Whether /chosen `aurora,g18g-gpu-firmware-profile` matched.
    pub(crate) firmware_profile_admitted: bool,
    /// Read-only samples of PSD services 3, 81 and 82, in that order.
    pub(crate) psd: [regs::T8152PsdSample; 3],
    /// SGX mapping. Every further SGX read must again follow a powered
    /// `regs::t8152_prove_sgx_powered()` result.
    pub(crate) registers: regs::Resources,
    /// Hardware identity, only when the SGX PSD proved the block powered.
    pub(crate) observed: Option<hw::GpuIdConfig>,
    /// Unregistered DRM device whose data slot is still uninitialized, as
    /// on M5. The live-boot module initializes it before any registration.
    pub(crate) drm: ARef<AsahiDevice>,
}

/// Loader receipt the G18G firmware stages require. The loader writes it
/// only after hashing both loaded TEXT images (b000 GFX 0x50000, b010 GFX1
/// 0x54000) against the 26A428 profile (plan step 0.2).
const G18G_FIRMWARE_PROFILE: &[u8] = b"G18G-RTKit-3514.0.15.release\0";

pub(crate) struct AsahiDriver {
    runtime: AsahiRuntime,
}

/// A matched device either has a complete legacy hardware configuration or is
/// an exact AGX3 identification target whose firmware boundary remains closed.
pub(crate) enum ProbeConfig {
    Supported(&'static hw::HwConfig),
    /// AGX3 (G15/G16/G17) target: chip identification and the read-only
    /// topology decode run at probe time, then the probe fails closed before
    /// DMA setup, ASC start, or any MMIO write.
    Agx3Diagnostic(&'static hw::agx3::SocConfig),
}

unsafe impl Send for AsahiDriver {}
unsafe impl Sync for AsahiDriver {}

/// Convenience type alias for the DRM device type for this driver.
pub(crate) type AsahiDevice = drm::device::Device<AsahiDriver>;
pub(crate) type AsahiDevRef = ARef<AsahiDevice>;

/// DRM Driver metadata
const INFO: drm::driver::DriverInfo = drm::driver::DriverInfo {
    major: 0,
    minor: 2,
    patchlevel: 0,
    name: c_str!("asahi"),
    desc: c_str!("Apple AGX Graphics"),
};

/// DRM Driver implementation for `AsahiDriver`.
#[vtable]
impl drm::driver::Driver for AsahiDriver {
    /// Our `DeviceData` type, reference-counted
    type Data = AsahiData;
    /// Our `File` type.
    type File = file::File;
    /// Our `Object` type.
    type Object = drm::gem::shmem::Object<AsahiObject>;

    const INFO: drm::driver::DriverInfo = INFO;
    const FEATURES: u32 = drm::driver::FEAT_GEM
        | drm::driver::FEAT_RENDER
        | drm::driver::FEAT_SYNCOBJ
        | drm::driver::FEAT_SYNCOBJ_TIMELINE
        | drm::driver::FEAT_GEM_GPUVA;

    kernel::declare_drm_ioctls! {
        (ASAHI_GET_PARAMS,      drm_asahi_get_params,
                          ioctl::RENDER_ALLOW, crate::file::File::get_params),
        (ASAHI_GET_TIME,        drm_asahi_get_time,
            ioctl::AUTH | ioctl::RENDER_ALLOW, crate::file::File::get_time),
        (ASAHI_VM_CREATE,       drm_asahi_vm_create,
            ioctl::AUTH | ioctl::RENDER_ALLOW, crate::file::File::vm_create),
        (ASAHI_VM_DESTROY,      drm_asahi_vm_destroy,
            ioctl::AUTH | ioctl::RENDER_ALLOW, crate::file::File::vm_destroy),
        (ASAHI_VM_BIND,         drm_asahi_vm_bind,
            ioctl::AUTH | ioctl::RENDER_ALLOW, crate::file::File::vm_bind),
        (ASAHI_GEM_CREATE,      drm_asahi_gem_create,
            ioctl::AUTH | ioctl::RENDER_ALLOW, crate::file::File::gem_create),
        (ASAHI_GEM_MMAP_OFFSET, drm_asahi_gem_mmap_offset,
            ioctl::AUTH | ioctl::RENDER_ALLOW, crate::file::File::gem_mmap_offset),
        (ASAHI_GEM_BIND_OBJECT, drm_asahi_gem_bind_object,
            ioctl::AUTH | ioctl::RENDER_ALLOW, crate::file::File::gem_bind_object),
        (ASAHI_QUEUE_CREATE,    drm_asahi_queue_create,
            ioctl::AUTH | ioctl::RENDER_ALLOW, crate::file::File::queue_create),
        (ASAHI_QUEUE_DESTROY,   drm_asahi_queue_destroy,
            ioctl::AUTH | ioctl::RENDER_ALLOW, crate::file::File::queue_destroy),
        (ASAHI_SUBMIT,          drm_asahi_submit,
            ioctl::AUTH | ioctl::RENDER_ALLOW, crate::file::File::submit),
    }
}

// OF Device ID table.s
kernel::of_device_table!(
    OF_TABLE,
    <AsahiDriver as platform::Driver>::IdInfo,
    [
        (
            of::DeviceId::new(c_str!("apple,agx-t8103")),
            ProbeConfig::Supported(&hw::t8103::HWCONFIG)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t8112")),
            ProbeConfig::Supported(&hw::t8112::HWCONFIG)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t6000")),
            ProbeConfig::Supported(&hw::t600x::HWCONFIG_T6000)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t6001")),
            ProbeConfig::Supported(&hw::t600x::HWCONFIG_T6001)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t6002")),
            ProbeConfig::Supported(&hw::t600x::HWCONFIG_T6002)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t6020")),
            ProbeConfig::Supported(&hw::t602x::HWCONFIG_T6020)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t6021")),
            ProbeConfig::Supported(&hw::t602x::HWCONFIG_T6021)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t6022")),
            ProbeConfig::Supported(&hw::t602x::HWCONFIG_T6022)
        ),
        // AGX3 (G15/G16/G17) identification targets. These intentionally
        // select no HwConfig: unproved power/MMIO/firmware-tuning fields
        // must never be represented by copied or zero-filled legacy values,
        // and the probe fails closed before any firmware handoff.
        (
            of::DeviceId::new(c_str!("apple,agx-t8122")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T8122)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t6030")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T6030)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t6031")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T6031)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t6032")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T6032)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t8132")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T8132)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t6040")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T6040)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t6041")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T6041)
        ),
        // A18 Pro / D93AP. Reserve both the Linux-style handoff compatible and
        // the raw ADT-compatible form as diagnostic-only until the complete
        // G17P firmware lifecycle is proven on the physical J700 target.
        (
            of::DeviceId::new(c_str!("apple,agx-t8140")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T8140)
        ),
        (
            of::DeviceId::new(c_str!("gpu,t8140")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T8140)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t8142")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T8142)
        ),
        // M6 / J873. Refuses with ENODEV unless g18_initdata_test is set.
        (
            of::DeviceId::new(c_str!("apple,agx-t8152")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T8152)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t6050")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T6050)
        ),
        (
            of::DeviceId::new(c_str!("apple,agx-t6051")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T6051)
        ),
        // The literal compatible captured from the J714s/T6050 ADT.
        (
            of::DeviceId::new(c_str!("gpu,t6050")),
            ProbeConfig::Agx3Diagnostic(&hw::agx3::T6050)
        ),
    ]
);

/// Start the persistent runtime for an exact T8140/G17P match.
fn probe_t8140_runtime(
    pdev: &platform::Device<Core<'_>>,
    soc: &'static hw::agx3::SocConfig,
) -> Result<g17_drm::SharedG17PRuntime> {
    let expected = identity::decode_gpu_identity(soc.hw_family, soc.hw_variant, soc.num_dies as u8)
        .ok_or(EINVAL)?;
    if soc.chip_id != 0x8140
        || expected.gpu_gen != hw::GpuGen::G17
        || expected.gpu_variant != identity::GpuVariant::P
        || expected.uat_input_address_bits != soc.uat_ias
    {
        return Err(EINVAL);
    }

    // Topology is read and admitted before mappings, DMA setup, or provider
    // start. The token is consumed by the retained runtime constructor.
    let topology = admit_t8140_live_topology(pdev, &expected)?;
    let registers = regs::Resources::new(pdev)?;
    let observed = registers.get_gpu_id()?;
    if observed.gpu_gen != soc.gpu_gen
        || observed.gpu_variant != soc.gpu_variant
        || observed.num_dies != soc.num_dies
    {
        dev_err!(
            pdev.as_ref(),
            "G17P: hardware identity {:?}{:?}/{} dies does not match exact T8140 {:?}{:?}/{} dies\n",
            observed.gpu_gen,
            observed.gpu_variant,
            observed.num_dies,
            soc.gpu_gen,
            soc.gpu_variant,
            soc.num_dies
        );
        return Err(ENODEV);
    }

    unsafe { pdev.dma_set_mask_and_coherent(DmaMask::try_new(soc.uat_oas)?)? };

    // The retained manager allocates its GEM objects from this DRM device.
    // Registration follows runtime and adapter initialization below.
    let uninit = unsafe {
        pin_init::pin_init_from_closure::<AsahiData, kernel::error::Error>(|_slot| Ok(()))
    };
    let drm: ARef<AsahiDevice> = drm::device::Device::new(pdev.as_ref(), uninit)?;

    // v94 boot-only `g17p_gem_huge`: back every GEM object created from here
    // on with a dedicated huge tmpfs mount (1 = "within_size", 2 = "always"),
    // as panfrost/panthor/v3d/i915 do. Must precede the first GEM object.
    let huge = crate::module_parameters::g17p_gem_huge.value();
    #[cfg(CONFIG_TRANSPARENT_HUGEPAGE)]
    if huge != 0 {
        let value = if huge == 2 { c"always" } else { c"within_size" };
        // `drm::Device` is `repr(C)` with the `drm_device` as its first field.
        let raw = (&*drm as *const AsahiDevice).cast_mut().cast::<bindings::drm_device>();
        // SAFETY: `raw` is this live, unregistered DRM device; no GEM object
        // exists yet and the mount is released by a drmm action.
        let ret = unsafe { bindings::drm_gem_huge_mnt_create(raw, value.as_ptr()) };
        if ret == 0 {
            crate::g17_mem_path::HUGE_MOUNT.store(huge, core::sync::atomic::Ordering::Relaxed);
            let mode = if huge == 2 { "always" } else { "within_size" };
            dev_info!(pdev.as_ref(), "G17P GEM: huge tmpfs mount ({}) for GEM backing\n", mode);
        } else {
            dev_warn!(pdev.as_ref(), "G17P GEM: huge tmpfs mount failed ({}); using shm_mnt\n", ret);
        }
    }

    #[cfg(not(CONFIG_TRANSPARENT_HUGEPAGE))]
    if huge != 0 {
        dev_warn!(pdev.as_ref(), "G17P GEM: huge pages unavailable; using shm_mnt\n");
    }

    let runtime = g17_live_boot::G17PLiveRuntime::start(
        pdev,
        drm.clone(),
        registers,
        soc,
        &expected,
        &observed,
        topology,
    )?;
    let runtime = Arc::pin_init(new_mutex!(runtime), GFP_KERNEL)?;
    let mut data_initialized = false;
    let registration = (|| -> Result {
        let runtime_ref = g17_drm::G17PRuntimeRef::new(&runtime);
        let gpu: Arc<dyn drm_gpu::DrmGpu> = Arc::new(
            g17_drm::G17PDrmGpu::new(runtime_ref.clone(), soc, observed)?,
            GFP_KERNEL,
        )?;
        let data = try_pin_init!(AsahiData {
            gpu,
            pdev: pdev.into(),
            resources: None,
            g17_runtime: Some(runtime_ref),
            g17_event_work <- new_work!("AsahiData::g17_event_work"),
            g17_recovery_work <- new_work!("AsahiData::g17_recovery_work"),
            g17_reclaim_work <- new_work!("AsahiData::g17_reclaim_work"),
            g17_tvb_grow_work <- new_work!("AsahiData::g17_tvb_grow_work"),
            g17_dvfs_work <- new_delayed_work!("AsahiData::g17_dvfs_work"),
            g17_feed_perf_work <- new_work!("AsahiData::g17_feed_perf_work"),
            g17_stall_work <- new_delayed_work!("AsahiData::g17_stall_work"),
            g17_reclaims <- new_mutex!(KVec::new(), "AsahiData::g17_reclaims"),
        });
        let ptr: *const AsahiData = &raw const **drm;
        // SAFETY: `drm` was created with an uninitialized data slot above; this
        // is its only initialization before registration exposes file operations.
        unsafe {
            data.__init(ptr as *mut AsahiData)?;
        }
        data_initialized = true;
        runtime.lock().arm_primary_firmware_event_worker()?;
        runtime.lock().mark_firmware_cache_flush_ready()?;
        (*drm).gpu.init()?;
        drm::driver::Registration::new_foreign_owned(&drm, pdev.as_ref(), 0)?;
        Ok(())
    })();
    if let Err(error) = registration {
        // Once data is initialized it owns strong runtime handles, including
        // from workers armed before registration. Join them before breaking
        // the device/runtime cycle, even when registration itself failed.
        if data_initialized {
            g17_lifecycle::quiesce_driver_unbind(&G17PDriverUnbind {
                runtime: &runtime, drm: drm.clone(),
            });
        }
        runtime.lock().shutdown();
        return Err(error);
    }
    Ok(runtime)
}

fn prepare_t8142_device(pdev: &platform::Device<Core<'_>>)
    -> Result<(ARef<AsahiDevice>, regs::Resources, hw::GpuIdConfig)> {
    // The RAM loader supplies this receipt only after checking both loaded
    // TEXT images against the exact M5 profile, including all instructions.
    let chosen = of::chosen().ok_or(ENODEV)?;
    let profile: KVec<u8> = chosen.get_property(c_str!("aurora,m5-gpu-firmware-profile"))?;
    if profile.as_slice() != b"M5-RTKit-3255.160.4.release\0" {
        return Err(ENODEV);
    }
    let registers = regs::Resources::new(pdev)?;
    let observed = registers.get_gpu_id()?;
    if observed.gpu_gen != hw::GpuGen::G17 || observed.gpu_variant != hw::GpuVariant::G
        // M5 GPUs ship with 10 cores, or 8 with two fused off (MacBook Air).
        || observed.num_dies != 1 || !matches!(observed.total_active_cores, 8 | 10)
    {
        return Err(ENODEV);
    }
    unsafe { pdev.dma_set_mask_and_coherent(DmaMask::try_new(42)?)? };
    // This device is deliberately unregistered. The DRM release path does
    // not drop its uninitialized data slot; no callback reads that slot.
    let uninit = unsafe {
        pin_init::pin_init_from_closure::<AsahiData, kernel::error::Error>(|_slot| Ok(()))
    };
    let drm: ARef<AsahiDevice> = drm::device::Device::new(pdev.as_ref(), uninit)?;
    Ok((drm, registers, observed))
}

/// T8152 probe gate. With `g18_initdata_test=0` (the default) this is a
/// complete no-op: it logs and refuses with ENODEV before any MMIO, DMA
/// mask, reserved-memory, mailbox or processor access, just as M5 refuses by
/// default. Unlike the M5 default it also skips the read-only SGX identity
/// probe, because J873 cold-boots with the GPU PSD services idle, where an
/// SGX read can raise an SError.
fn probe_t8152(
    pdev: &platform::Device<Core<'_>>,
    soc: &'static hw::agx3::SocConfig,
) -> Result<Arc<dyn G18Runtime>> {
    let stage = crate::module_parameters::g18_initdata_test.value();
    if stage == 0 {
        dev_info!(
            pdev.as_ref(),
            "G18G: T8152 matched; g18_initdata_test=0 refuses before any MMIO, DMA, mailbox or processor access\n"
        );
        return Err(ENODEV);
    }
    if !g18_live_boot::stage_is_known(stage) {
        dev_err!(
            pdev.as_ref(),
            "G18G: g18_initdata_test={} is not a known stage (1-9, 90-93); refused before any MMIO\n",
            stage
        );
        return Err(EINVAL);
    }
    let variant = crate::module_parameters::g18_compute_variant.value();
    if variant > 2 || (variant != 0 && stage != g18_live_boot::STAGE_COMPUTE_ADD) {
        dev_err!(
            pdev.as_ref(),
            "G18G: g18_compute_variant={} needs 0, or 1/2 with g18_initdata_test=8; refused before any MMIO\n",
            variant
        );
        return Err(EINVAL);
    }
    let render_variant = crate::module_parameters::g18_render_variant.value();
    let render_profile = crate::module_parameters::g18_render_profile.value();
    let sma_minimum = crate::module_parameters::g18_sma_minimum.value();
    let sma_ideal = crate::module_parameters::g18_sma_ideal.value();
    let native_barrier = crate::module_parameters::g18_native_barrier.value();
    if native_barrier > 1 || (native_barrier != 0 && (sma_ideal != 1 || sma_minimum != 0 ||
        stage != crate::g18_render_stage::STAGE_RENDER || render_variant != 0 || render_profile != 4)) {
        dev_err!(pdev.as_ref(), "G18G: native barrier probe requires ideal SMA stage9 variant0 profile4; refused before MMIO\n");
        return Err(EINVAL);
    }
    if sma_ideal > 1 || (sma_ideal != 0 && (sma_minimum != 0 ||
        stage != crate::g18_render_stage::STAGE_RENDER || render_variant != 0 || render_profile != 4)) {
        dev_err!(pdev.as_ref(), "G18G: SMA ideal probe requires stage9 variant0 profile4 and minimum=0; refused before MMIO\n");
        return Err(EINVAL);
    }
    // Explicit fixed-minimum comparison for the retained first application
    // draw, as well as the original H1 witness. This is not dynamic sizing.
    let minimum_probe = render_variant == 0 &&
        ((stage == crate::g18_render_stage::STAGE_RENDER && render_profile == 4) ||
         (stage == g18_live_boot::STAGE_DRM && render_profile == 1));
    let stall_trace = crate::module_parameters::g18_graphics_stall_trace.value();
    if stall_trace > 1 || (stall_trace != 0 && !minimum_probe) {
        dev_err!(pdev.as_ref(), "G18G: graphics stall tracing requires stage9/profile4 or stage10/profile1, variant0; refused before MMIO\n");
        return Err(EINVAL);
    }
    let high_vdm = crate::module_parameters::g18_vdm_high_alias.value();
    if high_vdm > 1 || (high_vdm != 0 &&
        (stage != crate::g18_render_stage::STAGE_RENDER || render_variant != 0 ||
         render_profile != 4 || sma_minimum != 1 || sma_ideal != 0 || stall_trace != 1)) {
        dev_err!(pdev.as_ref(), "G18G: high VDM alias requires stage9/profile4/variant0, minimum SMA and stall tracing; refused before MMIO\n");
        return Err(EINVAL);
    }
    let high_ppp = crate::module_parameters::g18_ppp_high_alias.value();
    if high_ppp > 1 || (high_ppp != 0 && high_vdm != 1) {
        dev_err!(pdev.as_ref(), "G18G: high PPP aliases require the bounded high VDM control; refused before MMIO\n");
        return Err(EINVAL);
    }
    if sma_minimum > 1 || (sma_minimum != 0 && !minimum_probe) {
        dev_err!(pdev.as_ref(), "G18G: SMA minimum probe requires stage9/profile4 or stage10/profile1, variant0; refused before MMIO\n");
        return Err(EINVAL);
    }
    let usc_statistics = crate::module_parameters::g18_usc_statistics.value();
    if usc_statistics > 1 || (usc_statistics != 0 &&
        (stage != crate::g18_render_stage::STAGE_RENDER || render_variant != 0)) {
        dev_err!(pdev.as_ref(), "G18G: USC statistics require the stage9 colour fixture; refused before MMIO\n");
        return Err(EINVAL);
    }
    if stage == crate::g18_render_stage::STAGE_RENDER {
        crate::g18_render_stage::admit(pdev.as_ref(), render_variant, render_profile)?;
    } else if render_variant != 0 || render_profile != 1 {
        return Err(EINVAL);
    }
    let admit_msaa4 = crate::module_parameters::g18_admit_msaa4.value();
    if admit_msaa4 > 1 || (admit_msaa4 != 0 && stage != g18_live_boot::STAGE_DRM) {
        dev_err!(pdev.as_ref(), "G18G: g18_admit_msaa4={} needs 0, or 1 with g18_initdata_test=10; refused before MMIO\n", admit_msaa4);
        return Err(EINVAL);
    }
    if stage == g18_live_boot::STAGE_DRM {
        if crate::module_parameters::g18_admit_compute.value() > 1 {return Err(EINVAL);}
        return probe_t8152_drm(pdev, soc, stage);
    }
    let probe = prepare_t8152_device(pdev, soc, stage)?;
    g18_live_boot_start(pdev, probe)
}

/// Register the G18 DRM render node over a started G18 session.
fn probe_t8152_drm(
    pdev: &platform::Device<Core<'_>>,
    soc: &'static hw::agx3::SocConfig,
    stage: u32,
) -> Result<Arc<dyn G18Runtime>> {
    let mut probe = prepare_t8152_device(pdev, soc, stage)?;
    let drm = probe.drm.clone();
    let observed = probe.observed.take();
    let (owner, runtime) = g18_live_boot::start_drm(pdev, probe)?;
    let config = crate::g18_drm::G18DrmConfig {
        admit_compute: crate::module_parameters::g18_admit_compute.value() != 0,
        admit_msaa4: crate::module_parameters::g18_admit_msaa4.value() != 0,
        admit_graphics_scratch:
            crate::module_parameters::g18_admit_graphics_scratch.value() != 0,
    };
    let gpu: Arc<dyn drm_gpu::DrmGpu> = Arc::new(
        crate::g18_drm::G18DrmGpu::new(runtime, observed.as_ref(), config)?,
        GFP_KERNEL,
    )?;
    let data = try_pin_init!(AsahiData {
        gpu, pdev: pdev.into(), resources: None, g17_runtime: None,
        g17_event_work <- new_work!("AsahiData::g17_event_work"),
        g17_recovery_work <- new_work!("AsahiData::g17_recovery_work"),
        g17_reclaim_work <- new_work!("AsahiData::g17_reclaim_work"),
        g17_tvb_grow_work <- new_work!("AsahiData::g17_tvb_grow_work"),
        g17_dvfs_work <- new_delayed_work!("AsahiData::g17_dvfs_work"),
        g17_feed_perf_work <- new_work!("AsahiData::g17_feed_perf_work"),
        g17_stall_work <- new_delayed_work!("AsahiData::g17_stall_work"),
        g17_reclaims <- new_mutex!(KVec::new(), "AsahiData::g17_reclaims"),
    });
    let ptr: *const AsahiData = &raw const **drm;
    // SAFETY: prepare_t8152_device leaves this data slot uninitialized. This
    // is its sole initialization, before registration exposes file operations.
    unsafe { data.__init(ptr as *mut AsahiData)?; }
    (*drm).gpu.init()?;
    drm::driver::Registration::new_foreign_owned(&drm, pdev.as_ref(), 0)?;
    dev_info!(pdev.as_ref(), "G18_DRM registered render node (asahi, apple,agx-t8152)\n");
    Ok(owner)
}

fn prepare_t8152_device(
    pdev: &platform::Device<Core<'_>>,
    soc: &'static hw::agx3::SocConfig,
    stage: u32,
) -> Result<T8152Probe> {
    let identity =
        identity::decode_gpu_identity(soc.hw_family, soc.hw_variant, soc.num_dies as u8)
            .ok_or(EINVAL)?;
    if soc.chip_id != 0x8152
        || identity.gpu_gen != hw::GpuGen::G18
        || identity.gpu_variant != identity::GpuVariant::G
        || identity.uat_input_address_bits != soc.uat_ias
    {
        return Err(EINVAL);
    }
    dev_info!(
        pdev.as_ref(),
        "G18G: T8152 stage {}: {:?}{:?} USC gen {} HAL {:?} {:?} roles {:?} submission, {}-bit UAT roots\n",
        stage,
        identity.gpu_gen,
        identity.gpu_variant,
        identity.usc_generation,
        identity.gpu_hal_generation,
        identity.firmware_roles,
        identity.submission_transport,
        soc.uat_ias
    );

    let firmware_profile_admitted = match of::chosen() {
        Some(chosen) => {
            let profile: Result<KVec<u8>> =
                chosen.get_property(c_str!("aurora,g18g-gpu-firmware-profile"));
            matches!(profile, Ok(profile) if profile.as_slice() == G18G_FIRMWARE_PROFILE)
        }
        None => false,
    };
    dev_info!(
        pdev.as_ref(),
        "G18G: firmware receipt {}\n",
        (if firmware_profile_admitted { "admitted" } else { "absent or different" })
    );
    if g18_live_boot::stage_runs_firmware(stage) && !firmware_profile_admitted {
        dev_err!(
            pdev.as_ref(),
            "G18G: stage {} needs /chosen aurora,g18g-gpu-firmware-profile = G18G-RTKit-3514.0.15.release\n",
            stage
        );
        return Err(ENODEV);
    }

    // PSD state first, read-only. Nothing below reads SGX unless the sgx
    // service proves the block active and acknowledged.
    let mut psd = [regs::T8152PsdSample {
        service: regs::T8152PsdService::Sgx,
        state: 0,
        ack: 0,
    }; 3];
    for (slot, service) in psd.iter_mut().zip(regs::T8152PsdService::ALL) {
        *slot = regs::t8152_read_psd(pdev, service)?;
        dev_info!(
            pdev.as_ref(),
            "G18G: PSD service {} ({:#x}) state={:#x} power state {} ack={:#x} -> {}\n",
            service.service_id(),
            service.physical_base(),
            slot.state,
            slot.power_state(),
            slot.ack,
            (if slot.powered() { "powered" } else { "not powered" })
        );
    }

    let registers = regs::Resources::new(pdev)?;
    let observed = match regs::t8152_prove_sgx_powered(pdev) {
        Ok(powered) => Some(registers.get_gpu_id_t8152(&powered, soc)?),
        Err(error) if error == regs::t8152_not_powered() => {
            dev_info!(
                pdev.as_ref(),
                "G18G: SGX identity not read (sgx PSD idle); identified by chip id {:#x} and compatible\n",
                soc.chip_id
            );
            None
        }
        Err(error) => return Err(error),
    };

    if g18_live_boot::stage_runs_firmware(stage) {
        // J873 DRAM ends at 0x106_0000_0000, inside a 42-bit output space.
        unsafe { pdev.dma_set_mask_and_coherent(DmaMask::try_new(soc.uat_oas)?)? };
    }

    // This device is deliberately unregistered. The DRM release path does
    // not drop its uninitialized data slot; no callback reads that slot.
    let uninit = unsafe {
        pin_init::pin_init_from_closure::<AsahiData, kernel::error::Error>(|_slot| Ok(()))
    };
    let drm: ARef<AsahiDevice> = drm::device::Device::new(pdev.as_ref(), uninit)?;
    Ok(T8152Probe {
        soc,
        identity,
        stage,
        firmware_profile_admitted,
        psd,
        registers,
        observed,
        drm,
    })
}

/// G18 live-boot entry point: the staged bring-up in `g18_live_boot.rs`.
///
/// `probe.stage` is `g18_initdata_test`, already checked by
/// [`g18_live_boot::stage_is_known`]:
/// - 1: read-only probe (PSD, SGX ID, wrapper CPU state, post-office
///   status), then ENODEV;
/// - 2, 3, 4, 5, 6 (REQUIRE USER APPROVAL, plus the mailbox provider's own
///   `apple_mailbox.ascwrap_v8_cpu_control=1`): ASC CPU start and RTKit,
///   init-data ACKs, openings + idle wake + heartbeat. Stages 2 and 3 end in
///   ENODEV with every firmware owner retained; stages 4/5/6 return the owner.
///   Stage 5 registers one empty queue; stage 6 binds its scheduler. No GPU jobs;
/// - 90 (REQUIRES USER APPROVAL): PMGR2 PSD power-on only, then ENODEV.
///
/// On success the driver keeps the owner and calls
/// [`G18Runtime::shutdown`] on unbind.
fn g18_live_boot_start(
    pdev: &platform::Device<Core<'_>>,
    probe: T8152Probe,
) -> Result<Arc<dyn G18Runtime>> {
    g18_live_boot::start(pdev, probe)
}

fn probe_t8142_initdata(pdev: &platform::Device<Core<'_>>) -> Result<g17_live_boot::m5::M5Runtime> {
    let (drm, registers, _) = prepare_t8142_device(pdev)?;
    if crate::module_parameters::m5_initdata_test.value() == 13 {
        g17_live_boot::m5::M5Runtime::start_application(pdev, drm, registers)
    } else {
        g17_live_boot::m5::M5Runtime::start(pdev, drm, registers)
    }
}

fn probe_t8142_runtime(pdev: &platform::Device<Core<'_>>) -> Result<crate::m5_drm::SharedM5Runtime> {
    let (drm, registers, observed) = prepare_t8142_device(pdev)?;
    // Before the session: a CLPC that has not probed yet defers the GPU.
    let clpc = crate::m5_drm::M5ClpcInput::new(pdev.as_ref())?;
    let runtime = g17_live_boot::m5::M5Runtime::start_application(pdev, drm.clone(), registers)?;
    let runtime = Arc::pin_init(new_mutex!(runtime), GFP_KERNEL)?;
    let gpu: Arc<dyn drm_gpu::DrmGpu> = Arc::new(
        crate::m5_drm::M5DrmGpu::new(runtime.clone(), observed, clpc), GFP_KERNEL)?;
    let data = try_pin_init!(AsahiData {
        gpu, pdev: pdev.into(), resources: None, g17_runtime: None,
        g17_event_work <- new_work!("AsahiData::g17_event_work"),
        g17_recovery_work <- new_work!("AsahiData::g17_recovery_work"),
        g17_reclaim_work <- new_work!("AsahiData::g17_reclaim_work"),
        g17_tvb_grow_work <- new_work!("AsahiData::g17_tvb_grow_work"),
        g17_dvfs_work <- new_delayed_work!("AsahiData::g17_dvfs_work"),
        g17_feed_perf_work <- new_work!("AsahiData::g17_feed_perf_work"),
        g17_stall_work <- new_delayed_work!("AsahiData::g17_stall_work"),
        g17_reclaims <- new_mutex!(KVec::new(), "AsahiData::g17_reclaims"),
    });
    let ptr: *const AsahiData = &raw const **drm;
    // SAFETY: prepare_t8142_device leaves this data slot uninitialized. This
    // is its sole initialization, before registration exposes file operations.
    unsafe { data.__init(ptr as *mut AsahiData)?; }
    (*drm).gpu.init()?;
    drm::driver::Registration::new_foreign_owned(&drm, pdev.as_ref(), 0)?;
    dev_info!(pdev.as_ref(),
        "M5_DRM registered experimental first-job render adapter; general Mesa rendering unqualified\n");
    Ok(runtime)
}

/// Identify an AGX3 target as far as read-only evidence allows, then refuse.
///
/// This logs the statically decoded identity, performs the read-only SGX
/// identification (ID block + core-enable masks; no DMA mask, no ASC start,
/// no MMIO write), cross-checks the two, and stops at the generation-specific
/// firmware boundary.
fn refuse_agx3_probe(pdev: &platform::Device<Core<'_>>, soc: &'static hw::agx3::SocConfig) -> Error {
    let Some(expected) =
        identity::decode_gpu_identity(soc.hw_family, soc.hw_variant, soc.num_dies as u8)
    else {
        dev_err!(
            pdev.as_ref(),
            "AGX3: static identity for chip {:#x} does not decode\n",
            soc.chip_id
        );
        return EINVAL;
    };

    if expected.uat_input_address_bits != soc.uat_ias {
        dev_err!(
            pdev.as_ref(),
            "AGX3: static UAT width for chip {:#x} disagrees with the identity decode\n",
            soc.chip_id
        );
        return EINVAL;
    }

    dev_info!(
        pdev.as_ref(),
        "AGX3: matched {:?}{:?} (chip {:#x}, USC gen {}, HAL {:?}, {:?} firmware roles, {:?} submission, {}-bit UAT roots, {}-bit OAS)\n",
        expected.gpu_gen,
        expected.gpu_variant,
        soc.chip_id,
        expected.usc_generation,
        expected.gpu_hal_generation,
        expected.firmware_roles,
        expected.submission_transport,
        soc.uat_ias,
        soc.uat_oas
    );

    // The G17S loaded-firmware handoff is reserved memory, not GPU MMIO. It
    // must be mapped and hashed before touching the SGX register aperture so
    // firmware identity can never be inferred from a powered or partially
    // initialized GPU. Other G17 variants never consume these G17S carveouts.
    // This token grants identity only.
    let loaded_g17_firmware = if expected.gpu_gen == hw::GpuGen::G17
        && expected.gpu_variant == identity::GpuVariant::S
    {
        match g17_firmware::admit_g17_loaded_pair_from_handoff(expected.gpu_variant as u32 as u8) {
            Ok(admission) => {
                dev_info!(
                    pdev.as_ref(),
                    "G17: exact versioned GFX/GFX1 loaded-segment identity admitted before SGX MMIO\n"
                );
                Some(admission)
            }
            Err(e) => {
                dev_err!(
                    pdev.as_ref(),
                    "G17S: loaded-segment firmware identity was invalid; stopping before SGX MMIO: {:?}\n",
                    e
                );
                return ENODEV;
            }
        }
    } else {
        None
    };

    match regs::Resources::new(pdev) {
        Ok(res) => match res.get_gpu_id() {
            Ok(id) => {
                dev_info!(
                    pdev.as_ref(),
                    "AGX3: hardware reports {:?}{:?} rev {:?}, {} dies, {} clusters, {} cores/cluster, {} active cores\n",
                    id.gpu_gen,
                    id.gpu_variant,
                    id.gpu_rev,
                    id.num_dies,
                    id.num_clusters,
                    id.num_cores,
                    id.total_active_cores
                );
                if id.gpu_gen != soc.gpu_gen || id.gpu_variant != soc.gpu_variant {
                    dev_warn!(
                        pdev.as_ref(),
                        "AGX3: hardware identity {:?}{:?} does not match the matched compatible ({:?}{:?})\n",
                        id.gpu_gen,
                        id.gpu_variant,
                        soc.gpu_gen,
                        soc.gpu_variant
                    );
                }
            }
            Err(e) => dev_warn!(pdev.as_ref(), "AGX3: GPU ID decode failed: {:?}\n", e),
        },
        Err(e) => {
            dev_warn!(
                pdev.as_ref(),
                "AGX3: SGX MMIO region unavailable: {:?}\n",
                e
            );
        }
    }

    match expected.gpu_gen {
        hw::GpuGen::G15 => refuse_g15_probe(pdev),
        hw::GpuGen::G17 => refuse_g17_probe(pdev, soc, &expected, loaded_g17_firmware.as_ref()),
        gen => {
            dev_err!(
                pdev.as_ref(),
                "AGX3 setup unavailable for {:?}: firmware tuning, initdata, and handoff are incomplete; no DMA or processor state changed\n",
                gen
            );
            ENODEV
        }
    }
}

/// Report the grounded-vs-UNKNOWN state of the single-role G15 (M3) boot, then
/// stop the probe. G15 is the easiest AGX3 target — one `GFX` role, classic
/// device-control-ring submission — but the firmware image is not on disk and
/// the init-data/RTKit graph is not boot-complete offline.
fn refuse_g15_probe(pdev: &platform::Device<Core<'_>>) -> Error {
    let Err(missing) = g15_boot::pre_handoff_gate(hw::GpuGen::G15 as u32) else {
        dev_err!(
            pdev.as_ref(),
            "G15 diagnostic target remained selected after its preflight opened\n"
        );
        return EINVAL;
    };

    dev_info!(
        pdev.as_ref(),
        "G15 identity decoded (single-role GFX, classic submission); grounded init-data root and shared RTKit codec are diagnostic-only\n"
    );
    dev_err!(
        pdev.as_ref(),
        "G15 setup incomplete: evidence mask {:#x}; no DMA, MMIO, UAT, or processor state changed\n",
        missing.bits()
    );
    if missing.contains(g15_boot::missing::FIRMWARE_IMAGE) {
        dev_err!(
            pdev.as_ref(),
            "G15 preflight: the G15 GFX RTKit firmware image and its nested identity are not available offline\n"
        );
    }
    if missing.contains(g15_boot::missing::UAT_HANDOFF) {
        dev_err!(
            pdev.as_ref(),
            "G15 preflight: the dual-TTBR 42-bit UAT/Handoff firmware-shared state is not validated\n"
        );
    }
    if missing.contains(g15_boot::missing::INITDATA_GRAPH) {
        dev_err!(
            pdev.as_ref(),
            "G15 preflight: init-data root and runtime-pointer graph are grounded, but hw_globals and struct sizes are not boot-complete\n"
        );
    }
    if missing.contains(g15_boot::missing::RTKIT_TRANSPORT) {
        dev_err!(
            pdev.as_ref(),
            "G15 preflight: the GPU RTKit codec is grounded but HELLO/EPMAP transmit is unproved\n"
        );
    }
    if missing.contains(g15_boot::missing::INTERFACE_VERSION) {
        dev_err!(
            pdev.as_ref(),
            "G15 preflight: the EP0 RTKit interface version for this firmware/OS build is unknown\n"
        );
    }
    if missing.contains(g15_boot::missing::SUBMISSION_TRANSPORT) {
        dev_err!(
            pdev.as_ref(),
            "G15 preflight: classic register-list submission is not implemented\n"
        );
    }

    ENODEV
}

/// Read one T8140 ASCWrap v6 provider from a `mboxes` phandle node.
///
/// Reads the node's two `reg` windows (CPU physical, #address/#size-cells = 2
/// each), the two named `send-empty`/`recv-not-empty` AIC interrupt numbers
/// (3 cells per interrupt), and the `apple,firmware-role` string. Anything
/// missing or malformed fails closed (returns `None`) rather than guessing.
fn read_t8140_asc_observation(
    pdev: &platform::Device<Core<'_>>,
    index: usize,
) -> Option<agx_power_recovery::T8140G17PLinuxDtAscObservation> {
    let node = pdev.as_ref().of_node()?;
    let mbox = node.parse_phandle(c_str!("mboxes"), index)?;

    // Require the exact provider ABI rather than accepting a node with the
    // same numeric resources under a different owner contract.
    let compatible: KVec<u8> = mbox.get_property(c_str!("compatible")).ok()?;
    if compatible.as_slice() != b"apple,t8140-ascwrap-v6\0" {
        return None;
    }
    let reg_names: KVec<u8> = mbox.get_property(c_str!("reg-names")).ok()?;
    if reg_names.as_slice() != b"wrapper\0iop-vbar\0" {
        return None;
    }
    let interrupt_names: KVec<u8> = mbox.get_property(c_str!("interrupt-names")).ok()?;
    if interrupt_names.as_slice() != b"send-empty\0recv-not-empty\0" {
        return None;
    }

    // reg = <wrapper_hi wrapper_lo wsize_hi wsize_lo iovbar_hi iovbar_lo isize_hi isize_lo>
    let reg: KVec<u32> = mbox.get_property(c_str!("reg")).ok()?;
    if reg.len() != 8 {
        return None;
    }
    let cell64 = |hi: u32, lo: u32| ((hi as u64) << 32) | lo as u64;

    // interrupts = <type send flags type recv flags>, AIC number is cell 1/4.
    let interrupts: KVec<u32> = mbox.get_property(c_str!("interrupts")).ok()?;
    const AIC_IRQ: u32 = 0;
    const IRQ_TYPE_LEVEL_HIGH: u32 = 4;
    if interrupts.len() != 6
        || interrupts[0] != AIC_IRQ
        || interrupts[2] != IRQ_TYPE_LEVEL_HIGH
        || interrupts[3] != AIC_IRQ
        || interrupts[5] != IRQ_TYPE_LEVEL_HIGH
    {
        return None;
    }

    // apple,firmware-role is "GFX" (primary) or "GFX1" (secondary).
    let role: KVec<u8> = mbox.get_property(c_str!("apple,firmware-role")).ok()?;
    let role_is_gfx1 = match role.as_slice() {
        b"GFX\0" | b"GFX" => false,
        b"GFX1\0" | b"GFX1" => true,
        _ => return None,
    };

    Some(agx_power_recovery::T8140G17PLinuxDtAscObservation {
        role_is_gfx1,
        wrapper_cpu_base: cell64(reg[0], reg[1]),
        wrapper_size: cell64(reg[2], reg[3]),
        iorvbar_cpu_base: cell64(reg[4], reg[5]),
        iorvbar_size: cell64(reg[6], reg[7]),
        send_empty_irq: interrupts[1],
        recv_not_empty_irq: interrupts[4],
    })
}

/// Read the full observed T8140 dual-ASC topology from the platform device
/// tree, exactly as a Linux platform decoder sees it. `None` when the two
/// `mboxes` providers cannot be read; the caller then reports missing
/// topology rather than admitting an inferred one.
fn read_t8140_linux_dt_topology(
    pdev: &platform::Device<Core<'_>>,
) -> Option<agx_power_recovery::T8140G17PLinuxDtTopology> {
    let node = pdev.as_ref().of_node()?;
    let gpu_reg_names: KVec<u8> = node.get_property(c_str!("reg-names")).ok()?;
    let mboxes: KVec<u32> = node.get_property(c_str!("mboxes")).ok()?;
    if gpu_reg_names.as_slice() != b"asc\0sgx\0"
        || mboxes.len() != 2
        || node.parse_phandle(c_str!("mboxes"), 2).is_some()
    {
        return None;
    }

    Some(agx_power_recovery::T8140G17PLinuxDtTopology {
        gfx: read_t8140_asc_observation(pdev, 0)?,
        gfx1: read_t8140_asc_observation(pdev, 1)?,
    })
}

/// Proof that the live T8140 DT matched both pinned provider roles exactly.
///
/// Construction is private to the read-only admission above; live boot must
/// receive this token before it can query either provider.
pub(crate) struct T8140LiveTopologyAdmission {
    _private: (),
}

fn admit_t8140_live_topology(
    pdev: &platform::Device<Core<'_>>,
    identity: &identity::GpuIdentity,
) -> Result<T8140LiveTopologyAdmission> {
    let observed = read_t8140_linux_dt_topology(pdev).ok_or(EINVAL)?;
    let preflight =
        g17_power_preflight::G17PowerPreflight::new_with_linux_dt(identity, Some(&observed))
            .map_err(|_| EINVAL)?;
    if preflight.t8140_strategy().is_none() {
        return Err(EINVAL);
    }

    Ok(T8140LiveTopologyAdmission { _private: () })
}

fn refuse_g17_probe(
    pdev: &platform::Device<Core<'_>>,
    soc: &'static hw::agx3::SocConfig,
    identity: &identity::GpuIdentity,
    firmware: Option<&g17_firmware::G17FirmwareIdentityAdmission>,
) -> Error {
    // G17P binds its power preflight to the exact D93AP/T8140 topology the
    // Linux device tree actually exposes (its two `mboxes` providers). Never
    // substitute the expected constant: only a real observation that decodes
    // to the pinned providers admits the A18 Pro strategy.
    let power_preflight = if identity.gpu_variant == identity::GpuVariant::P {
        let observed = read_t8140_linux_dt_topology(pdev);
        if observed.is_none() {
            dev_warn!(
                pdev.as_ref(),
                "G17P: could not read the T8140 dual-ASC topology from the device tree mboxes\n"
            );
        }
        match g17_power_preflight::G17PowerPreflight::new_with_linux_dt(identity, observed.as_ref())
        {
            Ok(preflight) => preflight,
            Err(e) => {
                dev_err!(
                    pdev.as_ref(),
                    "G17P topology mismatch: {:?}\n",
                    e
                );
                return ENODEV;
            }
        }
    } else {
        match g17_power_preflight::G17PowerPreflight::new(identity, None) {
            Ok(preflight) => preflight,
            Err(e) => {
                dev_err!(
                    pdev.as_ref(),
                    "G17 target setup unavailable: {:?}\n",
                    e
                );
                return ENODEV;
            }
        }
    };
    let Err(power_missing) = power_preflight.execution_gate() else {
        dev_err!(
            pdev.as_ref(),
            "G17 diagnostic target remained selected after its power execution gate opened\n"
        );
        return EINVAL;
    };
    dev_info!(
        pdev.as_ref(),
        "G17 {:?} cold-boot/power model selected; live execution evidence still missing {:#x}\n",
        power_preflight.target(),
        power_missing.bits()
    );

    let gpu_variant = identity.gpu_variant;
    let gate = match gpu_variant {
        identity::GpuVariant::S => g17_boot::pre_handoff_gate_with_identity(
            hw::GpuGen::G17 as u32,
            g17_boot::G17FirmwareVariant::G17S,
            firmware,
        ),
        identity::GpuVariant::P => g17_boot::pre_handoff_gate_for_variant(
            hw::GpuGen::G17 as u32,
            g17_boot::G17FirmwareVariant::G17P,
        ),
        _ => g17_boot::pre_handoff_gate(hw::GpuGen::G17 as u32),
    };
    let Err(missing) = gate else {
        dev_err!(
            pdev.as_ref(),
            "G17 diagnostic target remained selected after its preflight opened\n"
        );
        return EINVAL;
    };

    dev_info!(
        pdev.as_ref(),
        "G17 identity decoded; grounded init-data and RTKit codecs are diagnostic-only\n"
    );
    if firmware.is_some() {
        let Err(default_missing) = g17_boot::pre_handoff_gate(hw::GpuGen::G17 as u32) else {
            dev_err!(
                pdev.as_ref(),
                "G17: default pre-handoff unexpectedly opened\n"
            );
            return EINVAL;
        };
        if missing.bits() != default_missing.bits() & !g17_boot::missing::FIRMWARE_IDENTITY {
            dev_err!(
                pdev.as_ref(),
                "G17 firmware admission changed an unrelated setup gate\n"
            );
            return EINVAL;
        }
        dev_info!(
            pdev.as_ref(),
            "G17: loaded GFX/GFX1 evidence cleared the firmware-identity bit; remaining operations are reported below\n"
        );
    }
    dev_err!(
        pdev.as_ref(),
        "G17 setup incomplete: evidence mask {:#x}; no DMA, MMIO, UAT, or processor state changed\n",
        missing.bits()
    );
    if missing.contains(g17_boot::missing::FIRMWARE_IDENTITY) {
        dev_err!(
            pdev.as_ref(),
            "G17 preflight: no exact versioned m1n1 loaded-segment GFX/GFX1 identity was admitted\n"
        );
    }
    if missing.contains(g17_boot::missing::ROLE_RESOURCES) {
        dev_err!(
            pdev.as_ref(),
            "G17 preflight: distinct GFX/GFX1 resource plumbing is not wired\n"
        );
    }
    if missing.contains(g17_boot::missing::TOPOLOGY_REGISTERS) {
        // Cannot fire while regs.rs wires the proven G14X-family bank for
        // AGX3; kept for the case where that evidence is ever withdrawn.
        dev_err!(
            pdev.as_ref(),
            "G17 preflight: G17 core-mask register bank is not proven\n"
        );
    }
    if missing.contains(g17_boot::missing::UAT_HANDOFF) {
        dev_err!(
            pdev.as_ref(),
            "G17 preflight: SPTM/UAT ownership handoff is not implemented\n"
        );
    }
    if missing.contains(g17_boot::missing::INITDATA_GRAPH) {
        dev_err!(
            pdev.as_ref(),
            "G17 preflight: init-data graph has grounded fields but is not boot-complete\n"
        );
    }
    if missing.contains(g17_boot::missing::RTKIT_TRANSPORT) {
        dev_err!(
            pdev.as_ref(),
            "G17 preflight: RTKit codec is grounded but HELLO/EPMAP transmit is unproved\n"
        );
    }
    if missing.contains(g17_boot::missing::GFX1_POWER_SEQUENCE) {
        dev_err!(
            pdev.as_ref(),
            "{}{}",
            "G17 preflight: G17S PIO type identities, transition order, intervening state/sync order, and one 250us-reported poll are grounded, ",
            "but type 0/0xb physical bases, opaque helper ownership, and G17G/G17P firmware parity are incomplete\n"
        );
    }
    if missing.contains(g17_boot::missing::RECOVERY_HANDSHAKE) {
        dev_err!(
            pdev.as_ref(),
            "{}{}",
            "G17 preflight: recovery word states 0..3, host writes 1->2/3->0, and firmware writes 1 at begin/0 at completion are grounded, ",
            "but host-visible DRAM causality and the cross-role 0x22/0x23 handshake are incomplete\n"
        );
    }
    if missing.contains(g17_boot::missing::SUBMISSION_TRANSPORT) {
        dev_err!(
            pdev.as_ref(),
            "{}{}",
            "G17 preflight: submission codecs include the grounded EP 0x21 doorbell, SKSM queue/configure words, fixed-slot scratch geometry, and kick-record table; ",
            "Linux resource-0 ordered publication is implemented; firmware execution, the round trip, and completion IRQ causality remain unproved\n"
        );
    }

    if identity.gpu_variant == identity::GpuVariant::P {
        let target = g17_manager::TargetObservation {
            chip_id: soc.chip_id,
            declared_gen: soc.gpu_gen,
            declared_variant: soc.gpu_variant,
            hw_family: soc.hw_family,
            hw_variant: soc.hw_variant,
            num_dies: soc.num_dies,
            uat_ias: soc.uat_ias,
            uat_oas: soc.uat_oas,
        };
        let platform = g17_manager::T8140_G17P_PLATFORM_CONFIG;
        match g17_manager::validate_target(target)
            .and_then(|_| g17_manager::validate_platform(platform))
            .and_then(|_| platform.sksm_scratch.fifo_offsets())
        {
            Ok(scratch) => dev_info!(
                pdev.as_ref(),
                "G17P manager inputs admitted without AGX2 HwConfig: {}-bit UAT, {:#x}-byte pages, dual-role resource geometry, SKSM scratch {:#x}/{:#x}; resource-0 ordered publisher available\n",
                g17_manager::T8140_MANAGER_CONFIG.uat_ias,
                g17_manager::T8140_MANAGER_CONFIG.uat_page_size,
                scratch.write0,
                scratch.write1
            ),
            Err(e) => dev_err!(
                pdev.as_ref(),
                "G17P manager inputs were invalid before resource allocation: {:?}\n",
                e
            ),
        }
    }

    ENODEV
}

/// Platform Driver implementation for `AsahiDriver`.
impl platform::Driver for AsahiDriver {
    type IdInfo = ProbeConfig;
    type Data<'bound> = Self;
    const OF_ID_TABLE: Option<of::IdTable<Self::IdInfo>> = Some(&OF_TABLE);

    /// Device probe function.
    fn probe<'bound>(
        pdev: &'bound platform::Device<Core<'_>>,
        info: Option<&'bound Self::IdInfo>,
    ) -> impl PinInit<Self::Data<'bound>, Error> + 'bound {
        debug::update_debug_flags();

        dev_info!(pdev.as_ref(), "Probing...\n");

        let cfg = match info.ok_or(ENODEV)? {
            ProbeConfig::Supported(cfg) => *cfg,
            ProbeConfig::Agx3Diagnostic(soc) if soc.chip_id == 0x8140 => {
                let runtime = probe_t8140_runtime(pdev, soc)?;
                dev_info!(
                    pdev.as_ref(),
                    "G17P: persistent dual-ASC runtime bound and DRM render node registered\n"
                );
                return Ok(Self {
                    runtime: AsahiRuntime::G17P(runtime),
                });
            }
            // T8152/G18G is routed before the generic AGX3 refusal: that
            // path reads SGX ID registers, which J873 must not do unless the
            // sgx PSD service proves the block powered.
            ProbeConfig::Agx3Diagnostic(soc) if soc.chip_id == 0x8152 => {
                return Ok(Self { runtime: AsahiRuntime::G18(probe_t8152(pdev, soc)?) });
            }
            ProbeConfig::Agx3Diagnostic(soc) if soc.chip_id == 0x8142
                && crate::module_parameters::m5_initdata_test.value() == 14 => {
                return Ok(Self { runtime: AsahiRuntime::M5Drm(probe_t8142_runtime(pdev)?) });
            }
            ProbeConfig::Agx3Diagnostic(soc) if soc.chip_id == 0x8142
                && matches!(crate::module_parameters::m5_initdata_test.value(), 1..=13) => {
                return Ok(Self { runtime: AsahiRuntime::M5(probe_t8142_initdata(pdev)?) });
            }
            ProbeConfig::Agx3Diagnostic(soc) => return Err(refuse_agx3_probe(pdev, soc)),
        };

        // G17 requires two physical ASC roles with distinct transports and
        // init-data roots, but one shared GPU/UAT owner. Refuse before any
        // MMIO setup or ASC start until the target-specific GFX1 resources,
        // power mapping, recovery wire protocol, and submission handoff are
        // all implemented from evidence.
        if let Err(missing) = g17_boot::pre_handoff_gate(cfg.gpu_gen as u32) {
            dev_err!(
                pdev.as_ref(),
                "G17 dual-role setup incomplete: evidence mask {:#x}; processors were not started\n",
                missing.bits()
            );
            return Err(ENODEV);
        }

        unsafe { pdev.dma_set_mask_and_coherent(DmaMask::try_new(cfg.uat_oas)?)? };

        let res = regs::Resources::new(pdev)?;

        // Initialize misc MMIO
        res.init_mmio()?;

        // Start the coprocessor CPU, so UAT can initialize the handoff
        regs::Resources::start_cpu(pdev)?;

        let fwnode = pdev.as_ref().fwnode().ok_or(EIO)?;
        let compat: KVec<u32> = fwnode
            .property_read_array_vec(c_str!("apple,firmware-compat"), 3)?
            .required_by(pdev.as_ref())?;

        // TODO: This is very temporary
        // SAFETY: This should be safe as data is not touched by the driver
        // untill it gets fully initialised.
        // Additionally drm::device::Device::release() will not drop data and
        // leaks instead.
        let uninit = unsafe {
            pin_init::pin_init_from_closure::<AsahiData, kernel::error::Error>(|_slot| Ok(()))
        };
        let drm: ARef<AsahiDevice> = drm::device::Device::new(pdev.as_ref(), uninit)?;

        let legacy_gpu = match (cfg.gpu_gen, cfg.gpu_variant, compat.as_slice()) {
            (hw::GpuGen::G13, _, &[12, 3, 0]) => {
                gpu::GpuManagerG13V12_3::new(&drm.clone(), &res, cfg)? as Arc<dyn gpu::GpuManager>
            }
            (hw::GpuGen::G14, hw::GpuVariant::G, &[12, 4, 0]) => {
                gpu::GpuManagerG14V12_4::new(&drm.clone(), &res, cfg)? as Arc<dyn gpu::GpuManager>
            }
            (hw::GpuGen::G13, _, &[13, 5, 0]) => {
                gpu::GpuManagerG13V13_5::new(&drm.clone(), &res, cfg)? as Arc<dyn gpu::GpuManager>
            }
            (hw::GpuGen::G14, hw::GpuVariant::G, &[13, 5, 0]) => {
                gpu::GpuManagerG14V13_5::new(&drm.clone(), &res, cfg)? as Arc<dyn gpu::GpuManager>
            }
            (hw::GpuGen::G14, _, &[13, 5, 0]) => {
                gpu::GpuManagerG14XV13_5::new(&drm.clone(), &res, cfg)? as Arc<dyn gpu::GpuManager>
            }
            _ => {
                dev_info!(
                    pdev.as_ref(),
                    "Unsupported GPU/firmware combination ({:?}, {:?}, {:?})\n",
                    cfg.gpu_gen,
                    cfg.gpu_variant,
                    compat
                );
                return Err(ENODEV);
            }
        };
        let gpu: Arc<dyn drm_gpu::DrmGpu> =
            Arc::new(drm_gpu::LegacyDrmGpu::new(legacy_gpu), GFP_KERNEL)?;

        let data = try_pin_init!(AsahiData {
            gpu,
            pdev: pdev.into(),
            resources: Some(res),
            g17_runtime: None,
            g17_event_work <- new_work!("AsahiData::g17_event_work"),
            g17_recovery_work <- new_work!("AsahiData::g17_recovery_work"),
        g17_reclaim_work <- new_work!("AsahiData::g17_reclaim_work"),
            g17_tvb_grow_work <- new_work!("AsahiData::g17_tvb_grow_work"),
            g17_dvfs_work <- new_delayed_work!("AsahiData::g17_dvfs_work"),
            g17_feed_perf_work <- new_work!("AsahiData::g17_feed_perf_work"),
            g17_stall_work <- new_delayed_work!("AsahiData::g17_stall_work"),
        g17_reclaims <- new_mutex!(KVec::new(), "AsahiData::g17_reclaims"),
        });

        let ptr: *const AsahiData = &raw const **drm;
        unsafe {
            data.__init(ptr as *mut AsahiData)?;
        }

        (*drm).gpu.init()?;

        drm::driver::Registration::new_foreign_owned(&drm, pdev.as_ref(), 0)?;

        Ok(Self {
            runtime: AsahiRuntime::Legacy(drm),
        })
    }

    fn unbind(_pdev: &platform::Device<Core<'_>>, this: Pin<&Self>) {
        if let AsahiRuntime::G18(runtime) = &this.runtime {
            runtime.shutdown();
            return;
        }
        if let AsahiRuntime::M5Drm(runtime) = &this.runtime {
            let _ = runtime.lock().shutdown();
            return;
        }
        let AsahiRuntime::G17P(runtime) = &this.runtime else {
            return;
        };
        let drm = match runtime.lock().drm_ref() {
            Ok(drm) => drm,
            Err(_) => return,
        };
        g17_lifecycle::quiesce_driver_unbind(&G17PDriverUnbind { runtime, drm });
        // Explicit shutdown is required with owned callback handles: waiting
        // for the final Arc would retain a runtime/DRM reference cycle.
        runtime.lock().shutdown();
    }
}
