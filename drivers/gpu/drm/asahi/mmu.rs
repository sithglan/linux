// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! GPU UAT (MMU) management
//!
//! AGX GPUs use an MMU called the UAT, which is largely compatible with the ARM64 page table
//! format. This module manages the global MMU structures, including a shared handoff structure
//! that is used to coordinate VM management operations with the firmware, the TTBAT which points
//! to currently active GPU VM contexts, as well as the individual `Vm` operations to map and
//! unmap buffer objects into a single user or kernel address space.
//!
//! The actual page table management is in the `pt` module.

use core::fmt::Debug;
use core::mem::size_of;
use core::num::NonZeroUsize;
use core::ops::Range;
use core::sync::atomic::{
    fence,
    AtomicBool,
    AtomicU32,
    AtomicU64,
    AtomicU8,
    Ordering, //
};

use core::cmp;

use kernel::{
    addr::PhysicalAddr,
    bindings::{self, drm_gpuvm_flags_DRM_GPUVM_IMMEDIATE_MODE},
    c_str,
    device,
    drm::{
        gem::{shmem, BaseObject},
        gpuvm,
        mm, //
    },
    error::Result,
    io,
    new_condvar,
    new_mutex,
    page::Page,
    prelude::*,
    static_lock_class,
    sync::{
        aref::ARef,
        lock::{
            mutex::MutexBackend,
            Guard, //
        },
        Arc,
        CondVar,
        Mutex, //
    },
    time::{
        delay::fsleep,
        Delta,
        Instant,
        Monotonic, //
    }, //
};

use crate::debug::*;
use crate::module_parameters;
use crate::no_debug;
use crate::{
    driver,
    fw,
    gem,
    hw,
    mem,
    pgtable,
    slotalloc,
    uat::UatGeometry,
    util::RangeExt, //
};

// KernelMapping protection types
pub(crate) use crate::pgtable::Prot;
pub(crate) use pgtable::prot::*;
pub(crate) use pgtable::{
    UatPageTable,
    UAT_PGBIT,
    UAT_PGMSK,
    UAT_PGSZ, //
};

use pin_init;

const DEBUG_CLASS: DebugFlags = DebugFlags::Mmu;

#[path = "m5_context.rs"]
mod m5_context;
pub(crate) use m5_context::M5ExecutionContext;

/// PPL magic number for the handoff region
const PPL_MAGIC: u64 = 0x4b1d000000000002;

/// Number of supported context entries in the TTBAT
const UAT_NUM_CTX: usize = 64;
/// First context available for users
const UAT_USER_CTX_START: usize = 1;
/// Number of available user contexts
const UAT_USER_CTX: usize = UAT_NUM_CTX - UAT_USER_CTX_START;
const T8140_CONTEXT_BOOTSTRAP_SOURCE_HIGH: u8 = 0;
const T8140_CONTEXT_BOOTSTRAP_EMPTY_HIGH: u8 = 1;
const T8140_CONTEXT_USER_OWNED: u8 = 2;
const T8140_CONTEXT_OPENING_RENDER_SHARED: u8 = 3;

/// Hardware contexts the G17P compute path declares.
///
/// `G17PComputeSubmissionContext { context_id: 2, .. }` (`g17_live_boot.rs`)
/// is encoded into compute registers `0x10201`/`0x10428` as
/// `(context_id << 8) | work_ordinal` and into descriptor `+0x0c`
/// (`g17_compute.rs`).  These are the contexts m1n1's hardware-proven
/// `agx_g17p_native_add3.py` uses:
///
/// ```text
/// CONTEXT = 2                                                     # :24
/// context_id = CONTEXT if ordinal == 0 else CONTEXT + 1           # :364
/// ```
///
/// i.e. workload 0 runs in context 2 and every later workload runs in
/// context 3, the "retained context-3 pair" its own comment names (`:375`).
/// m1n1 installs both halves of both contexts' roots before it kicks
/// (`:1072-1073` and `:1087-1089`).
const T8140_COMPUTE_CTXS: [usize; 2] = [2, 3];
/// First context handed out by native's application-GART context manager on
/// the captured G17P system. Contexts 0/1 are the opening pair, 2/3 are the
/// fixed compute aliases and context 4 is accelerator-reserved. Native render
/// command GARTs consequently appear at 5, 6, ... depending on live allocator
/// state; those values are allocator results, not descriptor constants.
const T8140_APP_CONTEXT_START: usize = 5;
const T8140_APP_CONTEXT_RESERVED_MASK: u64 = (1u64 << T8140_APP_CONTEXT_START) - 1;
const T8140_APP_CONTEXT_AVAILABLE_MASK: u64 = !T8140_APP_CONTEXT_RESERVED_MASK;

/// Bootstrap-only T8140 UAT binding count. Ordinary jobs retain context 1 as
/// the global FList root and use independently allocated app-GART contexts.
const T8140_BIND_SLOTS: usize =
    1 + crate::g17_queue_limits::BIND_CONTEXT_COUNT as usize;

/// Hardware context for one T8140 UAT bind slot, or `None` above the pool.
///
/// Slot 0 maps to context 1 for the measured driver-owned opening. No ordinary
/// render or independent compute queue reaches this allocator.
const fn t8140_bind_context(slot: u32) -> Option<usize> {
    if slot == 0 {
        Some(1)
    } else if (slot as usize) < T8140_BIND_SLOTS {
        Some(crate::g17_queue_limits::BIND_CONTEXT_FIRST as usize + slot as usize - 1)
    } else {
        None
    }
}

const _: () = {
    // The bind band must not overlap anything else that owns a TTBAT entry.
    assert!(
        crate::g17_queue_limits::BIND_CONTEXT_MASK
            & crate::g17_queue_limits::COMPUTE_CONTEXT_MASK
            == 0
    );
    assert!(crate::g17_queue_limits::BIND_CONTEXT_MASK & T8140_APP_CONTEXT_RESERVED_MASK == 0);
    assert!(
        crate::g17_queue_limits::BIND_CONTEXT_MASK
            & (1u64 << crate::g17_queue_limits::RENDER_APP_CONTEXT)
            == 0
    );
    assert!(crate::g17_queue_limits::BIND_CONTEXT_FIRST as usize + 1 > T8140_APP_CONTEXT_START);
};
pub(crate) const T8140_PARAMETER_METRICS_LOW_VA: u64 = 0x0000_0010_0008_0000;
pub(crate) const T8140_PARAMETER_METRICS_SIZE: usize = 0x8000;

/// One native G17P process resource and its secure-GART context-0 view.
///
/// `source_address` names the canonical process-GART mapping userspace owns;
/// `context_address` is the low-32-bit address returned for the same native
/// `AGXSecureMemoryMap`. Permissions describe the restricted view, not the
/// (potentially more permissive) ordinary mapping.
#[derive(Clone, Copy)]
struct T8140NativeContextView {
    source_address: u64,
    context_address: u64,
    size: usize,
    requires_source_write: bool,
    context_prot: Prot,
}

/// Initial render-context restricted-view fanout observed through native
/// `AGXSecureGart`.
///
/// The first object is a four-page resource whose ordinary GART address is
/// `0x1000_0000_00`; `getGPUVirtualAddressForUserProcess` returns zero for the
/// same `AGXSecureMemoryMap`. Native maps the canonical high address with raw
/// leaf attributes `...0xc8b`; SecureGart supplies the low user presentation
/// without a second leaf in the captured native root. The following six views
/// are the same high-to-low transformation for the fixed resources occupying
/// the rest of Mesa's current 0x80000 context BO. Linux hardware reports dIPP
/// misses for those low presentations in context 0, so the driver emulates
/// SecureGart by materializing them in the retained lower root only while the
/// submitting VM owns the render graph.
const T8140_NATIVE_CONTEXT_VIEWS: [T8140NativeContextView; 7] = [
    T8140NativeContextView {
        source_address: 0x0010_0000_0000,
        context_address: 0,
        size: 0x1_0000,
        requires_source_write: false,
        context_prot: PROT_GPU_SHARED_RO,
    },
    T8140NativeContextView {
        source_address: 0x0010_0001_8000,
        context_address: 0x0001_8000,
        size: 0x8000,
        requires_source_write: false,
        context_prot: PROT_GPU_SHARED_RO,
    },
    T8140NativeContextView {
        source_address: 0x0010_0002_8000,
        context_address: 0x0002_8000,
        size: 0x8000,
        requires_source_write: false,
        context_prot: PROT_GPU_SHARED_RO,
    },
    T8140NativeContextView {
        source_address: 0x0010_0003_8000,
        context_address: 0x0003_8000,
        size: 0x8000,
        requires_source_write: false,
        context_prot: PROT_GPU_SHARED_RO,
    },
    T8140NativeContextView {
        source_address: 0x0010_0004_8000,
        context_address: 0x0004_8000,
        size: 0x8000,
        requires_source_write: false,
        context_prot: PROT_GPU_SHARED_RO,
    },
    T8140NativeContextView {
        source_address: 0x0010_0005_8000,
        context_address: 0x0005_8000,
        size: 0x8000,
        requires_source_write: true,
        context_prot: PROT_GPU_SHARED_RW,
    },
    T8140NativeContextView {
        source_address: 0x0010_0006_8000,
        context_address: 0x0006_8000,
        size: 0xc000,
        requires_source_write: true,
        context_prot: PROT_GPU_SHARED_RW,
    },
];
const T8140_NATIVE_CONTEXT_VIEW_COUNT: usize = T8140_NATIVE_CONTEXT_VIEWS.len();

/// Native's SecureGart low presentation of view 0, materialized in the
/// submitting process's *own* low root rather than in the retained context-0
/// root.
///
/// `AGXSecureGart` gives a process resource two presentations: the canonical
/// GART address (0x10_0000_0000 here) and a low "user process" address, which
/// `getGPUVirtualAddressForUserProcess` reports as zero for this resource.
/// Linux materializes the low presentation itself, and hardware proved the
/// client regime needs it too: with only the context-0 aliases installed, the
/// first fragment publication latched four gUPM (unit 0xd9) faults at
/// `0x1e00`/`0x1e80`/`0x1f00`/`0x1f80` with `reason=Unmapped` and `vm-slot=1`,
/// i.e. inside the render VM's own root, which had no leaf below
/// `IOVA_USER_BASE` at all.
///
/// The faulting transactions report `read=0`, so this view is mapped
/// GPU-writable: an unmapped *write* cannot be satisfied by a read-only leaf.
/// Contrast the context-0 copy above, which keeps the restricted class-1
/// attributes captured from native for the canonical mapping.
const T8140_CLIENT_NULL_VIEW: T8140NativeContextView = T8140NativeContextView {
    source_address: 0x0010_0000_0000,
    context_address: 0,
    size: 0x1_0000,
    requires_source_write: true,
    context_prot: PROT_GPU_SHARED_RW,
};

/// VA window a T8140 client root keeps for [`T8140_CLIENT_NULL_VIEW`].
///
/// Reserved for the VM's whole lifetime even though the leaf itself only
/// exists while that VM owns the render graph, so a user bind can never take
/// the window between graphs. Mesa's own heap starts orders of magnitude
/// above this, and Mesa binds the remaining low views (0x18000 upwards)
/// itself.
pub(crate) const T8140_CLIENT_NULL_VIEW_START: u64 = T8140_CLIENT_NULL_VIEW.context_address;
pub(crate) const T8140_CLIENT_NULL_VIEW_SIZE: usize = T8140_CLIENT_NULL_VIEW.size;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct T8140RootLayout {
    low: [u64; 2],
    high: [u64; 2],
}

fn t8140_tagged_root(root: u64, slot: usize) -> u64 {
    root | TTBR_VALID | ((slot as u64) << TTBR_ASID_SHIFT)
}

fn context_mask_for_root(root: u64, ttbs: &[SlotTTBS; UAT_NUM_CTX]) -> u64 {
    let mut contexts = 0u64;
    for (context_id, ttb) in ttbs.iter().enumerate() {
        if ttb.ttb0.load(Ordering::Acquire) == t8140_tagged_root(root, context_id) {
            contexts |= 1u64 << context_id;
        }
    }
    contexts
}

fn t8140_opening_render_layout(render_low: u64, source_high: u64) -> T8140RootLayout {
    T8140RootLayout {
        low: [
            t8140_tagged_root(render_low, 0),
            t8140_tagged_root(render_low, 1),
        ],
        high: [
            t8140_tagged_root(source_high, 0),
            t8140_tagged_root(source_high, 1),
        ],
    }
}

fn t8140_split_render_layout(
    retained_low: u64,
    render_low: u64,
    high: [u64; 2],
) -> T8140RootLayout {
    T8140RootLayout {
        low: [
            t8140_tagged_root(retained_low, 0),
            t8140_tagged_root(render_low, 1),
        ],
        high: [
            t8140_tagged_root(high[0], 0),
            t8140_tagged_root(high[1], 1),
        ],
    }
}

/// Lower/user base VA
pub(crate) const IOVA_USER_BASE: u64 = UAT_PGSZ as u64;
/// Lower/user top VA for the 39-bit (AGX2) roots. Geometry-aware paths must
/// use [`iova_user_range`]/[`iova_user_usable_range`] instead; this const
/// remains for the AGX2 initdata builder, which is 39-bit-only.
pub(crate) const IOVA_USER_TOP: u64 = 1 << 39;

fn lower_vm_start(cfg: UatConfig, t8140_internal: bool) -> u64 {
    if cfg == UatConfig::T8140 && t8140_internal {
        0
    } else {
        IOVA_USER_BASE
    }
}

/// Whether the firmware-shared MMU state (Handoff/FlushInfo layout, TTBAT
/// publication, kernel-half window placement) has been validated against an
/// AGX3 (G15/G16/G17) firmware build.
///
/// The 42-bit walk geometry, the kernel-half VA layout, and the leaf PTE
/// policy below this boundary are implemented and host-tested (the AGX3 leaf
/// PTE encoder is byte-identical to AGX2: AGXG17X `encodePTEFlags`
/// @ 0xfffffe0008b9975c). The [`Handoff`] struct is not an ordinary firmware
/// input struct that can be byte-grounded from a disassembler: it is a live
/// AP<->firmware PPL coherence region driven by a Dekker mutual-exclusion
/// handshake and a `magic_ap`/`magic_fw` exchange (`PPL_MAGIC`), managed by
/// the RTKit MMU/PPL runtime, not by the AGX message handlers. The on-disk M5
/// `GFX.a010.macho` confirms the region exists on G17 (`"MMU uat_handoff !map"`
/// / `"PPL handoff !vm export"` @ 0x2b570 / 0x2b518) but does not expose its
/// exact field offsets, and — critically — [`Handoff::init`] blocks until the
/// firmware writes `magic_fw == PPL_MAGIC`, so a correct layout can only ever
/// be *confirmed* by a live boot ACK. This is therefore the one gate that
/// stays fail-closed by design: it is exactly the "needs a live firmware ACK"
/// boundary. [`Uat::new`] fails closed on it for 42-bit parts *in Dekker
/// mode*. Flip it only once a live G15+/G17 boot completes the PPL handshake.
///
/// T8140 (A18 Pro / G17P) is different and is handled by
/// [`HandoffMode::AbsentStandinT8140`], not by this flag: live J700 runs that
/// booted both firmware instances confirmed the firmware **never publishes
/// its half of the PPL magic on this part** — a `magic_fw` waiter hangs
/// forever. The correct behavior there is an absent-handoff stand-in (no
/// magic wait, no Dekker peer, no flush handshake ACK expectation), which is
/// what that mode implements.
const AGX3_FIRMWARE_HANDOFF_VALIDATED: bool = false;

/// How the firmware-shared UAT handoff region behaves for the target SoC.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum HandoffMode {
    /// The AGX2-proven protocol: firmware publishes `magic_fw`, participates
    /// in the Dekker lock, and ACKs flush requests (state 1 -> 2).
    FirmwareDekker,
    /// T8140-verified stand-in: the firmware never participates in the
    /// handoff region. The AP publishes its own half and clears the flush
    /// slots, but never waits for `magic_fw` (hardware fact: it never
    /// arrives; a waiter hangs forever) and never expects a flush ACK.
    /// Only valid on T8140, where this was observed on live hardware.
    ///
    /// Constructed by the T8140 resource owner before either role receives an
    /// init-data root.
    AbsentStandinT8140,
    /// M5 25G76 bring-up mode. Native snapshots before/after exact GPU work
    /// leave the entire handoff page zero. The explicit M5 startup path
    /// tests this no-peer policy; it does not enable userspace or flushing.
    NoPeerT8142,
    /// T8152/G18G (J873, 26A428) bring-up mode: the same no-peer policy as
    /// [`HandoffMode::NoPeerT8142`], kept separate because the G18G handoff
    /// page has not been observed. [`Handoff::init`] refuses it when the
    /// firmware half (magic or lock) is already non-zero, so a live peer is
    /// detected rather than overwritten.
    NoPeerT8152,
}

const T8140_SECONDARY_SHARED_ROOT_DELTA: u64 = 0x40000;
const T8140_FIRMWARE_PRIVATE_ROOT_ENTRIES: usize = 2;
const T8140_FIRMWARE_SHARED_ROOT_ENTRIES: usize = 3;
const T8140_FIRMWARE_CRASH_BUFFERS: [Range<u64>; 2] = [
    0x01e1_c000..0x01e2_4000,
    0x01f5_0000..0x01f5_8000,
];
const T8140_FIRMWARE_CRASH_SENTINELS: [u8; 2] = [0xa5, 0x5a];

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct T8140PrimaryRootSnapshot {
    entries: [u64; T8140_FIRMWARE_SHARED_ROOT_ENTRIES],
}

/// Read-only descriptor for the context-0 lower root left by the boot
/// firmware. G17P advertises its preallocated RTKit crash buffers through
/// this tree before Linux replaces the live TTBAT entry with its own root.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct FirmwarePageTableRoot {
    ttb: PhysicalAddr,
    cfg: UatConfig,
}

impl FirmwarePageTableRoot {
    pub(crate) const fn ttb(&self) -> PhysicalAddr {
        self.ttb
    }

    #[cfg(CONFIG_DEV_COREDUMP)]
    pub(crate) fn copy_firmware_range(&self, address: u64, size: usize) -> Result<KVVec<u8>> {
        if size == 0 {
            return Err(EINVAL);
        }
        let geometry = uat_geometry(self.cfg)?;
        let end = address.checked_add(size as u64).ok_or(EOVERFLOW)?;
        if end > geometry.user_va_top() {
            return Err(EFAULT);
        }
        let mut data = KVVec::from_elem(0, size, GFP_KERNEL)?;
        UatPageTable::copy_from_phys_root(
            self.ttb,
            address,
            &mut data,
            self.cfg.ias,
            self.cfg.oas,
        )?;
        Ok(data)
    }

    /// Copy live firmware-owned memory through this root with cache
    /// invalidation before every page-table and payload load.
    #[cfg(CONFIG_DEV_COREDUMP)]
    pub(crate) fn copy_live_firmware_range(
        &self,
        address: u64,
        size: usize,
    ) -> Result<KVVec<u8>> {
        if size == 0 {
            return Err(EINVAL);
        }
        let geometry = uat_geometry(self.cfg)?;
        let end = address.checked_add(size as u64).ok_or(EOVERFLOW)?;
        if end > geometry.user_va_top() {
            return Err(EFAULT);
        }
        let mut data = KVVec::from_elem(0, size, GFP_KERNEL)?;
        UatPageTable::copy_live_from_phys_root(
            self.ttb,
            address,
            &mut data,
            self.cfg.ias,
            self.cfg.oas,
        )?;
        Ok(data)
    }
}

/// Retained read-only access to the live TTBAT. RTKit can replace context 0's
/// lower root after Linux has initialized the table, so crash handling must
/// sample the entry after the firmware reports its crash rather than cache the
/// boot-time value.
pub(crate) struct FirmwareContextTableReader {
    inner: Arc<UatInner>,
    cfg: UatConfig,
    t8140_crash_buffers: Option<[ARef<gem::Object>; 2]>,
}

impl Clone for FirmwareContextTableReader {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            cfg: self.cfg,
            t8140_crash_buffers: self.t8140_crash_buffers.as_ref().map(|buffers| {
                [buffers[0].clone(), buffers[1].clone()]
            }),
        }
    }
}

impl FirmwareContextTableReader {
    pub(crate) fn context_root(&self, slot: usize) -> Option<FirmwarePageTableRoot> {
        if slot >= UAT_NUM_CTX {
            return None;
        }
        let shared = self.inner.lock();
        decode_firmware_ttb_root(shared.ttbs()[slot].ttb0.load(Ordering::Acquire), self.cfg)
            .map(|ttb| FirmwarePageTableRoot { ttb, cfg: self.cfg })
    }

    pub(crate) fn context0_root(&self) -> Option<FirmwarePageTableRoot> {
        self.context_root(0)
    }

    /// Diagnostic read-only view of context 0's upper root. Addresses passed
    /// to the walker are offsets within that root, not canonical high VAs.
    #[cfg(CONFIG_DEV_COREDUMP)]
    pub(crate) fn context0_upper_root(&self) -> Option<FirmwarePageTableRoot> {
        let shared = self.inner.lock();
        decode_firmware_ttb_root(shared.ttbs()[0].ttb1.load(Ordering::Acquire), self.cfg)
            .map(|ttb| FirmwarePageTableRoot { ttb, cfg: self.cfg })
    }

    /// Copy one T8140 crash buffer through the retained WC CPU alias. This is
    /// distinct from the raw physical walker used to diagnose the live UAT:
    /// the latter maps pages WB and can hide device writes behind a conflicting
    /// WC alias.
    #[cfg(CONFIG_DEV_COREDUMP)]
    pub(crate) fn copy_t8140_crash_buffer(
        &self,
        index: usize,
        size: usize,
    ) -> Result<(KVVec<u8>, u8)> {
        let buffers = self.t8140_crash_buffers.as_ref().ok_or(ENOENT)?;
        let object = buffers.get(index).ok_or(EINVAL)?;
        if size == 0 || size > object.size() {
            return Err(ERANGE);
        }

        // The crash notification is ordered after firmware's payload stores.
        // Complete the device/CPU visibility boundary before reading the WC
        // alias that was used to create the firmware mapping.
        mem::sync();
        fence(Ordering::Acquire);
        let vmap = object.vmap::<u8>()?;
        let bytes = unsafe {
            // SAFETY: the VMap covers the complete retained GEM allocation and
            // `size` was checked against that allocation above.
            core::slice::from_raw_parts(vmap.as_ptr(), size)
        };
        let mut data = KVVec::new();
        data.extend_from_slice(bytes, GFP_ATOMIC)?;
        Ok((data, T8140_FIRMWARE_CRASH_SENTINELS[index]))
    }
}

fn decode_firmware_ttb_root(raw: u64, cfg: UatConfig) -> Option<PhysicalAddr> {
    if raw & TTBR_VALID == 0 {
        return None;
    }
    let physical_mask = (1u64 << cfg.oas) - 1;
    let root = raw & physical_mask & !(UAT_PGMSK as u64);
    (root != 0).then_some(root)
}

fn clear_stale_host_root_entries(root: &mut [u64]) {
    if root.len() > T8140_FIRMWARE_SHARED_ROOT_ENTRIES {
        root[T8140_FIRMWARE_SHARED_ROOT_ENTRIES..].fill(0);
    }
}

fn mirror_host_root_entries(primary: &[u64], secondary: &mut [u64]) -> usize {
    let mut mirrored = 0;
    for (source, destination) in primary
        .iter()
        .zip(secondary.iter_mut())
        .skip(T8140_FIRMWARE_PRIVATE_ROOT_ENTRIES)
    {
        if *source != 0 && *destination != *source {
            *destination = *source;
            mirrored += 1;
        }
    }
    mirrored
}

/// Return the evidence-backed UAT root geometry for this SoC.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct UatConfig {
    chip_id: u32,
    ias: u8,
    oas: u32,
}

impl UatConfig {
    const fn from_hw(cfg: &hw::HwConfig) -> Self {
        Self {
            chip_id: cfg.chip_id,
            ias: cfg.uat_ias,
            oas: cfg.uat_oas,
        }
    }

    const T8140: Self = Self {
        chip_id: 0x8140,
        ias: 42,
        oas: 42,
    };

    const T8142: Self = Self { chip_id: 0x8142, ias: 42, oas: 42 };

    /// T8152 (J873, G18G). The ADT uat-vaddr-size is 43, as on T8142: 42
    /// bits per root plus the TTBR-select bit. The ADT
    /// agx-address-space-mgmt-mode is 1 on both. J873 DRAM ends at
    /// 0x106_0000_0000, inside the 42-bit output space.
    const T8152: Self = Self { chip_id: 0x8152, ias: 42, oas: 42 };

    fn has_g17_private_roots(self) -> bool {
        self == Self::T8140 || self == Self::T8142 || self == Self::T8152
    }

    /// Dual-role firmware whose native topology gives each role a private
    /// upper root that shares one L2 page (M5 25G76 and J873 26A428).
    fn has_m5_native_topology(self) -> bool {
        self == Self::T8142 || self == Self::T8152
    }
}

/// J873 (T8152) boot-local firmware reservations from the ADT /arm-io/sgx
/// node, as a loader copies them into /reserved-memory.
#[allow(dead_code)]
const T8152_DRAM_BASE: u64 = 0x100_0000_0000;
/// J873 DRAM end: /chosen dram-base 0x100_0000_0000 + dram-size 24 GiB.
#[allow(dead_code)]
const T8152_DRAM_END: u64 = 0x106_0000_0000;
/// (memory-region name, exact size): 26A428 GFX/GFX1 __DATA, then the
/// __DATA_SHARED_RO segments (firmware VA +0x10c000 / +0x108000).
#[allow(dead_code)]
const T8152_FIRMWARE_DATA: [(&CStr, u64); 2] = [
    (c_str!("gfx-data"), 0xbc000),
    (c_str!("gfx1-data"), 0xb4000),
];
#[allow(dead_code)]
const T8152_FIRMWARE_SHARED_RO: [(&CStr, u64); 2] = [
    (c_str!("gfx-data-shared-ro"), 0x8000),
    (c_str!("gfx1-data-shared-ro"), 0x8000),
];
/// Size of one RTKit crash view (ADT iop-gfx(1)-nub coredump-copy-buffer).
#[allow(dead_code)]
const T8152_CRASH_VIEW_SIZE: u64 = 0x8000;

fn uat_geometry(cfg: UatConfig) -> Result<UatGeometry> {
    // 39-bit (G13/G14) and 42-bit (G15+) roots are the two recovered
    // geometries; anything else is undefined.
    UatGeometry::new(cfg.ias).ok_or(ENODEV)
}

/// Lower/user VA range for this SoC's root geometry (one page up to the
/// per-root translation top: `1 << 39` on AGX2, `1 << 42` on AGX3).
#[allow(dead_code)]
pub(crate) fn iova_user_range(cfg: &hw::HwConfig) -> Result<Range<u64>> {
    Ok(IOVA_USER_BASE..uat_geometry(UatConfig::from_hw(cfg))?.user_va_top())
}

/// The unknown-page VA for this SoC's root geometry (two pages below the
/// user top, reproducing the AGX2-proven placement under either root width).
pub(crate) fn iova_unk_page(cfg: &hw::HwConfig) -> Result<u64> {
    Ok(uat_geometry(UatConfig::from_hw(cfg))?.user_va_top() - 2 * UAT_PGSZ as u64)
}

/// Usable lower/user VA range for this SoC (excludes the unknown page).
pub(crate) fn iova_user_usable_range(cfg: &hw::HwConfig) -> Result<Range<u64>> {
    Ok(IOVA_USER_BASE..iova_unk_page(cfg)?)
}

/// Driver-managed kernel VA range within the upper root for this SoC.
///
/// `0xffffffa000000000..0xffffffb000000000` for 39-bit roots (the
/// AGX2-proven placement) and the same window offset under the bit-42
/// selector (`0xfffffc2000000000..0xfffffc3000000000`) for 42-bit roots.
fn iova_kern_range(cfg: UatConfig) -> Result<Range<u64>> {
    let geometry = uat_geometry(cfg)?;
    Ok(geometry.kernel_va_base()..geometry.kernel_va_top())
}

const TTBR_VALID: u64 = 0x1; // BIT(0)
const TTBR_ASID_SHIFT: usize = 48;

/// Address of a special dummy page?
//const IOVA_UNK_PAGE: u64 = 0x6f_ffff8000;
pub(crate) const IOVA_UNK_PAGE: u64 = IOVA_USER_TOP - 2 * UAT_PGSZ as u64;

/// A pre-allocated memory region for UAT management
struct UatRegion {
    base: PhysicalAddr,
    map: io::mem::Mem,
}

/// SAFETY: It's safe to share UAT region records across threads.
unsafe impl Send for UatRegion {}
/// SAFETY: It's safe to share UAT region records across threads.
unsafe impl Sync for UatRegion {}

/// Handoff region flush info structure
#[repr(C)]
struct FlushInfo {
    state: AtomicU64,
    addr: AtomicU64,
    size: AtomicU64,
}

/// UAT Handoff region layout
#[repr(C)]
struct Handoff {
    magic_ap: AtomicU64,
    magic_fw: AtomicU64,

    lock_ap: AtomicU8,
    lock_fw: AtomicU8,
    // Implicit padding: 2 bytes
    turn: AtomicU32,
    cur_slot: AtomicU32,
    // Implicit padding: 4 bytes
    flush: [FlushInfo; UAT_NUM_CTX + 1],

    unk2: AtomicU8,
    // Implicit padding: 7 bytes
    unk3: AtomicU64,
}

const HANDOFF_SIZE: usize = size_of::<Handoff>();

/// One VM slot in the TTBAT
#[repr(C)]
struct SlotTTBS {
    ttb0: AtomicU64,
    ttb1: AtomicU64,
}

const SLOTS_SIZE: usize = UAT_NUM_CTX * size_of::<SlotTTBS>();

// We need at least page 0 (ttb0)
const PAGETABLES_SIZE: usize = UAT_PGSZ;

// Retained renders can prove their mappings unchanged across unrelated BO
// changes without walking every retained PTE on each submission.
const MAPPING_VALIDATION_HISTORY: usize = 128;

/// Read-mostly mapping proof version. Writers hold this VM's GPUVM exec lock;
/// nested leaf helpers keep the version unavailable until the outermost exits.
/// u64::MAX permanently refuses cached proofs after epoch overflow.
struct MappingValidationState {
    visible: AtomicU64,
    pending: AtomicU64,
    writers: AtomicU32,
    /// v94 `g17p_mem_path` bit3 (REACH_LOCKLESS): a lock-free copy of the
    /// change history, written under the exec lock only while the bit is set.
    /// Each entry is its own seqlock (epoch MAX while written); readers also
    /// double-collect `visible`. A skipped or stale entry has the wrong epoch
    /// and makes the reader refuse, so toggling the bit at runtime is safe.
    history: [MappingHistoryEntry; MAPPING_VALIDATION_HISTORY],
}

struct MappingHistoryEntry {
    epoch: AtomicU64,
    start: AtomicU64,
    end: AtomicU64,
}

impl MappingHistoryEntry {
    const fn new() -> Self {
        Self { epoch: AtomicU64::new(u64::MAX), start: AtomicU64::new(0), end: AtomicU64::new(0) }
    }
}

/// Result of one history walk: the epoch walked to, whether any change
/// touched the caller's ranges, and how many changes were examined.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct MappingChangeWalk {
    pub(crate) epoch: u64,
    pub(crate) touched: bool,
    pub(crate) walked: u32,
    /// CNTPCT when the exec lock was held (0 for the lock-free walk or when
    /// the caller did not ask for timing).
    pub(crate) locked_at: u64,
}

impl MappingValidationState {
    fn new() -> Self {
        Self { visible: AtomicU64::new(0), pending: AtomicU64::new(0),
            writers: AtomicU32::new(0),
            history: [const { MappingHistoryEntry::new() }; MAPPING_VALIDATION_HISTORY] }
    }

    /// Writer side (exec lock held, after `begin`): publish one change.
    fn record_history(&self, epoch: u64, start: u64, end: u64) {
        let entry = &self.history[epoch as usize % MAPPING_VALIDATION_HISTORY];
        entry.epoch.store(u64::MAX, Ordering::Relaxed);
        // Orders the invalidation (and `begin`'s swap) before the new range.
        fence(Ordering::Release);
        entry.start.store(start, Ordering::Relaxed);
        entry.end.store(end, Ordering::Relaxed);
        entry.epoch.store(epoch, Ordering::Release);
    }

    /// Lock-free walk of (previous, current]: `current` must be the epoch the
    /// caller already observed through `snapshot`. Refuses (None) on history
    /// loss, a skipped/torn entry, or any mutation that became visible (or
    /// started) during the walk.
    fn changes_since(&self, previous: u64, current: u64,
        mut intersects: impl FnMut(Range<u64>) -> bool) -> Option<MappingChangeWalk> {
        let count = current.checked_sub(previous)?;
        if count > MAPPING_VALIDATION_HISTORY as u64 { return None; }
        if self.snapshot()? != current { return None; }
        let mut touched = false;
        let mut walked = 0u32;
        // `count == 0` needs no entry (and `previous + 1` could overflow).
        for epoch in (previous..current).map(|epoch| epoch + 1) {
            let entry = &self.history[epoch as usize % MAPPING_VALIDATION_HISTORY];
            let first = entry.epoch.load(Ordering::Acquire);
            let start = entry.start.load(Ordering::Relaxed);
            let end = entry.end.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if first != epoch || entry.epoch.load(Ordering::Relaxed) != epoch { return None; }
            walked += 1;
            if intersects(start..end) { touched = true; break; }
        }
        fence(Ordering::Acquire);
        if self.visible.load(Ordering::Relaxed) != current { return None; }
        Some(MappingChangeWalk { epoch: current, touched, walked, locked_at: 0 })
    }

    fn begin(&self) {
        self.writers.fetch_add(1, Ordering::Relaxed);
        // Acquire orders the following leaf writes after invalidation; Release
        // orders any previous transaction before this one becomes unavailable.
        self.visible.swap(u64::MAX, Ordering::AcqRel);
    }

    fn record(&self, epoch: Option<u64>) {
        self.pending.store(epoch.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    fn finish(&self) {
        if self.writers.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.visible.store(self.pending.load(Ordering::Relaxed), Ordering::Release);
        }
    }

    fn snapshot(&self) -> Option<u64> {
        let epoch = self.visible.load(Ordering::Acquire);
        (epoch != u64::MAX).then_some(epoch)
    }
}

/// Must be dropped before releasing the corresponding GPUVM exec lock, also
/// on partial failure. It owns no PTEs and cannot authorize mapping changes.
#[must_use = "retain the mutation guard until leaf writes and invalidations finish"]
struct MappingValidationMutation(Option<Arc<MappingValidationState>>);

impl MappingValidationMutation {
    fn new(state: Option<Arc<MappingValidationState>>) -> Self {
        if let Some(state) = state.as_ref() { state.begin(); }
        Self(state)
    }
}

impl Drop for MappingValidationMutation {
    fn drop(&mut self) {
        if let Some(state) = self.0.as_ref() { state.finish(); }
    }
}

#[derive(Clone, Copy)]
struct MappingValidationChange {
    epoch: u64,
    start: u64,
    end: u64,
}

/// Inner data for a Vm instance. This is reference-counted by the outer Vm object.
struct VmInner {
    dev: driver::AsahiDevRef,
    is_kernel: bool,
    va_range: Range<u64>,
    page_table: UatPageTable,
    /// Changes before every attempted leaf or permission mutation while the
    /// GPUVM exec lock is held. A failed attempt may invalidate a cached
    /// reachability result conservatively. Overflow disables caching.
    mapping_validation_epoch: Option<u64>,
    mapping_validation: Option<Arc<MappingValidationState>>,
    mapping_validation_changes: Option<KBox<[Option<MappingValidationChange>; MAPPING_VALIDATION_HISTORY]>>,
    mm: mm::Allocator<(), KernelMappingInner>,
    uat_inner: Arc<UatInner>,
    binding: Arc<Mutex<VmBinding>>,
    id: u64,
}

impl VmInner {
    fn note_mapping_validation_change(&mut self, range: Range<u64>) -> MappingValidationMutation {
        let mutation = MappingValidationMutation::new(self.mapping_validation.clone());
        self.mapping_validation_epoch = self.mapping_validation_epoch
            .and_then(|epoch| epoch.checked_add(1));
        // A PTE covers the whole page. Record that leaf even if the caller's
        // byte range is narrower, so an adjacent graph predicate cannot be
        // incorrectly treated as untouched.
        let mask = UAT_PGSZ as u64 - 1;
        let end = range.end.checked_add(mask).map(|end| end & !mask);
        if range.start >= range.end || end.is_none() {
            self.mapping_validation_epoch = None;
        }
        if let (Some(epoch), Some(changes)) = (
            self.mapping_validation_epoch,
            self.mapping_validation_changes.as_mut(),
        ) {
            changes[epoch as usize % MAPPING_VALIDATION_HISTORY] =
                Some(MappingValidationChange {
                    epoch,
                    start: range.start & !mask,
                    end: end.unwrap(),
                });
            if crate::g17_mem_path::enabled(crate::g17_mem_path::REACH_LOCKLESS) {
                if let Some(state) = self.mapping_validation.as_ref() {
                    state.record_history(epoch, range.start & !mask, end.unwrap());
                }
            }
        }
        if crate::g17_mem_path::enabled(crate::g17_mem_path::MEM_STATS) {
            crate::g17_mem_path::note_vm_change(self.is_kernel, range.start);
        }
        if let Some(state) = self.mapping_validation.as_ref() {
            state.record(self.mapping_validation_epoch);
        }
        mutation
    }
}

/// Slot binding-related inner data for a Vm instance.
struct VmBinding {
    active_users: usize,
    binding: Option<slotalloc::Guard<SlotInner>>,
    bind_token: Option<slotalloc::SlotToken>,
    /// Hardware TTBAT context this VM's slot maps to.
    ///
    /// Sticky exactly like `bind_token`, and for the same reason: an unbound
    /// VM still has to invalidate the ASID it last owned. It cannot be
    /// recomputed from the token, because on T8140 slot index and hardware
    /// context are no longer the same number.
    bind_context: Option<u32>,
    ttb: u64,
}

struct VmBoInner {
    sgt: Option<shmem::SGTable<gem::AsahiObject>>,
    sg_vec: Option<KVVec<(usize, Range<usize>)>>,
}

/// Data associated with a VM <=> BO pairing
#[pin_data]
struct VmBo {
    #[pin]
    inner: Mutex<VmBoInner>,
}

impl gpuvm::DriverGpuVmBo for VmBo {
    fn new() -> impl PinInit<Self> {
        pin_init!(VmBo {
            inner <- new_mutex!(VmBoInner {
                sgt: None,
                sg_vec: None,
            }, "VmBinding"),
        })
    }
}

#[derive(Default)]
struct StepContext {
    new_va: Option<Pin<KBox<gpuvm::GpuVa<VmInner>>>>,
    prev_va: Option<Pin<KBox<gpuvm::GpuVa<VmInner>>>>,
    next_va: Option<Pin<KBox<gpuvm::GpuVa<VmInner>>>>,
    vm_bo: Option<ARef<gpuvm::GpuVmBo<VmInner>>>,
    prot: Prot,
}

/// Per-map host resources reserved before a VM_BIND batch starts changing
/// visible GPU mappings. Page-table allocation stays in the ordinary GPUVM
/// commit callback, under its one interruptible dma-resv acquisition.
pub(crate) struct PreparedUserMap {
    gem: ARef<gem::Object>,
    ctx: StepContext,
    addr: u64,
    size: u64,
    offset: u64,
    single_page: bool,
}
no_debug!(PreparedUserMap);

/// Split GPUVA nodes reserved before an unmap is committed.
pub(crate) struct PreparedUserUnmap {
    ctx: StepContext,
    iova: u64,
    size: u64,
}
no_debug!(PreparedUserUnmap);

pub(crate) enum PreparedUserBindOp {
    Map(PreparedUserMap),
    Unmap(PreparedUserUnmap),
}
no_debug!(PreparedUserBindOp);

pub(crate) struct PreparedUserBindBatch {
    ops: KVec<PreparedUserBindOp>,
}
no_debug!(PreparedUserBindBatch);

impl PreparedUserBindBatch {
    pub(crate) fn new(capacity: usize) -> Result<Self> {
        Ok(Self {
            ops: KVec::with_capacity(capacity, GFP_KERNEL)?,
        })
    }

    pub(crate) fn push_map(&mut self, map: PreparedUserMap) -> Result {
        Ok(self.ops.push(PreparedUserBindOp::Map(map), GFP_KERNEL)?)
    }

    pub(crate) fn push_unmap(&mut self, unmap: PreparedUserUnmap) -> Result {
        Ok(self
            .ops
            .push(PreparedUserBindOp::Unmap(unmap), GFP_KERNEL)?)
    }
}

impl gpuvm::DriverGpuVm for VmInner {
    type Driver = driver::AsahiDriver;
    type GpuVmBo = VmBo;
    type StepContext = StepContext;

    fn step_map(
        self: &mut gpuvm::UpdatingGpuVm<'_, Self>,
        op: &mut gpuvm::OpMap<Self>,
        ctx: &mut Self::StepContext,
    ) -> Result {
        let mut iova = op.addr();
        let mut left = op.range() as usize;
        let mut offset = op.offset() as usize;

        let bo = ctx.vm_bo.as_ref().expect("step_map with no BO");

        let one_page = op.flags().contains(gpuvm::GpuVaFlags::REPEAT);

        // GPUVM cannot see driver-owned KernelMappings.  Preflight the full
        // object range in the authoritative page table and allocate every
        // intermediate table before the first scatterlist leaf is written.
        let _validation_change = self.note_mapping_validation_change(op.addr()..op.addr().checked_add(op.range()).ok_or(EOVERFLOW)?);
        self.page_table
            .prepare_map(op.addr()..op.addr().checked_add(op.range()).ok_or(EOVERFLOW)?)?;

        let mut do_map = |mut addr: usize, mut len: usize, offset: &mut usize| -> Result<bool> {
            if left == 0 {
                return Ok(false);
            }

            if *offset > 0 {
                let skip = len.min(*offset);
                addr += skip;
                len -= skip;
                *offset -= skip;
            }
            if len == 0 {
                return Ok(true);
            }
            assert!(*offset == 0);

            if one_page {
                len = left;
            } else {
                len = len.min(left);
            }

            mod_dev_dbg!(
                self.dev,
                "MMU: map: {:#x}:{:#x} -> {:#x} [OP={}]\n",
                addr,
                len,
                iova,
                one_page
            );

            self.page_table.map_pages(
                iova..(iova + len as u64),
                addr as PhysicalAddr,
                ctx.prot,
                one_page,
            )?;

            left -= len;
            iova += len as u64;
            Ok(true)
        };

        let guard = bo.inner().inner.lock();
        if let Some(sg_vec) = guard.sg_vec.as_ref() {
            let start_idx = sg_vec
                .binary_search_by(|range| {
                    if range.0 > offset {
                        cmp::Ordering::Greater
                    } else if (range.0 + range.1.len()) <= offset {
                        cmp::Ordering::Less
                    } else {
                        cmp::Ordering::Equal
                    }
                })
                .expect("sg_vec does not contain offset???");

            offset -= sg_vec[start_idx].0 as usize;

            for cur in start_idx..sg_vec.len() {
                let addr = sg_vec[cur].1.start as usize;
                let len: usize = sg_vec[cur].1.len() as usize;
                if do_map(addr, len, &mut offset)? == false {
                    break;
                }
            }
        } else {
            for range in guard.sgt.as_ref().expect("step_map with no SGT").iter() {
                // TODO: proper DMA address/length handling
                let addr = range.dma_address() as usize;
                let len: usize = range.dma_len() as usize;
                if do_map(addr, len, &mut offset)? == false {
                    break;
                }
            }
        }

        let gpuva = ctx.new_va.take().expect("Multiple step_map calls");

        self.flush_m5_app_range(op.addr(), op.range() as usize);

        if op
            .map_and_link_va(
                self,
                gpuva,
                ctx.vm_bo.as_ref().expect("step_map with no BO"),
            )
            .is_err()
        {
            dev_err!(
                self.dev.as_ref(),
                "map_and_link_va failed: {:#x} [{:#x}] -> {:#x}\n",
                op.offset(),
                op.range(),
                op.addr()
            );
            return Err(EINVAL);
        }

        // A T8140 VM can already be published through one or more
        // independently leased compute/render contexts when userspace adds a
        // new mapping. Those contexts cache invalid translations too. The
        // historical fixed-compute-context path happened to invalidate its
        // ASID again at every submit, masking this omission; a real per-queue
        // context instead kept the negative entry and faulted on the newly
        // mapped scratch page. Invalidate only the changed range, in exactly
        // the contexts which currently name this page-table root.
        self.tlbi_live_contexts(op.addr(), op.range());
        Ok(())
    }
    fn step_unmap(
        self: &mut gpuvm::UpdatingGpuVm<'_, Self>,
        op: &mut gpuvm::OpUnMap<Self>,
        _ctx: &mut Self::StepContext,
    ) -> Result {
        let va = op.va().expect("step_unmap: missing VA");
        mod_dev_dbg!(self.dev, "MMU: unmap: {:#x}:{:#x}\n", va.addr(), va.range());

        let _validation_change = self.note_mapping_validation_change(va.addr()..va.addr().checked_add(va.range()).ok_or(EOVERFLOW)?);
        self.page_table
            .unmap_pages(va.addr()..(va.addr() + va.range()))?;

        self.flush_m5_app_range(va.addr(), va.range() as usize);
        self.tlbi_live_contexts(va.addr(), va.range());

        if op.unmap_and_unlink_va_defer().is_none() {
            dev_err!(self.dev.as_ref(), "step_unmap: could not unlink gpuva");
        }
        Ok(())
    }
    fn step_remap(
        self: &mut gpuvm::UpdatingGpuVm<'_, Self>,
        op: &mut gpuvm::OpReMap<Self>,
        vm_bo: &gpuvm::GpuVmBo<Self>,
        ctx: &mut Self::StepContext,
    ) -> Result {
        let va = op.unmap().va().expect("No previous VA");
        let orig_addr = va.addr();
        let orig_range = va.range();

        // Only unmap the hole between prev/next, if they exist
        let unmap_start = if let Some(op) = op.prev_map() {
            op.addr() + op.range()
        } else {
            orig_addr
        };

        let unmap_end = if let Some(op) = op.next_map() {
            op.addr()
        } else {
            orig_addr + orig_range
        };

        mod_dev_dbg!(
            self.dev,
            "MMU: unmap for remap: {:#x}..{:#x} (from {:#x}:{:#x})\n",
            unmap_start,
            unmap_end,
            orig_addr,
            orig_range
        );

        let unmap_range = unmap_end - unmap_start;

        let _validation_change = self.note_mapping_validation_change(unmap_start..unmap_end);
        self.page_table.unmap_pages(unmap_start..unmap_end)?;

        self.flush_m5_app_range(unmap_start, unmap_range as usize);
        self.tlbi_live_contexts(unmap_start, unmap_range);

        if op.unmap().unmap_and_unlink_va_defer().is_none() {
            dev_err!(self.dev.as_ref(), "step_unmap: could not unlink gpuva");
        }

        if let Some(prev_op) = op.prev_map() {
            let prev_gpuva = ctx
                .prev_va
                .take()
                .expect("Multiple step_remap calls with prev_op");
            if prev_op.map_and_link_va(self, prev_gpuva, vm_bo).is_err() {
                dev_err!(self.dev.as_ref(), "step_remap: could not relink prev gpuva");
                return Err(EINVAL);
            }
        }

        if let Some(next_op) = op.next_map() {
            let next_gpuva = ctx
                .next_va
                .take()
                .expect("Multiple step_remap calls with next_op");
            if next_op.map_and_link_va(self, next_gpuva, vm_bo).is_err() {
                dev_err!(self.dev.as_ref(), "step_remap: could not relink next gpuva");
                return Err(EINVAL);
            }
        }

        Ok(())
    }
}

impl VmInner {
    /// Returns the slot index, if this VM is bound.
    fn slot(&self) -> Option<u32> {
        if self.is_kernel {
            // The GFX ASC does not care about the ASID. Pick an arbitrary one.
            // TODO: This needs to be a persistently reserved ASID once we integrate
            // with the ARM64 kernel ASID machinery to avoid overlap.
            Some(0)
        } else {
            // We don't check whether we lost the slot, which could cause unnecessary
            // invalidations against another Vm. However, this situation should be very
            // rare (e.g. a Vm lost its slot, which means 63 other Vms bound in the
            // interim, and then it gets killed / drops its mappings without doing any
            // final rendering). Anything doing active maps/unmaps is probably also
            // rendering and therefore likely bound.
            self.binding.lock().bind_context
        }
    }

    /// Returns the translation table base for this Vm
    fn ttb(&self) -> u64 {
        self.page_table.ttb()
    }

    /// Invalidate this VM's changed range in every hardware context which
    /// currently publishes its root. G17P compute uses independently leased
    /// application contexts, so invalidating only the ordinary render slot
    /// leaves stale executable translations live across a USC unbind/rebind.
    fn tlbi_live_contexts(&self, addr: u64, size: u64) {
        let contexts = {
            let shared = self.uat_inner.lock();
            context_mask_for_root(self.ttb(), shared.ttbs())
        };

        if contexts == 0 {
            return;
        }

        fence(Ordering::SeqCst);
        for context_id in 0..UAT_NUM_CTX {
            if contexts & (1u64 << context_id) != 0 {
                if self.uat_inner.t8140 {
                    mem::tlbi_range_t8140(context_id as u8, addr as usize, size as usize);
                } else {
                    mem::tlbi_range(context_id as u8, addr as usize, size as usize);
                }
            }
        }
        mem::sync();
    }

    /// Map an `mm::Node` representing an mapping in VA space.
    fn map_node(&mut self, node: &mm::Node<(), KernelMappingInner>, prot: Prot) -> Result {
        let mut iova = node.start();
        let mapping_end = iova
            .checked_add(node.mapped_size as u64)
            .ok_or(EOVERFLOW)?;
        // The private fixed-mapping allocator cannot see GPUVM-owned leaves.
        // Reserve the complete page-table walk before installing any SG run.
        let _validation_change = self.note_mapping_validation_change(iova..mapping_end);
        self.page_table.prepare_map(iova..mapping_end)?;
        let guard = node.bo.as_ref().ok_or(EINVAL)?.get()?.inner().inner.lock();
        let sgt = guard.sgt.as_ref().ok_or(EINVAL)?;
        let mut offset = node.offset;
        let mut left = node.mapped_size;

        for range in sgt.iter() {
            if left == 0 {
                break;
            }

            // TODO: proper DMA address/length handling
            let mut addr = range.dma_address() as usize;
            let mut len: usize = range.dma_len() as usize;

            if (offset | addr | len | iova as usize) & UAT_PGMSK != 0 {
                dev_err!(
                    self.dev.as_ref(),
                    "MMU: KernelMapping {:#x}:{:#x} -> {:#x} is not page-aligned\n",
                    addr,
                    len,
                    iova
                );
                return Err(EINVAL);
            }

            if offset > 0 {
                let skip = len.min(offset);
                addr += skip;
                len -= skip;
                offset -= skip;
            }

            len = len.min(left);

            if len == 0 {
                continue;
            }

            mod_dev_dbg!(
                self.dev,
                "MMU: map: {:#x}:{:#x} -> {:#x}\n",
                addr,
                len,
                iova
            );

            self.page_table.map_pages(
                iova..(iova + len as u64),
                addr as PhysicalAddr,
                prot,
                false,
            )?;

            iova += len as u64;
            left -= len;
        }
        // Fixed driver mappings can be added after this root is already live.
        // G17P TVB growth is the important case: firmware requests more blocks
        // while the render application context remains published, and the GPU
        // may already have cached a negative translation for those reserved
        // addresses.  GPUVM's step_map applies the same matching-root range
        // invalidation after installing user PTEs.  Do it here only after the
        // complete fixed mapping is visible, before its address is published
        // through the parameter-buffer inventory.
        self.tlbi_live_contexts(node.start(), node.mapped_size as u64);
        Ok(())
    }
}

/// Shared reference to a virtual memory address space ([`Vm`]).
#[derive(Clone)]
pub(crate) struct Vm {
    id: u64,
    kernel_range: Range<u64>,
    mapping_validation: Option<Arc<MappingValidationState>>,
    inner: ARef<gpuvm::GpuVm<VmInner>>,
    dummy_obj: ARef<gem::Object>,
    binding: Arc<Mutex<VmBinding>>,
    driver_mappings: Arc<Mutex<Option<VmDriverMappings>>>,
    /// Only the lazy, VM-wide context alias installation transaction. Pair
    /// graph allocation and command preparation never take this gate.
    t8140_context_alias_install: Option<Arc<Mutex<()>>>,
    job_lifetime: Arc<Mutex<T8140VmJobLifetime>>,
    mapping_gate: Arc<T8140VmMappingGate>,
    status: Arc<crate::g17_status::VmStatus>,
    /// User bindings which are eligible for native's restricted context-0
    /// view. This records backing identity only; it never changes context 0
    /// until the VM owns a render submission.
    t8140_native_context_bindings: Option<Arc<Mutex<T8140NativeContextBindings>>>,
    /// Canonical context view 0 is unavailable. One cheap submit load;
    /// the registry lock confirms missing state before refusing publication.
    t8140_context_view0_unbound: Arc<AtomicBool>,
    /// `g17p_tile_backstop` (D): user bindings of shareable GEM objects.
    shared_bindings: Arc<SharedBindings>,
}
no_debug!(Vm);

/// One user binding of a shareable (non VM-private) GEM object: GPU VA range
/// and the object offset its start maps. The object is named by address only
/// (identity; never dereferenced). A stale entry can only make the tile
/// backstop order two renders that did not need it.
struct SharedBinding {
    start: u64,
    end: u64,
    object: usize,
    offset: u64,
}

/// `g17p_tile_backstop` (D): the shareable-object bindings of one VM, so a
/// render's attachment span can also be named as (object, offset range),
/// which two processes mapping one dma-buf share. `count` keeps the common
/// no-shared-binding case to one atomic load per render.
#[pin_data]
struct SharedBindings {
    #[pin]
    list: Mutex<KVec<SharedBinding>>,
    count: AtomicU32,
}

struct T8140VmJobLifetime {
    active: usize,
    /// At least one accepted job may still be dereferenced by firmware after
    /// its error fence has signalled. Healthy synchronized VM_BIND unmaps stay
    /// immediate; only this lost-VM state defers them to processor stop.
    quarantined: bool,
    /// The final job guard is applying deferred close/unmap work. New jobs
    /// cannot enter until that teardown is complete.
    draining: bool,
    closed: bool,
    close_ranges: Option<(Range<u64>, Range<u64>)>,
    deferred_unmaps: KVec<(u64, u64)>,
    closed_objects: KVec<ARef<gem::Object>>,
}

struct T8140VmMappingGateState {
    blocked: bool,
    commits: usize,
}

/// Serializes only the already-prepared leaf commit against lost-job
/// quarantine.  In particular, this is acquired after drm_exec has locked the
/// batch's BOs, so it neither expands dma-resv lock ordering into VM_BIND
/// preparation nor turns an interruptible reservation wait into a deadlock.
#[pin_data]
struct T8140VmMappingGate {
    #[pin]
    state: Mutex<T8140VmMappingGateState>,
    #[pin]
    idle: CondVar,
}

struct T8140VmMappingCommitGuard {
    gate: Arc<T8140VmMappingGate>,
}

impl Drop for T8140VmMappingCommitGuard {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock();
        debug_assert!(state.commits != 0);
        state.commits -= 1;
        if state.commits == 0 {
            self.gate.idle.notify_all();
        }
    }
}

/// GPU ownership outlives a DRM handle or file. Deferred user mappings retain
/// their GEM backing until all accepted jobs retire; quarantined packets keep
/// this guard until the processors have provably stopped.
pub(crate) struct T8140VmJobGuard { vm: Vm }
impl T8140VmJobGuard {
    /// Mark this VM as retaining a packet whose firmware ownership was not
    /// disproved. The sticky VM failure prevents new work; explicit unbinds
    /// are held until the last job guard reaches the processor-stop drain.
    pub(crate) fn quarantine(&self) {
        // Close admission before publishing the error fence. A VM_BIND which
        // already owns its dma-resv transaction finishes first; one which was
        // sleeping on that interruptible acquisition observes `blocked` after
        // it wakes and cannot mutate a firmware-owned mapping.
        let mut gate = self.vm.mapping_gate.state.lock();
        gate.blocked = true;
        while gate.commits != 0 {
            self.vm.mapping_gate.idle.wait(&mut gate);
        }
        self.vm.job_lifetime.lock().quarantined = true;
    }
}
/// v94 `g17p_mem_path` bit0: closed objects captured at a VM-idle instant,
/// released on the unbound workqueue instead of inside the last job's
/// completion and without the VM-wide `draining` admission gate.
#[pin_data]
struct T8140ClosedObjectDrain {
    vm: Vm,
    #[pin]
    objects: Mutex<KVec<ARef<gem::Object>>>,
    #[pin]
    work: kernel::workqueue::Work<T8140ClosedObjectDrain>,
}

kernel::impl_has_work! {
    impl HasWork<Self> for T8140ClosedObjectDrain { self.work }
}

impl kernel::workqueue::WorkItem for T8140ClosedObjectDrain {
    type Pointer = Arc<T8140ClosedObjectDrain>;

    fn run(this: Arc<T8140ClosedObjectDrain>) {
        let objects = core::mem::replace(&mut *this.objects.lock(), KVec::new());
        this.vm.release_idle_closed_objects(objects, &crate::g17_mem_path::DRAIN_ASYNC);
    }
}

impl T8140ClosedObjectDrain {
    /// Queue `objects`; on allocation failure hand them back to the caller.
    fn queue(vm: Vm, objects: KVec<ARef<gem::Object>>) -> core::result::Result<(), KVec<ARef<gem::Object>>> {
        // The initializer takes the list only once allocation has succeeded,
        // so an allocation failure hands it back unchanged.
        let cell = core::cell::Cell::new(Some(objects));
        let result = {
            let cell_ref = &cell;
            Arc::pin_init(pin_init!(T8140ClosedObjectDrain {
                vm,
                objects <- new_mutex!(cell_ref.take().unwrap_or_default(), "T8140ClosedObjectDrain"),
                work <- kernel::new_work!("T8140ClosedObjectDrain::work"),
            }), GFP_KERNEL)
        };
        match result {
            Ok(drain) => {
                let _ = kernel::workqueue::system_unbound().enqueue(drain);
                Ok(())
            }
            Err(_) => Err(cell.take().unwrap_or_default()),
        }
    }
}

impl Drop for T8140VmJobGuard {
    fn drop(&mut self) {
        let mut state = self.vm.job_lifetime.lock();
        state.active -= 1;
        if state.active != 0 { return; }
        // v94 bit0: only closed objects on a healthy VM. They were closed
        // while jobs ran and no job is active now, so none can be reached by
        // an accepted job; release them off this completion path, ungated.
        if crate::g17_mem_path::enabled(crate::g17_mem_path::ASYNC_CLOSED_DRAIN)
            && !state.quarantined && !state.closed && state.close_ranges.is_none()
            && state.deferred_unmaps.is_empty() && !state.closed_objects.is_empty()
        {
            let objects = core::mem::replace(&mut state.closed_objects, KVec::new());
            core::mem::drop(state);
            if let Err(objects) = T8140ClosedObjectDrain::queue(self.vm.clone(), objects) {
                // No memory for the work item: release the captured (still
                // unreachable) list here, still without the gate.
                self.vm.release_idle_closed_objects(objects, &crate::g17_mem_path::DRAIN_INLINE);
            }
            return;
        }
        let drain_started = crate::g17_mem_path::stamp();
        let mut drained_objects = 0u64;
        state.draining = true;
        // Keep admission blocked, but do not hold the lifetime mutex while
        // taking dma-resv locks. A VM_BIND may already own those locks and be
        // waiting to append its now-blocked unmap under this mutex.
        loop {
            let ranges = state.close_ranges.take();
            let deferred_unmaps =
                core::mem::replace(&mut state.deferred_unmaps, KVec::new());
            let objects = core::mem::replace(&mut state.closed_objects, KVec::new());
            core::mem::drop(state);

            if let Some((user, kernel)) = ranges {
                if self.vm.unmap_user_ranges_now(user, kernel).is_err() {
                    pr_err!("G17P VM job retirement: deferred user unmap failed\n");
                }
            } else {
                for (iova, size) in deferred_unmaps {
                    if self.vm.unmap_range(iova, size).is_err() {
                        pr_err!("G17P VM job retirement: deferred range unmap failed\n");
                    }
                }
            }
            drained_objects += objects.len() as u64;
            for object in objects {
                if self.vm.drop_mappings_now(&object).is_err() {
                    pr_err!("G17P VM job retirement: deferred GEM unmap failed\n");
                }
            }

            // Pair with quarantine(): gate -> lifetime is the only nested
            // order. A blocked unbind holds the same gate while appending, so
            // observing an empty set here is an atomic reopen boundary.
            let mut gate = self.vm.mapping_gate.state.lock();
            state = self.vm.job_lifetime.lock();
            if state.close_ranges.is_some()
                || !state.deferred_unmaps.is_empty()
                || !state.closed_objects.is_empty()
            {
                core::mem::drop(gate);
                continue;
            }
            state.quarantined = false;
            state.draining = false;
            if !state.closed {
                gate.blocked = false;
            }
            core::mem::drop(state);
            core::mem::drop(gate);
            self.vm.mapping_gate.idle.notify_all();
            break;
        }
        self.vm.bo_deferred_cleanup();
        if drained_objects != 0 {
            crate::g17_mem_path::DRAIN_INLINE.since(drain_started);
            crate::g17_mem_path::count(&crate::g17_mem_path::DRAIN_OBJECTS, drained_objects);
        }
    }
}


/// Internal submit-path reason for missing canonical context view 0. It is
/// produced only before publication by the job-context alias validator and
/// translated back to ENOENT at ioctl and fence boundaries. Keep this reason
/// distinct from other ENOENT errors, including missing views 1..6.
pub(crate) fn context_unbound_error() -> Error {
    Error::from_errno(-(bindings::ESHUTDOWN as i32))
}

pub(crate) fn is_context_unbound(error: Error) -> bool {
    error.to_errno() == -(bindings::ESHUTDOWN as i32)
}

pub(crate) fn context_refusal_to_uapi(error: Error) -> Error {
    if is_context_unbound(error) { ENOENT } else { error }
}

struct T8140NativeContextBinding {
    gem: ARef<gem::Object>,
    object_offset: usize,
}

struct T8140NativeContextBindings {
    slots: [Option<T8140NativeContextBinding>; T8140_NATIVE_CONTEXT_VIEW_COUNT],
    generation: u64,
}
no_debug!(T8140NativeContextBindings);

/// A complete, same-backing context-0 view retained while one render VM owns
/// the firmware graph. Dropping it removes every leaf before invalidating the
/// two ASIDs that can carry the retained lower root.
pub(crate) struct T8140NativeContextAliases {
    source_vm_id: u64,
    source_generation: u64,
    mappings: KVec<KernelMapping>,
    /// Native's low view 0 inside the submitting VM's own root. Dropped
    /// before the context-0 copies so the client regime loses the leaf first.
    client_mapping: Option<KernelMapping>,
    /// Live UAT state, used at teardown to invalidate every hardware context
    /// that still carries the client root (its render slot plus any
    /// application-GART lease pointing at the same page tables).
    inner: Arc<UatInner>,
}
no_debug!(T8140NativeContextAliases);

impl T8140NativeContextAliases {
    pub(crate) fn matches(&self, vm: &Vm) -> bool {
        self.source_vm_id == vm.id
            && vm
                .t8140_native_context_bindings
                .as_ref()
                .is_some_and(|bindings| {
                    self.source_generation == bindings.lock().generation
                })
    }
}

impl Drop for T8140NativeContextAliases {
    fn drop(&mut self) {
        // KernelMapping removes the leaves first. kernel_lower_vm is installed
        // directly rather than through Vm::bind(), so it has no slot token and
        // its generic drop path cannot infer either hardware ASID.
        //
        // The client copy does infer its render slot, but the same root is also
        // rooted in every live application-GART context, so those ASIDs are
        // invalidated here from the live lease bitmap.
        let client_contexts = self.inner.t8140_app_contexts.load(Ordering::Acquire);
        self.client_mapping = None;
        self.mappings.clear();
        fence(Ordering::SeqCst);
        for view in T8140_NATIVE_CONTEXT_VIEWS {
            mem::tlbi_range_t8140(0, view.context_address as usize, view.size);
            mem::tlbi_range_t8140(1, view.context_address as usize, view.size);
        }
        for context_id in 0..UAT_NUM_CTX {
            if client_contexts & (1u64 << context_id) != 0 {
                mem::tlbi_range_t8140(
                    context_id as u8,
                    T8140_CLIENT_NULL_VIEW_START as usize,
                    T8140_CLIENT_NULL_VIEW_SIZE,
                );
            }
        }
        mem::sync();
    }
}

struct VmDriverMappings {
    mappings: KVec<KernelMapping>,
    reserved_ranges: KVec<Range<u64>>,
    simplefb: Option<KernelMapping>,
    render_pools: [Option<VmRenderPoolMappings>; 64],
    compute_pool: Option<VmRenderPoolMappings>,
}
no_debug!(VmDriverMappings);

/// Cached aliases of one retained render pool in this VM. KernelMapping owns
/// GpuVm<VmInner>, which has no back-reference to Vm::driver_mappings. Do not
/// retain a Vm, VmBind or application-context lease here: those own the outer
/// Vm and would make this cache keep itself alive.
struct VmRenderPoolMappings {
    pool_id: u64,
    /// Number of this pool's TVB blocks mapped in this VM (zero for compute).
    tvb_blocks: usize,
    mappings: KVec<KernelMapping>,
}
no_debug!(VmRenderPoolMappings);

impl VmDriverMappings {
    fn iter_mappings(&self) -> impl Iterator<Item = &KernelMapping> {
        Iterator::chain(
            Iterator::chain(
                Iterator::chain(self.mappings.iter(), self.simplefb.iter()),
                self.compute_pool.iter().flat_map(|pool| pool.mappings.iter()),
            ),
            self.render_pools.iter().flatten().flat_map(|pool| pool.mappings.iter()),
        )
    }
}

fn any_range_overlaps<I>(target: Range<u64>, mut ranges: I) -> bool
where
    I: Iterator<Item = Range<u64>>,
{
    ranges.any(|range| range.overlaps(target.clone()))
}

/// Slot data for a [`Vm`] slot (nothing, we only care about the indices).
pub(crate) struct SlotInner();

/// Which VM currently owns each UAT slot.
///
/// A bind failure is otherwise unattributable: the log says the pool is full
/// and nothing says who filled it. The allocator clears an entry as the slot
/// is returned, so this is exact rather than a last-seen record.
pub(crate) struct SlotOwners {
    vm_ids: [u64; UAT_USER_CTX],
}

impl SlotOwners {
    fn new() -> Self {
        Self {
            vm_ids: [0; UAT_USER_CTX],
        }
    }

    fn record_owner(&mut self, slot: u32, vm_id: u64) {
        if let Some(entry) = self.vm_ids.get_mut(slot as usize) {
            *entry = vm_id;
        }
    }

    /// The owners of the T8140 bind pool only. Deliberately not the whole
    /// 63-entry table: this runs on an error path, and the caller's frame is
    /// on the submit stack.
    fn snapshot(&self) -> [u64; T8140_BIND_SLOTS] {
        core::array::from_fn(|slot| self.vm_ids.get(slot).copied().unwrap_or(0))
    }
}

impl slotalloc::SlotItem for SlotInner {
    type Data = SlotOwners;

    fn release(&mut self, data: &mut Self::Data, slot: u32) {
        if let Some(entry) = data.vm_ids.get_mut(slot as usize) {
            *entry = 0;
        }
    }
}

/// Represents a single user of a binding of a [`Vm`] to a slot.
///
/// The number of users is counted, and the slot will be freed when it drops to 0.
#[derive(Debug)]
pub(crate) struct VmBind(Vm, u32);

impl VmBind {
    /// Returns the slot that this `Vm` is bound to.
    pub(crate) fn slot(&self) -> u32 {
        self.1
    }

    /// The bound `Vm` itself, for diagnostics that need to walk its tables.
    pub(crate) fn vm(&self) -> &Vm {
        &self.0
    }

    /// Whether this retained binding belongs to the supplied DRM VM.
    pub(crate) fn matches(&self, vm: &Vm) -> bool {
        core::ptr::eq(&*self.0.inner, &*vm.inner)
    }

    /// Physical base of the bound VM's page-table root.
    pub(crate) fn root(&self) -> u64 {
        self.0.ttb()
    }

    /// The bound VM's driver-assigned id.
    pub(crate) fn vm_id(&self) -> u64 {
        self.0.id
    }
}

impl Drop for VmBind {
    fn drop(&mut self) {
        let mut binding = self.0.binding.lock();

        assert_ne!(binding.active_users, 0);
        binding.active_users -= 1;
        mod_pr_debug!(
            "MMU: slot {} active users {}\n",
            self.1,
            binding.active_users
        );
        if binding.active_users == 0 {
            binding.binding = None;
        }
    }
}

impl Clone for VmBind {
    fn clone(&self) -> VmBind {
        let mut binding = self.0.binding.lock();

        binding.active_users += 1;
        mod_pr_debug!(
            "MMU: slot {} active users {}\n",
            self.1,
            binding.active_users
        );
        VmBind(self.0.clone(), self.1)
    }
}

/// One dynamically allocated T8140 application-GART context.
///
/// The lease retains the ordinary VM binding for at least as long as its
/// TTBAT alias is live. Dropping it first retracts the exact root pair it
/// installed, invalidates that ASID, and only then returns the ID to the
/// allocator. This mirrors `AGXContextIDManager::alloc` plus
/// `AGXSecureGart::registerContextID` without making an allocator ID a module
/// parameter or letting a recycled page-table page confuse ownership.
/// Root-table ownership is separate from the legacy slot-1 binding. The
/// root lease drops before either enclosing VM owner, so PTE roots remain
/// alive through ASID invalidation and allocator release.
struct T8140ContextRootLease {
    inner: Arc<UatInner>,
    context_id: u8,
    generation: u8,
    low: u64,
    high: u64,
}
no_debug!(T8140ContextRootLease);

pub(crate) struct T8140AppContextLease {
    root: T8140ContextRootLease,
    _bind: VmBind,
}
no_debug!(T8140AppContextLease);

impl T8140AppContextLease {
    pub(crate) fn context_id(&self) -> u32 { u32::from(self.root.context_id) }
    pub(crate) fn generation(&self) -> u8 { self.root.generation }
    pub(crate) fn matches(&self, bind: &VmBind) -> bool {
        self._bind.matches(&bind.0) && self._bind.root() == bind.root()
    }
}

/// An independently rooted execution context owned by its submitted jobs.
/// It never acquires or rewrites slot1: unrelated VMs can retain distinct
/// roots concurrently. Clone an Arc of this lease into each immutable job;
/// releasing a userspace queue must not release an in-flight job's context.
/// Compute contexts use the populated firmware upper root, matching the
/// existing context2/3 path. Render's restricted context0 views remain a
/// separate ownership problem and do not use this API yet.
pub(crate) struct T8140ComputeExecutionContext {
    root: T8140ContextRootLease,
    vm: Vm,
}
no_debug!(T8140ComputeExecutionContext);

impl T8140ComputeExecutionContext {
    pub(crate) fn context_id(&self) -> u32 { u32::from(self.root.context_id) }
    /// Native ContextIDManager increments this byte on allocation/reassignment,
    /// not on submission. Every engine using this lease carries the same byte.
    pub(crate) fn generation(&self) -> u8 { self.root.generation }
    pub(crate) fn vm(&self) -> &Vm { &self.vm }
    pub(crate) fn root(&self) -> u64 { self.vm.ttb() }
    pub(crate) fn vm_id(&self) -> u64 { self.vm.id }
    pub(crate) fn matches(&self, vm: &Vm) -> bool { self.vm.id == vm.id && self.root() == vm.ttb() }
    pub(crate) fn is_current(&self) -> bool {
        let inner = self.root.inner.lock();
        let slot = &inner.ttbs()[self.root.context_id as usize];
        slot.ttb0.load(Ordering::Acquire) == self.root.low
            && slot.ttb1.load(Ordering::Acquire) == self.root.high
    }

}

impl Drop for T8140ContextRootLease {
    fn drop(&mut self) {
        let context_id = self.context_id as usize;
        let mut release = false;
        {
            let inner = self.inner.lock();
            inner.handoff().lock();
            let ttbs = inner.ttbs();
            let low = ttbs[context_id].ttb0.load(Ordering::Acquire);
            let high = ttbs[context_id].ttb1.load(Ordering::Acquire);
            if low == self.low && high == self.high {
                if inner.handoff().current_slot() == Some(self.context_id as u32) {
                    pr_err!(
                        "Uat: dropping live T8140 app context {}; retracting its roots to prevent a stale VM reference\n",
                        context_id,
                    );
                }
                ttbs[context_id].ttb0.store(0, Ordering::Release);
                ttbs[context_id].ttb1.store(0, Ordering::Release);
                release = true;
            } else if low == 0 && high == 0 {
                // A stopped-session reset may have cleared the table first.
                release = true;
            } else {
                // Never make a context reusable while a different root pair
                // is resident. Leaking one ID is safer than aliasing two VMs.
                pr_err!(
                    "Uat: T8140 app context {} root ownership changed while leased: expected [{:#x},{:#x}], found [{:#x},{:#x}]; quarantining ID\n",
                    context_id,
                    self.low,
                    self.high,
                    low,
                    high,
                );
            }
            inner.handoff().unlock();
        }

        if release {
            fence(Ordering::SeqCst);
            mem::tlbi_asid(self.context_id);
            mem::sync();
            self.inner
                .t8140_app_contexts
                .fetch_and(!(1u64 << context_id), Ordering::AcqRel);
        }
    }
}

/// A `drm_gpuvm_bo` reference owned by an Asahi fixed mapping.
///
/// Every Asahi GPUVM uses `DRM_GPUVM_IMMEDIATE_MODE`, so its GEM GPUVA list is
/// protected by `drm_gem_object::gpuva.lock`, not `dma_resv`. The generic Rust
/// `ARef<GpuVmBo>` destructor currently uses the latter. Fixed mappings are
/// particularly likely to expose that mismatch because several aliases of one
/// internal GEM can be created and retired concurrently.
///
/// Consume the Rust reference explicitly through DRM's immediate-mode
/// deferred put. This takes the right lock and keeps both the GEM mutex and
/// GPUVM alive until cleanup runs. Retaining the owner separately makes it
/// safe to drain the deferred list after the put, including when this was the
/// final BO reference.
struct ImmediateGpuVmBoRef {
    bo: Option<ARef<gpuvm::GpuVmBo<VmInner>>>,
    owner: ARef<gpuvm::GpuVm<VmInner>>,
}

impl ImmediateGpuVmBoRef {
    fn new(
        bo: ARef<gpuvm::GpuVmBo<VmInner>>,
        owner: ARef<gpuvm::GpuVm<VmInner>>,
    ) -> Self {
        Self {
            bo: Some(bo),
            owner,
        }
    }

    fn get(&self) -> Result<&gpuvm::GpuVmBo<VmInner>> {
        self.bo.as_deref().ok_or(EINVAL)
    }
}

impl Drop for ImmediateGpuVmBoRef {
    fn drop(&mut self) {
        let Some(bo) = self.bo.take() else {
            return;
        };
        let raw = ARef::into_raw(bo);
        // SAFETY: `GpuVmBo` is `repr(C)` with `drm_gpuvm_bo` as its first
        // field. `into_raw` transfers the live reference to this call, and
        // every Asahi GPUVM is constructed with IMMEDIATE_MODE.
        unsafe {
            bindings::drm_gpuvm_bo_put_deferred(
                raw.as_ptr().cast::<bindings::drm_gpuvm_bo>(),
            );
        }
        self.owner.bo_deferred_cleanup();
    }
}

/// Inner data required for an object mapping into a [`Vm`].
pub(crate) struct KernelMappingInner {
    // Drop order matters:
    // - Deferred-put the GpuVmBo first under its immediate-mode GPUVA lock
    // - Drop the GEM BO next, since BO free can take the resv lock itself
    // - Drop the owner GpuVm last, since that again can take resv locks when the refcount drops to 0
    bo: Option<ImmediateGpuVmBoRef>,
    _gem: Option<ARef<gem::Object>>,
    owner: ARef<gpuvm::GpuVm<VmInner>>,
    uat_inner: Arc<UatInner>,
    prot: Prot,
    offset: usize,
    mapped_size: usize,
}

/// An object mapping into a [`Vm`], which reserves the address range from use by other mappings.
pub(crate) struct KernelMapping(mm::Node<(), KernelMappingInner>);

/// Keeps the originating UAT identity and its final-stop witness alive even
/// when a cached object outlives the platform runtime.
pub(crate) struct FirmwareCacheOwner(Arc<UatInner>);

impl FirmwareCacheOwner {
    pub(crate) fn stopped(&self) -> bool {
        self.0.firmware_cache_stopped.load(Ordering::SeqCst)
    }

    pub(crate) fn matches(&self, uat: &Uat) -> bool {
        core::ptr::eq(&*self.0, &*uat.inner) && !self.stopped()
    }
}

impl KernelMapping {
    pub(crate) fn firmware_cache_owner(&self) -> FirmwareCacheOwner {
        FirmwareCacheOwner(self.0.uat_inner.clone())
    }
    /// Returns the IOVA base of this mapping
    pub(crate) fn iova(&self) -> u64 {
        self.0.start()
    }

    /// Returns the size of this mapping in bytes
    pub(crate) fn size(&self) -> usize {
        self.0.mapped_size
    }

    /// Returns the IOVA base of this mapping
    pub(crate) fn iova_range(&self) -> Range<u64> {
        self.0.start()..(self.0.start() + self.0.mapped_size as u64)
    }

    /// Borrow the mapped GEM subrange through a temporary CPU mapping.
    ///
    /// The caller must serialize CPU access with firmware or GPU writes. This
    /// helper only owns the kernel VMap lifetime and applies the object offset
    /// carried by this mapping.
    pub(crate) fn with_cpu_bytes<R>(
        &self,
        f: impl FnOnce(&mut [u8]) -> Result<R>,
    ) -> Result<R> {
        let gem = self.0._gem.as_ref().ok_or(EINVAL)?;
        let end = self
            .0
            .offset
            .checked_add(self.0.mapped_size)
            .ok_or(EOVERFLOW)?;
        if end > gem.size() {
            return Err(ERANGE);
        }
        let vmap = gem.vmap::<u8>()?;
        let bytes = unsafe {
            // SAFETY: the VMap covers the complete GEM allocation and the
            // checked subrange is retained by this KernelMapping.
            core::slice::from_raw_parts_mut(
                vmap.as_mut_ptr().add(self.0.offset),
                self.0.mapped_size,
            )
        };
        f(bytes)
    }

    /// Map the same GEM subrange into another VM. The caller owns the returned
    /// alias and must retain it for every GPU access that uses its address.
    pub(crate) fn map_alias_into_range(
        &self,
        vm: &Vm,
        range: Range<u64>,
        prot: Prot,
    ) -> Result<KernelMapping> {
        let gem = self.0._gem.as_ref().ok_or(EINVAL)?;
        let end = self
            .0
            .offset
            .checked_add(self.0.mapped_size)
            .ok_or(EOVERFLOW)?;
        vm.map_in_range(
            &*gem,
            self.0.offset..end,
            UAT_PGSZ as u64,
            range,
            prot,
            false,
        )
    }

    /// Map this shared GEM subrange into the end of another mapping's guards.
    /// The caller must keep GPU work quiescent until the complete resource
    /// update is published. Existing mappings are never removed or relocated.
    pub(crate) fn map_alias_into_guard(
        &self, anchor: &mut KernelMapping, prot: Prot,
    ) -> Result<KernelMapping> {
        let size = self.size();
        let gem = self.0._gem.as_ref().ok_or(EINVAL)?;
        if size == 0 || size & UAT_PGMSK != 0 || prot.is_cached_noncoherent()
            || !gem.is_kernel_object()
            || self.0.offset.checked_add(size).ok_or(EOVERFLOW)? > gem.size()
        {
            return Err(EINVAL);
        }
        let available = anchor.0.size().checked_sub(anchor.size() as u64).ok_or(EINVAL)?;
        if size as u64 > available { return Err(ENOSPC); }
        let sgt = gem.owned_sg_table()?;
        let owner_ref = anchor.0.owner.clone();
        let mut owner = owner_ref.exec_lock(Some(gem), false)?;
        let vm_bo = owner_ref.obtain_bo(gem)?;
        {
            let mut guard = vm_bo.inner().inner.lock();
            if guard.sgt.is_none() { guard.sgt.replace(sgt); }
        }
        let mut vm_bo = Some(ImmediateGpuVmBoRef::new(vm_bo, owner_ref.clone()));
        // As in map_at_with_guards, the fallible allocator operation must not
        // own DMA-reservation references whose Drop would reacquire this lock.
        let node = anchor.0.as_mut().split_tail(size as u64, KernelMappingInner {
            owner: owner_ref.clone(), uat_inner: owner.uat_inner.clone(), prot,
            bo: None, _gem: None, offset: self.0.offset, mapped_size: size,
        });
        let mut node = match node {
            Ok(node) => node,
            Err(error) => { drop(owner); return Err(error); }
        };
        {
            let inner = node.as_mut().inner_mut();
            inner.bo = vm_bo.take();
            inner._gem = Some(gem.clone());
        }
        let mapped = owner.map_node(&node, prot);
        if let Err(error) = mapped {
            // Restore reservation coverage before the temporary mapping is
            // dropped. Its normal Drop unmaps partial PTEs and invalidates
            // every live context; the detached node cannot free the guard.
            let rollback = anchor.0.as_mut().rejoin_tail(node.as_mut());
            drop(owner);
            drop(KernelMapping(node));
            rollback?;
            return Err(error);
        }
        drop(owner);
        Ok(KernelMapping(node))
    }

    /// Undo a staged guard-tail alias before publication. Consume tails in
    /// reverse growth order so each remains adjacent to its original anchor.
    /// The caller owns the tail and must ensure no GPU work can reference it.
    pub(crate) fn rollback_guard_tail(&mut self, mut tail: KernelMapping) -> Result {
        if tail.0.prot.is_cached_noncoherent() { return Err(EINVAL); }
        self.0.as_mut().rejoin_tail(tail.0.as_mut())?;
        // KernelMapping::drop removes the PTEs and flushes live contexts. Its
        // now-invalid MM node leaves the restored guard reservation intact.
        drop(tail);
        Ok(())
    }

    /// v90: give the first `prefix` bytes of a fresh, unpublished kernel
    /// mapping the firmware-cached attribute (UAT memattr 0), as native maps
    /// its pooled command objects (bit41 option 0x20900000084). The mapping's
    /// recorded `prot` stays uncached, so a later drop never enters the
    /// unsupported G17 fwctl flush; the caller owns that lifecycle (pooled
    /// reuse, never an early unmap). Every PTE is read back: the prefix may
    /// differ only in attribute bit 3, all other PTEs must be byte-identical.
    /// Returns `(proof, mutated)`; on error `mutated` says whether a PTE may
    /// already carry the cached attribute (then the caller must never
    /// release the mapping for reuse or unmap it early).
    ///
    /// # Safety
    /// No pointer into this mapping may have been published to firmware/GPU.
    pub(crate) unsafe fn set_fw_cached_prefix(&self, prefix: usize) -> (Result<[u64; 2]>, bool) {
        let mut mutated = false;
        let result = (|| {
            if !self.0.uat_inner.t8140 || self.size() == 0 || prefix == 0 || prefix > self.size()
                || (prefix | self.size()) & (UAT_PGSZ - 1) != 0
                || self.size() > 32 * UAT_PGSZ {
                return Err(EINVAL);
            }
            let mut owner = self.0.owner.exec_lock(None, false)?;
            if !owner.is_kernel { return Err(EINVAL); }
            let pages = self.size() / UAT_PGSZ;
            let cached_pages = prefix / UAT_PGSZ;
            let mut before = [0u64; 32];
            for (index, value) in before[..pages].iter_mut().enumerate() {
                let start = self.iova() + (index * UAT_PGSZ) as u64;
                let mut ranges = [pgtable::MappedRange::default(); 1];
                let (count, truncated) = owner.page_table.collect_mapped_ranges(
                    start..start + UAT_PGSZ as u64, &mut ranges)?;
                if count != 1 || truncated || (ranges[0].pte >> 2) & 7 != 2 {
                    return Err(EINVAL);
                }
                *value = ranges[0].pte;
            }
            let range = self.iova()..self.iova() + prefix as u64;
            let _validation = owner.note_mapping_validation_change(range.clone());
            mutated = true;
            owner.page_table.reprot_pages(range, PROT_GPU_FW_PRIV_RW)?;
            fence(Ordering::SeqCst);
            mem::tlbi_all();
            mem::sync();
            let mut after = [0u64; 32];
            for (index, value) in after[..pages].iter_mut().enumerate() {
                let start = self.iova() + (index * UAT_PGSZ) as u64;
                let mut ranges = [pgtable::MappedRange::default(); 1];
                let (count, truncated) = owner.page_table.collect_mapped_ranges(
                    start..start + UAT_PGSZ as u64, &mut ranges)?;
                if count != 1 || truncated { return Err(EFAULT); }
                *value = ranges[0].pte;
            }
            for index in 0..pages {
                let expected_xor = if index < cached_pages { 8 } else { 0 };
                if before[index] ^ after[index] != expected_xor { return Err(EFAULT); }
            }
            Ok([after[0], after[cached_pages.min(pages - 1)]])
        })();
        (result, mutated)
    }

    /// Remap a cached mapping as uncached, then synchronously flush that range of VAs from the
    /// coprocessor cache. This is required to safely unmap cached/private mappings.
    fn remap_uncached_and_flush(&mut self) {
        let mut owner = self
            .0
            .owner
            .exec_lock(None, false)
            .expect("Failed to exec_lock in remap_uncached_and_flush");

        mod_dev_dbg!(
            owner.dev,
            "MMU: remap as uncached {:#x}:{:#x}\n",
            self.iova(),
            self.size()
        );

        // Remap in-place as uncached.
        // Do not try to unmap the guard page (-1)
        let prot = self.0.prot.as_uncached();
        let _validation_change = owner.note_mapping_validation_change(self.iova_range());
        if owner
            .page_table
            .reprot_pages(self.iova_range(), prot)
            .is_err()
        {
            dev_err!(
                owner.dev.as_ref(),
                "MMU: remap {:#x}:{:#x} failed\n",
                self.iova(),
                self.size()
            );
        }
        fence(Ordering::SeqCst);

        // T8140 builds its UAT and object graph before AsahiData.gpu is
        // initialized. A constructor error can therefore drop cached mappings
        // while owner.dev has no DrmGpu yet. Those mappings were never
        // published to firmware, so the PTE downgrade above is sufficient.
        if !self.0.uat_inner.firmware_cache_flush_ready() {
            mod_dev_dbg!(
                owner.dev,
                "MMU: firmware cache flush not armed for {:#x}:{:#x}\n",
                self.iova(),
                self.size()
            );
            return;
        }

        // If we don't have (and have never had) a VM slot, just return
        let slot = match owner.slot() {
            None => return,
            Some(slot) => slot,
        };

        let flush_slot = if owner.is_kernel {
            // If this is a kernel mapping, always flush on index 64
            UAT_NUM_CTX as u32
        } else {
            // Otherwise, check if this slot is the active one, otherwise return
            // Also check that we actually own this slot
            let ttb = owner.ttb() | TTBR_VALID | (slot as u64) << TTBR_ASID_SHIFT;

            let uat_inner = self.0.uat_inner.lock();
            uat_inner.handoff().lock();
            let cur_slot = uat_inner.handoff().current_slot();
            let ttb_cur = uat_inner.ttbs()[slot as usize].ttb0.load(Ordering::Relaxed);
            uat_inner.handoff().unlock();
            if cur_slot == Some(slot) && ttb_cur == ttb {
                slot
            } else {
                return;
            }
        };

        // FIXME: There is a race here, though it'll probably never happen in practice.
        // In theory, it's possible for the ASC to finish using our slot, whatever command
        // it was processing to complete, the slot to be lost to another context, and the ASC
        // to begin using it again with a different page table, thus faulting when it gets a
        // flush request here. In practice, the chance of this happening is probably vanishingly
        // small, as all 62 other slots would have to be recycled or in use before that slot can
        // be reused, and the ASC using user contexts at all is very rare.

        // Still, the locking around UAT/Handoff/TTBs should probably be redesigned to better
        // model the interactions with the firmware and avoid these races.
        // Possibly TTB changes should be tied to slot locks:

        // Flush:
        //  - Can early check handoff here (no need to lock).
        //      If user slot and it doesn't match the active ASC slot,
        //      we can elide the flush as the ASC guarantees it flushes
        //      TLBs/caches when it switches context. We just need a
        //      barrier to ensure ordering.
        //  - Lock TTB slot
        //      - If user ctx:
        //          - Lock handoff AP-side
        //              - Lock handoff dekker
        //                  - Check TTB & handoff cur ctx
        //      - Perform flush if necessary
        //          - This implies taking the fwring lock
        //
        // TTB change:
        //  - lock TTB slot
        //      - lock handoff AP-side
        //          - lock handoff dekker
        //              change TTB

        // Lock this flush slot, and write the range to it
        let flush = self.0.uat_inner.lock_flush(flush_slot);
        let pages = self.size() >> UAT_PGBIT;
        flush.begin_flush(self.iova(), self.size() as u64);
        if pages >= 0x10000 {
            dev_err!(
                owner.dev.as_ref(),
                "MMU: Flush too big ({:#x} pages))\n",
                pages
            );
        }

        let cmd = fw::channels::FwCtlMsg {
            addr: fw::types::U64(self.iova()),
            unk_8: 0,
            slot: flush_slot,
            page_count: pages as u16,
            unk_12: 2, // ?
        };

        // Tell the firmware to do a cache flush
        if let Err(e) = (*owner.dev).gpu.fwctl(cmd) {
            dev_err!(
                owner.dev.as_ref(),
                "MMU: ASC cache flush {:#x}:{:#x} failed (err: {:?})\n",
                self.iova(),
                self.size(),
                e
            );
        }

        // Finish the flush
        flush.end_flush();

        // Slot is unlocked here
    }
}
no_debug!(KernelMapping);

impl Drop for KernelMapping {
    fn drop(&mut self) {
        // This is the main unmap function for UAT mappings.
        // The sequence of operations here is finicky, due to the interaction
        // between cached GFX ASC mappings and the page tables. These mappings
        // always have to be flushed from the cache before being unmapped.

        // For uncached mappings, just unmapping and flushing the TLB is sufficient.

        // For cached mappings, this is the required sequence:
        // 1. Remap it as uncached
        // 2. Flush the TLB range
        // 3. If kernel VA mapping OR user VA mapping and handoff.current_slot() == slot:
        //    a. Take a lock for this slot
        //    b. Write the flush range to the right context slot in handoff area
        //    c. Issue a cache invalidation request via FwCtl queue
        //    d. Poll for completion via queue
        //    e. Check for completion flag in the handoff area
        //    f. Drop the lock
        // 4. Unmap
        // 5. Flush the TLB range again

        if self.0.prot.is_cached_noncoherent() {
            mod_pr_debug!(
                "MMU: remap as uncached {:#x}:{:#x}\n",
                self.iova(),
                self.size()
            );
            self.remap_uncached_and_flush();
        }

        let mut owner = self
            .0
            .owner
            .exec_lock(None, false)
            .expect("exec_lock failed in KernelMapping::drop");
        mod_dev_dbg!(
            owner.dev,
            "MMU: unmap {:#x}:{:#x}\n",
            self.iova(),
            self.size()
        );

        let _validation_change = owner.note_mapping_validation_change(self.iova_range());
        if owner.page_table.unmap_pages(self.iova_range()).is_err() {
            dev_err!(
                owner.dev.as_ref(),
                "MMU: unmap {:#x}:{:#x} failed\n",
                self.iova(),
                self.size()
            );
        }

        owner.flush_m5_app_range(self.iova(), self.size());

        if let Some(asid) = owner.slot() {
            fence(Ordering::SeqCst);
            if owner.uat_inner.t8140 {
                mem::tlbi_range_t8140(asid as u8, self.iova() as usize, self.size());
            } else {
                mem::tlbi_range(asid as u8, self.iova() as usize, self.size());
            }
            mod_dev_dbg!(
                owner.dev,
                "MMU: flush range: asid={:#x} start={:#x} len={:#x}\n",
                asid,
                self.iova(),
                self.size()
            );
            mem::sync();
        }

        // On T8140 the bind slot is not the only context that publishes this
        // root. kernel_lower_vm is installed directly in contexts 0/1 and has
        // no slot at all, and client roots are also leased to application-GART
        // contexts. A translation any of them still caches outlives the leaf
        // removed above, and this range returns to the VA allocator when the
        // node is released after this function: the next mapping placed there
        // would otherwise be read through the stale entry from the previous
        // owner's freed backing. Invalidate every context naming the root
        // before the address can be reused, as GPUVM unmap already does.
        if owner.uat_inner.t8140 {
            owner.tlbi_live_contexts(self.iova(), self.size() as u64);
        }
    }
}

/// Shared UAT global data structures
struct UatShared {
    kernel_ttb1: u64,
    map_kernel_to_user: bool,
    handoff_rgn: UatRegion,
    ttbs_rgn: UatRegion,
}

impl UatShared {
    /// Returns the handoff region area
    fn handoff(&self) -> &Handoff {
        // SAFETY: pointer is non-null per the type invariant
        unsafe { (self.handoff_rgn.map.ptr() as *mut Handoff).as_ref() }.unwrap()
    }

    /// Returns the TTBAT area
    fn ttbs(&self) -> &[SlotTTBS; UAT_NUM_CTX] {
        // SAFETY: pointer is non-null per the type invariant
        unsafe { (self.ttbs_rgn.map.ptr() as *mut [SlotTTBS; UAT_NUM_CTX]).as_ref() }.unwrap()
    }
}

// SAFETY: Nothing here is unsafe to send across threads.
unsafe impl Send for UatShared {}

/// Inner data for the top-level UAT instance.
#[pin_data]
struct UatInner {
    /// One-way final teardown witness; recovery never sets this.
    firmware_cache_stopped: AtomicBool,
    firmware_cache_flush_ready: AtomicBool,
    /// `Vm::id` of the VM whose root is currently published at the G17P
    /// compute contexts (`T8140_COMPUTE_CTXS`), or 0 for none.
    ///
    /// `VmInner::drop` used to decide whether to retract those entries by
    /// comparing the stored TTB0 against its own root *value*.  A VM's root is
    /// one page from the page allocator, so the page a dying VM frees can be
    /// handed straight back to the next VM created -- at which point the old
    /// VM's drop matches on the new VM's identical root and retracts the
    /// aliases the new VM just published.  Keying on the VM id instead makes
    /// that impossible.
    t8140_alias_owner: AtomicU64,
    /// Allocation bitmap for native-style application-GART context leases.
    /// Reserved and fixed contexts are permanently marked; bits 5..63 are
    /// claimed before their TTBAT roots become visible and released only
    /// after lease teardown invalidates the corresponding ASID.
    t8140_app_contexts: AtomicU64,
    /// M5 application root leases. Contexts 0..2 belong to firmware and its
    /// opening transaction; application queues claim contexts 3..63.
    t8142_app_contexts: AtomicU64,
    /// Per-ID allocation history survives lease release. Native wraps through
    /// zero; this byte is not a validity flag or the queue's submission count.
    t8140_context_generations: [AtomicU8; UAT_NUM_CTX],
    /// T8140 publishes one page-table root in contexts other than a VM's bind
    /// slot, so unmapping must invalidate every context naming that root.
    t8140: bool,
    #[pin]
    shared: Mutex<UatShared>,
    #[pin]
    handoff_flush: [Mutex<HandoffFlush>; UAT_NUM_CTX + 1],
}

impl UatInner {
    fn firmware_cache_flush_ready(&self) -> bool {
        self.firmware_cache_flush_ready.load(Ordering::Acquire)
    }

    /// Take the lock on the shared data and return the guard.
    fn lock(&self) -> Guard<'_, UatShared, MutexBackend> {
        self.shared.lock()
    }

    /// Take a lock on a handoff flush slot and return the guard.
    fn lock_flush(&self, slot: u32) -> Guard<'_, HandoffFlush, MutexBackend> {
        self.handoff_flush[slot as usize].lock()
    }
}

/// Top-level UAT manager object
pub(crate) struct Uat {
    dev: driver::AsahiDevRef,
    cfg: UatConfig,

    inner: Arc<UatInner>,
    slots: slotalloc::SlotAllocator<SlotInner>,

    kernel_vm: Vm,
    kernel_lower_vm: Vm,
    /// Dedicated native context-1 root for the device-global FList/operand
    /// closure. Context 0 has overlapping fixed KSM mappings, so sharing its
    /// lower root cannot represent the render-only final operand block.
    t8140_render_global_vm: Option<Vm>,
    _t8140_firmware_crash_buffers: Option<[KernelMapping; 2]>,
    _t8142_firmware_crash_views: Option<[KernelMapping; 2]>,
    t8140_firmware_crash_gems: Option<[ARef<gem::Object>; 2]>,
    _t8140_parameter_metrics_low: Option<KernelMapping>,
    t8140_parameter_metrics_gem: Option<ARef<gem::Object>>,
    t8140_secondary_ttb1: Option<u64>,
    t8140_empty_high_roots: Option<[Vm; 2]>,
    t8140_context_phase: AtomicU8,
}

impl Handoff {
    /// Lock the handoff region from firmware access.
    ///
    /// Dekker's algorithm against the firmware. Both waits are BOUNDED: they
    /// used to spin without limit on words only the firmware can clear, so a
    /// wedged ASC hung the caller in kernel context forever. That is reachable
    /// -- a render that fails crashes the GFX ASC, and the next submission's
    /// `Uat::bind` takes this lock -- and it was measured as a submission that
    /// never returns, with no task in D state because a spin never sleeps.
    ///
    /// Proceeding after the bound is the right call specifically here: on
    /// T8140 the firmware never writes its half of this protocol at all
    /// (hardware-verified; see `HandoffMode::AbsentStandinT8140` below), so in
    /// the healthy case neither loop iterates and the bound is unreachable.
    /// Reaching it means the peer is gone, and there is no peer to arbitrate
    /// with. A warning beats an unkillable spin.
    fn lock(&self) {
        const HANDOFF_SPIN_LIMIT: u32 = 10_000_000;
        self.lock_ap.store(1, Ordering::Relaxed);
        fence(Ordering::SeqCst);

        let mut spins: u32 = 0;
        while self.lock_fw.load(Ordering::Relaxed) != 0 {
            if self.turn.load(Ordering::Relaxed) != 0 {
                self.lock_ap.store(0, Ordering::Relaxed);
                let mut turn_spins: u32 = 0;
                while self.turn.load(Ordering::Relaxed) != 0 {
                    turn_spins += 1;
                    if turn_spins >= HANDOFF_SPIN_LIMIT {
                        break;
                    }
                }
                self.lock_ap.store(1, Ordering::Relaxed);
                fence(Ordering::SeqCst);
            }
            spins += 1;
            if spins >= HANDOFF_SPIN_LIMIT {
                pr_warn!(
                    "G17P handoff: firmware never released the handoff lock (lock_fw={}, turn={}); proceeding without the peer\n",
                    self.lock_fw.load(Ordering::Relaxed),
                    self.turn.load(Ordering::Relaxed),
                );
                break;
            }
        }
        fence(Ordering::Acquire);
    }

    /// Unlock the handoff region, allowing firmware access
    fn unlock(&self) {
        self.turn.store(1, Ordering::Relaxed);
        self.lock_ap.store(0, Ordering::Release);
    }

    /// Returns the current Vm slot mapped by the firmware for lower/unprivileged access, if any.
    fn current_slot(&self) -> Option<u32> {
        let slot = self.cur_slot.load(Ordering::Relaxed);
        if slot == 0 || slot == u32::MAX {
            None
        } else {
            Some(slot)
        }
    }

    /// Initialize the handoff region.
    ///
    /// In [`HandoffMode::FirmwareDekker`] this blocks (bounded) until the
    /// firmware publishes `magic_fw`. In
    /// [`HandoffMode::AbsentStandinT8140`] it must not: on T8140 the
    /// firmware never writes its half (hardware-verified), so the AP
    /// publishes its own state and proceeds without a peer.
    fn init(&self, mode: HandoffMode) -> Result {
        if mode == HandoffMode::NoPeerT8142
            && (self.magic_fw.load(Ordering::Acquire) != 0
                || self.lock_fw.load(Ordering::Acquire) != 0)
        {
            pr_err!("M5 handoff: unexpected firmware peer; refusing no-peer mode\n");
            return Err(ENODEV);
        }
        if mode == HandoffMode::NoPeerT8152
            && (self.magic_fw.load(Ordering::Acquire) != 0
                || self.lock_fw.load(Ordering::Acquire) != 0)
        {
            pr_err!("T8152 handoff: unexpected firmware peer; refusing no-peer mode\n");
            return Err(ENODEV);
        }
        self.magic_ap.store(PPL_MAGIC, Ordering::Relaxed);
        self.cur_slot.store(0, Ordering::Relaxed);
        self.unk3.store(0, Ordering::Relaxed);
        fence(Ordering::SeqCst);

        if mode == HandoffMode::FirmwareDekker {
            let start = Instant::<Monotonic>::now();
            const TIMEOUT: Delta = Delta::from_millis(1000);

            self.lock();
            while start.elapsed() < TIMEOUT {
                if self.magic_fw.load(Ordering::Relaxed) == PPL_MAGIC {
                    break;
                } else {
                    self.unlock();
                    fsleep(Delta::from_millis(10));
                    self.lock();
                }
            }

            if self.magic_fw.load(Ordering::Relaxed) != PPL_MAGIC {
                self.unlock();
                pr_err!("Handoff: Failed to initialize (firmware not running?)\n");
                return Err(EIO);
            }

            self.unlock();
        }

        for i in 0..=UAT_NUM_CTX {
            self.flush[i].state.store(0, Ordering::Relaxed);
            self.flush[i].addr.store(0, Ordering::Relaxed);
            self.flush[i].size.store(0, Ordering::Relaxed);
        }
        fence(Ordering::SeqCst);
        Ok(())
    }
}

/// Represents a single flush info slot in the handoff region.
///
/// # Invariants
/// The pointer is valid and there is no aliasing HandoffFlush instance.
struct HandoffFlush(*const FlushInfo, HandoffMode);

// SAFETY: These pointers are safe to send across threads.
unsafe impl Send for HandoffFlush {}

impl HandoffFlush {
    /// Set up a flush operation for the coprocessor
    fn begin_flush(&self, start: u64, size: u64) {
        // SAFETY: Per the type invariant, this is safe
        let flush = unsafe { self.0.as_ref().unwrap() };

        let state = flush.state.load(Ordering::Relaxed);
        if state != 0 {
            pr_err!("Handoff: expected flush state 0, got {}\n", state);
        }
        flush.addr.store(start, Ordering::Relaxed);
        flush.size.store(size, Ordering::Relaxed);
        flush.state.store(1, Ordering::Relaxed);
    }

    /// Complete a flush operation for the coprocessor
    fn end_flush(&self) {
        // SAFETY: Per the type invariant, this is safe
        let flush = unsafe { self.0.as_ref().unwrap() };
        let state = flush.state.load(Ordering::Relaxed);
        // In Dekker mode the firmware ACKs a consumed flush by moving the
        // state to 2. In the T8140 absent-handoff stand-in nothing consumes
        // it, so the state legitimately remains where begin_flush left it.
        let expected = match self.1 {
            HandoffMode::FirmwareDekker => 2,
            HandoffMode::AbsentStandinT8140
            | HandoffMode::NoPeerT8142
            | HandoffMode::NoPeerT8152 => 1,
        };
        if state != expected {
            pr_err!(
                "Handoff: expected flush state {}, got {}\n",
                expected,
                state
            );
        }
        flush.state.store(0, Ordering::Relaxed);
    }
}

impl Vm {
    pub(crate) fn status(&self) -> &Arc<crate::g17_status::VmStatus> { &self.status }

    /// This VM's driver-assigned id.
    ///
    /// The retained render graph is owned by a VM, not by a logical DRM queue:
    /// `render_bind.matches(vm)` is what decides whether a submission may
    /// restage the retained storage or has to hand it off. Anything that wants
    /// to keep a client on the storage it already built therefore has to key on
    /// the same identity, which is this id.
    pub(crate) fn id(&self) -> u64 { self.id }

    /// Application VA interval reserved by VM_CREATE for driver allocations.
    pub(crate) fn kernel_range(&self) -> Range<u64> { self.kernel_range.clone() }

    /// Create a new virtual memory address space
    fn new(
        dev: &driver::AsahiDevice,
        uat_inner: Arc<UatInner>,
        kernel_range: Range<u64>,
        cfg: UatConfig,
        ttb: Option<PhysicalAddr>,
        id: u64,
        t8140_internal: bool,
    ) -> Result<Vm> {
        let dummy_obj = gem::new_kernel_object(dev, UAT_PGSZ)?;
        let is_kernel = ttb.is_some();
        let iova_kern_range = iova_kern_range(cfg)?;

        let page_table = if let Some(ttb) = ttb {
            UatPageTable::new_with_ttb(ttb, iova_kern_range.clone(), cfg.ias, cfg.oas)?
        } else {
            UatPageTable::new(cfg.ias, cfg.oas)?
        };

        let lower_start = lower_vm_start(cfg, t8140_internal);
        // Native's SecureGart presents a process's first fixed resource at low
        // VA 0 inside that process's own translation regime, so a T8140 client
        // root has to be able to hold a leaf below `IOVA_USER_BASE`. Only the
        // driver's internal allocator is widened: the GPUVM range userspace
        // binds through, and the `vm_start` the driver publishes, both still
        // begin at `IOVA_USER_BASE`.
        // The saved T8152 compute submission also has a user allocation at VA
        // zero. Admit it only to explicitly internal VMs; the GPUVM userspace
        // range and ordinary T8152 client allocators still start at USER_BASE.
        let driver_start = if cfg == UatConfig::T8140
            || (cfg == UatConfig::T8152 && t8140_internal) { 0 } else { lower_start };
        let (va_range, gpuvm_range) = if is_kernel {
            (iova_kern_range, kernel_range.clone())
        } else {
            // Geometry-derived: 39-bit parts reproduce the AGX2-proven
            // user/usable ranges byte-for-byte; 42-bit parts extend the
            // user half to the full per-root width.
            (
                driver_start..uat_geometry(cfg)?.user_va_top(),
                lower_start..(uat_geometry(cfg)?.user_va_top() - 2 * UAT_PGSZ as u64),
            )
        };

        let mm = mm::Allocator::new(va_range.start, va_range.range(), ())?;

        let binding = Arc::pin_init(
            new_mutex!(
                VmBinding {
                    binding: None,
                    bind_token: None,
                    bind_context: None,
                    active_users: 0,
                    ttb: page_table.ttb(),
                },
                "VmBinding",
            ),
            GFP_KERNEL,
        )?;

        let mapping_validation_changes = if cfg == UatConfig::T8140 {
            Some(KBox::new([None; MAPPING_VALIDATION_HISTORY], GFP_KERNEL)?)
        } else {
            None
        };
        let binding_clone = binding.clone();
        let mapping_validation = if cfg == UatConfig::T8140 {
            Some(Arc::new(MappingValidationState::new(), GFP_KERNEL)?)
        } else {
            None
        };
        Ok(Vm {
            id,
            kernel_range: kernel_range.clone(),
            mapping_validation: mapping_validation.clone(),
            dummy_obj: dummy_obj.gem.clone(),
            inner: gpuvm::GpuVm::new(
                c_str!("Asahi::GpuVm"),
                // TODO: should we using DRM_GPUVM_RESV_PROTECTED as well?
                drm_gpuvm_flags_DRM_GPUVM_IMMEDIATE_MODE,
                dev,
                dummy_obj.gem.clone(),
                gpuvm_range,
                kernel_range,
                init!(VmInner {
                    dev: dev.into(),
                    va_range,
                    is_kernel,
                    page_table,
                    mapping_validation_epoch: Some(0),
                    mapping_validation,
                    mapping_validation_changes,
                    mm,
                    uat_inner,
                    binding: binding_clone,
                    id,
                }),
            )?,
            binding,
            driver_mappings: Arc::pin_init(new_mutex!(None, "VmDriverMappings"), GFP_KERNEL)?,
            status: Arc::new(crate::g17_status::VmStatus::new_for(id), GFP_KERNEL)?,
            job_lifetime: Arc::pin_init(new_mutex!(T8140VmJobLifetime {
                active: 0, quarantined: false, draining: false, closed: false,
                close_ranges: None, deferred_unmaps: KVec::new(),
                closed_objects: KVec::new(),
            }, "T8140VmJobLifetime"), GFP_KERNEL)?,
            mapping_gate: Arc::pin_init(
                pin_init!(T8140VmMappingGate {
                    state <- new_mutex!(T8140VmMappingGateState {
                        blocked: false,
                        commits: 0,
                    }, "T8140VmMappingGate"),
                    idle <- new_condvar!("T8140VmMappingGate::idle"),
                }),
                GFP_KERNEL,
            )?,
            t8140_context_alias_install: if cfg == UatConfig::T8140 {
                Some(Arc::pin_init(new_mutex!((), "T8140ContextAliasInstall"), GFP_KERNEL)?)
            } else {
                None
            },
            shared_bindings: Arc::pin_init(pin_init!(SharedBindings {
                list <- new_mutex!(KVec::new(), "SharedBindings"),
                count: AtomicU32::new(0),
            }), GFP_KERNEL)?,
            t8140_context_view0_unbound: Arc::new(AtomicBool::new(true), GFP_KERNEL)?,
            t8140_native_context_bindings: if cfg == UatConfig::T8140 {
                Some(Arc::pin_init(
                    new_mutex!(
                        T8140NativeContextBindings {
                            slots: [None, None, None, None, None, None, None],
                            generation: 0,
                        },
                        "T8140NativeContextBindings",
                    ),
                    GFP_KERNEL,
                )?)
            } else {
                None
            },
        })
    }

    /// Get the translation table base for this Vm
    fn ttb(&self) -> u64 {
        self.binding.lock().ttb
    }

    /// Track backing for every native restricted view covered by a user bind,
    /// without publishing it into context 0.
    ///
    /// A native resource can be a subrange of a larger userspace bind (Mesa's
    /// current context BO is one example), so record the derived object offset
    /// of the exact source interval rather than requiring equal bind bounds.
    /// Publication is delayed until this VM owns the retained render graph;
    /// Vulkan creates a probe VM and a logical-device VM with identical fixed
    /// resources, so doing it here would let the probe steal the global root
    /// from the queue that actually submits.
    pub(crate) fn track_t8140_native_context_binding(
        &self,
        gem: ARef<gem::Object>,
        address: u64,
        size: u64,
        object_offset: u64,
    ) -> Result {
        let Some(native_bindings) = self.t8140_native_context_bindings.as_ref() else {
            return Ok(());
        };
        let end = address.checked_add(size).ok_or(EOVERFLOW)?;
        let range = address..end;
        // Preflight every arithmetic/conversion before mutating the registry.
        // A failed oversized bind must not silently invalidate an otherwise
        // complete retained-view candidate without advancing its generation.
        let mut offsets = [None; T8140_NATIVE_CONTEXT_VIEW_COUNT];
        for (index, view) in T8140_NATIVE_CONTEXT_VIEWS.iter().copied().enumerate() {
            let source_end = view
                .source_address
                .checked_add(view.size as u64)
                .ok_or(EOVERFLOW)?;
            let source_range = view.source_address..source_end;
            if range.is_superset(source_range) {
                let delta = view.source_address.checked_sub(address).ok_or(EOVERFLOW)?;
                offsets[index] = Some(
                    object_offset
                        .checked_add(delta)
                        .ok_or(EOVERFLOW)?
                        .try_into()?,
                );
            }
        }
        let mut bindings = native_bindings.lock();
        let mut touched = false;
        for (index, view) in T8140_NATIVE_CONTEXT_VIEWS.iter().copied().enumerate() {
            let source_range =
                view.source_address..view.source_address + view.size as u64;
            if source_range.overlaps(range.clone()) {
                if index == 0 && bindings.slots[index].is_some() {
                    self.t8140_context_view0_unbound.store(true, Ordering::Release);
                }
                bindings.slots[index] = None;
                touched = true;
            }
            if let Some(object_offset) = offsets[index] {
                bindings.slots[index] = Some(T8140NativeContextBinding {
                    gem: gem.clone(),
                    object_offset,
                });
                if index == 0 {
                    self.t8140_context_view0_unbound.store(false, Ordering::Release);
                }
                touched = true;
            }
        }
        if touched {
            bindings.generation = bindings.generation.wrapping_add(1);
        }
        Ok(())
    }

    /// Invalidate every tracked restricted slot touched by a user unmap.
    pub(crate) fn untrack_t8140_native_context_range(&self, range: Range<u64>) {
        let Some(native_bindings) = self.t8140_native_context_bindings.as_ref() else {
            return;
        };
        let mut bindings = native_bindings.lock();
        let mut touched = false;
        for (index, view) in T8140_NATIVE_CONTEXT_VIEWS.iter().copied().enumerate() {
            let source_range =
                view.source_address..view.source_address + view.size as u64;
            if source_range.overlaps(range.clone()) {
                if index == 0 && bindings.slots[index].is_some() {
                    self.t8140_context_view0_unbound.store(true, Ordering::Release);
                }
                bindings.slots[index] = None;
                touched = true;
            }
        }
        if touched {
            bindings.generation = bindings.generation.wrapping_add(1);
        }
    }

    /// `g17p_tile_backstop` (D): record a committed user binding. A binding
    /// replaces whatever it overlaps; only a shareable object's (an external
    /// GPUVM object, i.e. not VM-private) multi-page binding is kept. An
    /// allocation failure leaves it unrecorded: the backstop then simply does
    /// not see it, as before this registry existed.
    pub(crate) fn track_shared_binding(
        &self, gem: &gem::Object, address: u64, size: u64, offset: u64, single_page: bool,
    ) {
        let Some(end) = address.checked_add(size) else { return };
        let shareable = !single_page && self.is_extobj(gem);
        // Common case: a VM-private bind into a VM without shared bindings.
        if !shareable && self.shared_bindings.count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut list = self.shared_bindings.list.lock();
        list.retain(|binding| binding.end <= address || end <= binding.start);
        if shareable {
            let _ = list.push(SharedBinding {
                start: address, end, object: gem as *const gem::Object as usize, offset,
            }, GFP_KERNEL);
        }
        self.shared_bindings.count.store(list.len() as u32, Ordering::Relaxed);
    }

    /// `g17p_tile_backstop` (D): a user unbind of `range`.
    pub(crate) fn untrack_shared_range(&self, range: Range<u64>) {
        if self.shared_bindings.count.load(Ordering::Relaxed) == 0 { return; }
        let mut list = self.shared_bindings.list.lock();
        list.retain(|binding| binding.end <= range.start || range.end <= binding.start);
        self.shared_bindings.count.store(list.len() as u32, Ordering::Relaxed);
    }

    /// `g17p_tile_backstop` (D): GEM close dropped every mapping of `gem`.
    pub(crate) fn untrack_shared_object(&self, gem: &gem::Object) {
        if self.shared_bindings.count.load(Ordering::Relaxed) == 0 { return; }
        let object = gem as *const gem::Object as usize;
        let mut list = self.shared_bindings.list.lock();
        list.retain(|binding| binding.object != object);
        self.shared_bindings.count.store(list.len() as u32, Ordering::Relaxed);
    }

    /// `g17p_tile_backstop` (D): add to `footprint` the object ranges that the
    /// VA range `[address, address + size)` reaches through shareable-object
    /// bindings. One atomic load when the VM has none.
    pub(crate) fn shared_footprint_spans(
        &self, address: u64, size: u64, footprint: &mut crate::g17_tile_backstop::Footprint,
    ) {
        if self.shared_bindings.count.load(Ordering::Relaxed) == 0 { return; }
        let Some(end) = address.checked_add(size) else { return };
        let list = self.shared_bindings.list.lock();
        for binding in list.iter() {
            let start = binding.start.max(address);
            let stop = binding.end.min(end);
            if start >= stop { continue; }
            let object_start = binding.offset.saturating_add(start - binding.start);
            footprint.push_object_span(binding.object, object_start,
                object_start.saturating_add(stop - start));
        }
    }

    /// GEM close can remove all GPUVA mappings without issuing range unbinds.
    pub(crate) fn untrack_t8140_native_context_object(&self, gem: &gem::Object) {
        let Some(native_bindings) = self.t8140_native_context_bindings.as_ref() else {
            return;
        };
        let mut bindings = native_bindings.lock();
        let mut touched = false;
        for (index, slot) in bindings.slots.iter_mut().enumerate() {
            if slot
                .as_ref()
                .is_some_and(|binding| core::ptr::eq(&*binding.gem, gem))
            {
                if index == 0 {
                    self.t8140_context_view0_unbound.store(true, Ordering::Release);
                }
                *slot = None;
                touched = true;
            }
        }
        if touched {
            bindings.generation = bindings.generation.wrapping_add(1);
        }
    }

    /// Called with a submit's mapping pin held. Confirm the flag under the
    /// registry lock so a concurrent rebind cannot create a stale refusal.
    pub(crate) fn t8140_job_context_view0_unbound(&self) -> bool {
        if !self.t8140_context_view0_unbound.load(Ordering::Acquire) { return false; }
        self.t8140_native_context_bindings.as_ref().is_some_and(|bindings|
            bindings.lock().slots[0].is_none())
    }

    /// A repeated single-page GPUVA is not the same physical layout as any
    /// native restricted resource. Reject any overlap before GPUVM commits it;
    /// otherwise alias installation could derive contiguous object pages from
    /// a source interval whose GPU mapping repeatedly names only one page.
    pub(crate) fn validate_t8140_native_context_binding(
        &self,
        address: u64,
        size: u64,
        single_page: bool,
    ) -> Result {
        if self.t8140_native_context_bindings.is_none() || !single_page {
            return Ok(());
        }
        let end = address.checked_add(size).ok_or(EOVERFLOW)?;
        let range = address..end;
        if T8140_NATIVE_CONTEXT_VIEWS.iter().any(|view| {
            let source_range =
                view.source_address..view.source_address + view.size as u64;
            source_range.overlaps(range.clone())
        }) {
            return Err(EINVAL);
        }
        Ok(())
    }

    /// Retain hardware-required fixed aliases for this VM's complete
    /// lifetime. Installing them before the VM reaches userspace also makes
    /// the GPUVM allocator reserve the addresses against user mappings.
    pub(crate) fn install_driver_mappings(
        &self,
        mappings: KVec<KernelMapping>,
        reserved_ranges: KVec<Range<u64>>,
    ) -> Result {
        let mut retained = self.driver_mappings.lock();
        if retained.is_some() {
            return Err(EBUSY);
        }
        *retained = Some(VmDriverMappings {
            mappings,
            reserved_ranges,
            simplefb: None,
            render_pools: [const { None }; 64],
            compute_pool: None,
        });
        Ok(())
    }

    /// Add VM-wide aliases discovered lazily by the render path. These are
    /// shared by every render pair and must not be owned by one pair's pool,
    /// because clearing that pool while another pair is live would unmap them.
    pub(crate) fn append_driver_mappings(&self, mappings: KVec<KernelMapping>) -> Result {
        if mappings.is_empty() {
            return Ok(());
        }
        let mut retained = self.driver_mappings.lock();
        let driver = Option::as_mut(&mut *retained).ok_or(EINVAL)?;
        let added = mappings.len();
        let length = driver.mappings.len().checked_add(added).ok_or(EOVERFLOW)?;
        driver.mappings.reserve(added, GFP_KERNEL)?;
        for (slot, mapping) in driver.mappings.spare_capacity_mut().iter_mut().zip(mappings) {
            slot.write(mapping);
        }
        // SAFETY: reserve provided room for every exact-size input element,
        // and the loop initialized each added slot once.
        unsafe { driver.mappings.set_len(length); }
        Ok(())
    }

    /// Keep validation, mapping and permanent retention in one VM-local
    /// transaction. A competing pair must never accept a tentative mapping
    /// whose owner can still be dropped by a failed builder. Failure drops
    /// the unpublished aliases before this guard permits the next builder.
    fn retain_job_context_aliases(
        &self,
        build: impl FnOnce() -> Result<KVec<KernelMapping>>,
    ) -> Result {
        let _install = self.t8140_context_alias_install.as_ref().ok_or(EINVAL)?.lock();
        self.append_driver_mappings(build()?)
    }

    /// Whether this VM already retains aliases of the exact render-pool
    /// generation. The caller allocates pool IDs monotonically, never from a
    /// recyclable GPU address, so a new firmware session cannot reuse stale
    /// aliases merely because its backing landed at the same address.
    pub(crate) fn render_pool_mappings_match(&self, render_slot: u8, pool_id: u64) -> bool {
        pool_id != 0
            && self.driver_mappings.lock().as_ref().is_some_and(|driver| {
                driver
                    .render_pools
                    .get(render_slot as usize)
                    .and_then(Option::as_ref)
                    .is_some_and(|pool| pool.pool_id == pool_id)
            })
    }

    /// Retain the first-use render aliases and exclude them from user unmaps.
    /// Installation never replaces a live generation; callers must retire its
    /// GPU work and explicitly clear it before installing a different pool.
    pub(crate) fn install_render_pool_mappings(
        &self,
        render_slot: u8,
        pool_id: u64,
        tvb_blocks: usize,
        mappings: KVec<KernelMapping>,
    ) -> Result {
        if pool_id == 0 || render_slot as usize >= 64 {
            return Err(EINVAL);
        }
        let mut retained = self.driver_mappings.lock();
        let driver = Option::as_mut(&mut *retained).ok_or(EINVAL)?;
        let pool = driver.render_pools.get_mut(render_slot as usize).ok_or(EINVAL)?;
        if pool.is_some() {
            return Err(EBUSY);
        }
        *pool = Some(VmRenderPoolMappings { pool_id, tvb_blocks, mappings });
        Ok(())
    }

    pub(crate) fn render_pool_tvb_blocks(&self, render_slot: u8, pool_id: u64) -> Option<usize> {
        if pool_id == 0 { return None; }
        self.driver_mappings.lock().as_ref()?.render_pools.get(render_slot as usize)?.as_ref()
            .filter(|pool| pool.pool_id == pool_id).map(|pool| pool.tvb_blocks)
    }

    /// Append newly backed blocks without replacing any existing VM alias.
    /// Both allocation and the expected-capacity check precede mutation. The
    /// caller only publishes the enlarged PM pool after this succeeds.
    pub(crate) fn append_render_pool_mappings(
        &self, render_slot: u8, pool_id: u64, previous_blocks: usize, tvb_blocks: usize,
        mappings: KVec<KernelMapping>,
    ) -> Result {
        if pool_id == 0 || render_slot as usize >= 64 || tvb_blocks <= previous_blocks
            || mappings.len() != tvb_blocks - previous_blocks { return Err(EINVAL); }
        let mut retained = self.driver_mappings.lock();
        let pool = Option::as_mut(&mut *retained)
            .and_then(|driver| driver.render_pools.get_mut(render_slot as usize))
            .and_then(Option::as_mut)
            .filter(|pool| pool.pool_id == pool_id).ok_or(EINVAL)?;
        if pool.tvb_blocks != previous_blocks { return Err(EBUSY); }
        let added = mappings.len();
        let length = pool.mappings.len().checked_add(added).ok_or(EOVERFLOW)?;
        pool.mappings.reserve(added, GFP_KERNEL)?;
        for (slot, mapping) in pool.mappings.spare_capacity_mut().iter_mut().zip(mappings) {
            slot.write(mapping);
        }
        // SAFETY: reserve provided room for every element of the exact-size
        // mapping vector, and the loop initialized those added slots once.
        unsafe { pool.mappings.set_len(length); }
        pool.tvb_blocks = tvb_blocks;
        Ok(())
    }

    /// Release an obsolete pool after its GPU references have retired. Drop
    /// outside driver_mappings: KernelMapping teardown takes the GPUVM lock.
    pub(crate) fn clear_render_pool_mappings(&self, render_slot: u8) {
        let previous = {
            let mut retained = self.driver_mappings.lock();
            Option::as_mut(&mut *retained)
                .and_then(|driver| driver.render_pools.get_mut(render_slot as usize))
                .and_then(Option::take)
        };
        drop(previous);
    }

    /// Retire only the named graph's aliases. A delayed teardown or failed
    /// install must not remove a successor that already owns the same pair.
    /// Keep the identity check and removal in one mapping-cache transaction;
    /// unmapping still happens after releasing that lock.
    pub(crate) fn clear_render_pool_mappings_if_owner(&self, render_slot: u8, pool_id: u64) {
        if pool_id == 0 { return; }
        let previous = {
            let mut retained = self.driver_mappings.lock();
            Option::as_mut(&mut *retained)
                .and_then(|driver| driver.render_pools.get_mut(render_slot as usize))
                .filter(|pool| pool.as_ref().is_some_and(|owner| owner.pool_id == pool_id))
                .and_then(Option::take)
        };
        drop(previous);
    }

    pub(crate) fn compute_pool_mappings_match(&self, pool_id: u64) -> bool {
        pool_id != 0
            && self.driver_mappings.lock().as_ref().is_some_and(|driver| {
                driver
                    .compute_pool
                    .as_ref()
                    .is_some_and(|pool| pool.pool_id == pool_id)
            })
    }

    /// Retain the first-use compute aliases and exclude them from user unmaps.
    /// Installation never replaces a live generation; callers must retire its
    /// GPU work and explicitly clear it before installing a different pool.
    pub(crate) fn install_compute_pool_mappings(
        &self,
        pool_id: u64,
        mappings: KVec<KernelMapping>,
    ) -> Result {
        if pool_id == 0 {
            return Err(EINVAL);
        }
        let mut retained = self.driver_mappings.lock();
        let driver = Option::as_mut(&mut *retained).ok_or(EINVAL)?;
        if driver.compute_pool.is_some() {
            return Err(EBUSY);
        }
        driver.compute_pool = Some(VmRenderPoolMappings { pool_id, tvb_blocks: 0, mappings });
        Ok(())
    }

    /// Release an obsolete pool after its GPU references have retired. Drop
    /// outside driver_mappings: KernelMapping teardown takes the GPUVM lock.
    pub(crate) fn clear_compute_pool_mappings(&self) {
        let previous = {
            let mut retained = self.driver_mappings.lock();
            Option::as_mut(&mut *retained).and_then(|driver| driver.compute_pool.take())
        };
        drop(previous);
    }

    pub(crate) fn driver_range_overlaps(&self, range: Range<u64>) -> bool {
        self.driver_mappings.lock().as_ref().is_some_and(|driver| {
            any_range_overlaps(
                range.clone(),
                driver.iter_mappings().map(KernelMapping::iova_range),
            ) || any_range_overlaps(range, driver.reserved_ranges.iter().cloned())
        })
    }

    /// The first job guard of a submission (no other guard held by the
    /// caller). Recovery isolation `ADMISSION_WAIT`: a VM whose status is
    /// clean but whose admission is blocked by retained (quarantined) work
    /// waits a bounded time for the last guard's drain instead of failing
    /// the submission with EIO (which would poison it as a lost device).
    pub(crate) fn retain_first_t8140_job(&self) -> Result<T8140VmJobGuard> {
        // Hot path: exactly Y. Only a refusal (gate blocked) of a VM whose
        // status is clean takes the bounded wait below.
        match self.retain_t8140_job() {
            Err(error) if error == EIO
                && self.status.get() == 0
                && crate::g17_recovery_isolation::enabled(crate::g17_recovery_isolation::ADMISSION_WAIT) => {}
            result => return result,
        }
        let mut remaining = kernel::time::msecs_to_jiffies(
            crate::g17_recovery_isolation::ADMISSION_WAIT_MS);
        crate::g17_recovery_isolation::count(&crate::g17_recovery_isolation::ADMISSION_WAITS);
        pr_info!("G17P VM job admission waiting for quarantine release: vm={}\n", self.id);
        let mut gate = self.mapping_gate.state.lock();
        loop {
            let blocked = gate.blocked;
            if !blocked || self.status.get() != 0 || remaining == 0 { break; }
            match self.mapping_gate.idle.wait_interruptible_timeout(&mut gate, remaining) {
                kernel::sync::CondVarTimeoutResult::Signal { .. } => return Err(ERESTARTSYS),
                kernel::sync::CondVarTimeoutResult::Timeout => {
                    remaining = 0;
                    crate::g17_recovery_isolation::count(&crate::g17_recovery_isolation::ADMISSION_TIMEOUTS);
                }
                kernel::sync::CondVarTimeoutResult::Woken { jiffies } => remaining = jiffies,
            }
        }
        core::mem::drop(gate);
        self.retain_t8140_job()
    }

    pub(crate) fn retain_t8140_job(&self) -> Result<T8140VmJobGuard> {
        let mut gate = self.mapping_gate.state.lock();
        let mut waited = 0u64;
        loop {
            let mut state = self.job_lifetime.lock();
            if state.closed { return Err(ENOENT); }
            if gate.blocked { return Err(EIO); }
            if state.draining {
                core::mem::drop(state);
                if waited == 0 { waited = crate::g17_mem_path::stamp().max(1); }
                self.mapping_gate.idle.wait(&mut gate);
                continue;
            }
            state.active = state.active.checked_add(1).ok_or(EOVERFLOW)?;
            if waited > 1 { crate::g17_mem_path::RETAIN_WAIT.since(waited); }
            return Ok(T8140VmJobGuard { vm: self.clone() });
        }
    }

    /// Remove userspace mappings while preserving VM-lifetime driver aliases.
    pub(crate) fn unmap_user_ranges(
        &self,
        user_range: Range<u64>,
        kernel_range: Range<u64>,
    ) -> Result {
        // VM destruction permanently closes this gate. Wait for an already
        // prepared leaf transaction before deciding whether the final unmap is
        // immediate or belongs to the last job guard.
        let mut gate = self.mapping_gate.state.lock();
        gate.blocked = true;
        while gate.commits != 0 {
            self.mapping_gate.idle.wait(&mut gate);
        }
        let mut state = self.job_lifetime.lock();
        state.closed = true;
        if state.active != 0 {
            state.close_ranges = Some((user_range, kernel_range));
            return Ok(());
        }
        core::mem::drop(state);
        core::mem::drop(gate);
        self.unmap_user_ranges_now(user_range, kernel_range)
    }

    fn unmap_user_ranges_now(
        &self,
        user_range: Range<u64>,
        kernel_range: Range<u64>,
    ) -> Result {
        let retained = self.driver_mappings.lock();
        let driver = retained.as_ref();
        let mut cursor = user_range.start;

        while cursor < user_range.end {
            if kernel_range.contains(&cursor) {
                cursor = kernel_range.end.min(user_range.end);
                continue;
            }

            let mut next = user_range.end;
            let mut covering_end = cursor;
            if let Some(driver) = driver {
                for mapping in driver.iter_mappings() {
                    let range = mapping.iova_range();
                    if range.contains(&cursor) {
                        covering_end = covering_end.max(range.end);
                    } else if cursor < range.start {
                        next = next.min(range.start);
                    }
                }
                for range in driver.reserved_ranges.iter() {
                    if range.contains(&cursor) {
                        covering_end = covering_end.max(range.end);
                    } else if cursor < range.start {
                        next = next.min(range.start);
                    }
                }
            }
            if covering_end != cursor {
                cursor = covering_end.min(user_range.end);
                continue;
            }
            if cursor < kernel_range.start {
                next = next.min(kernel_range.start);
            }
            if next <= cursor {
                return Err(EIO);
            }
            self.unmap_range(cursor, next - cursor)?;
            cursor = next;
        }
        Ok(())
    }

    /// Install one VM-lifetime alias of the boot framebuffer aperture.
    ///
    /// The caller has already validated that the physical interval is the
    /// exact DT-owned simplefb allocation and that the IOVA is inside the
    /// usable user range. Keeping this separate from GEM prevents the no-map
    /// carveout from ever being treated as ordinary `struct page` memory.
    pub(crate) fn install_simplefb_mapping(
        &self,
        iova: u64,
        phys: usize,
        size: usize,
    ) -> Result {
        {
            let retained = self.driver_mappings.lock();
            let driver = retained.as_ref().ok_or(EINVAL)?;
            if driver.simplefb.is_some() {
                return Err(EBUSY);
            }
        }

        let mapping = self.map_io(iova, phys, size, PROT_GPU_SHARED_RW)?;
        let mut retained = self.driver_mappings.lock();
        let driver = Option::as_mut(&mut *retained).ok_or(EINVAL)?;
        if driver.simplefb.is_some() {
            core::mem::drop(retained);
            core::mem::drop(mapping);
            return Err(EBUSY);
        }
        driver.simplefb = Some(mapping);
        Ok(())
    }

    /// Check that a byte range is fully mapped with the requested GPU access.
    fn covers_range_locked(
        inner: &mut VmInner,
        address: u64,
        size: u64,
        need_read: bool,
        need_write: bool,
    ) -> bool {
        if size == 0 {
            return false;
        }
        let Some(end) = address.checked_add(size) else {
            return false;
        };
        let page_mask = UAT_PGMSK as u64;
        let start = address & !page_mask;
        let aligned_end = if end & page_mask == 0 {
            end
        } else {
            match (end | page_mask).checked_add(1) {
                Some(end) => end,
                None => return false,
            }
        };
        if start < inner.va_range.start || aligned_end > inner.va_range.end {
            return false;
        }
        inner
            .page_table
            .covers_range(start..aligned_end, need_read, need_write)
            .unwrap_or(false)
    }

    pub(crate) fn covers_range(
        &self,
        address: u64,
        size: u64,
        need_read: bool,
        need_write: bool,
    ) -> bool {
        let mut inner = match self.inner.exec_lock(None, false) {
            Ok(inner) => inner,
            Err(_) => return false,
        };
        Self::covers_range_locked(&mut inner, address, size, need_read, need_write)
    }

    /// v91 VALIDATE_PROOF: evaluate several `(address, size, read, write)`
    /// coverage predicates against one consistent page-table snapshot, i.e.
    /// under a single GPUVM exec lock instead of one per range, and return the
    /// mapping-validation epoch read under that same lock. Every predicate is
    /// the exact `covers_range_locked` test `covers_range` applies. Writers
    /// hold the exec lock and publish the epoch before releasing it, so the
    /// returned epoch names exactly the snapshot that was validated (`None`
    /// when this VM keeps no epoch or it overflowed).
    /// `ranges` holds `(address, size, access)` with access bit0 = read and
    /// bit1 = write.
    pub(crate) fn covers_ranges_batch(
        &self,
        ranges: &[(u64, u64, u8)],
    ) -> (bool, Option<u64>) {
        let mut inner = match self.inner.exec_lock(None, false) {
            Ok(inner) => inner,
            Err(_) => return (false, None),
        };
        let epoch = inner.mapping_validation_epoch;
        let covered = ranges.iter().all(|&(address, size, access)| {
            Self::covers_range_locked(&mut inner, address, size, access & 1 != 0, access & 2 != 0)
        });
        (covered, epoch)
    }

    /// Evaluate a sparse block/guard set against one consistent page-table
    /// snapshot. The old loop acquired the GPUVM exec lock twice per block.
    pub(crate) fn covers_sparse_blocks_with_guards(
        &self,
        addresses: &[u64],
        block_size: u64,
        stride: u64,
    ) -> bool {
        if stride <= block_size {
            return false;
        }
        let mut inner = match self.inner.exec_lock(None, false) {
            Ok(inner) => inner,
            Err(_) => return false,
        };
        addresses.iter().all(|&address| {
            let Some(guard) = address.checked_add(block_size) else {
                return false;
            };
            Self::covers_range_locked(&mut inner, address, block_size, true, true)
                && !Self::covers_range_locked(
                    &mut inner, guard, stride - block_size, false, false)
        })
    }

    /// Cached-proof lookup never takes the GPUVM exec lock. An in-progress or
    /// uncacheable mutation returns None, forcing the existing locked walk.
    /// Readers still double-collect both VMs' versions around proof reuse;
    /// normal job/mapping ownership governs changes after validation.
    pub(crate) fn mapping_validation_epoch(&self) -> Option<u64> {
        self.mapping_validation.as_ref()?.snapshot()
    }

    /// Report whether mutations since `previous` touched a dependency.
    /// History loss or an unavailable lock refuses the proof.
    /// Also reports the number of examined changes and, when `timed`, the
    /// CNTPCT at which the exec lock was held (v94 diagnostics).
    pub(crate) fn mapping_changes_since_counted(
        &self,
        previous: u64,
        mut intersects: impl FnMut(Range<u64>) -> bool,
        timed: bool,
    ) -> Option<MappingChangeWalk> {
        let inner = self.inner.exec_lock(None, false).ok()?;
        let locked_at = if timed { crate::g17_timing_buffer::arch_counter() } else { 0 };
        let changes = inner.mapping_validation_changes.as_ref()?;
        let current = inner.mapping_validation_epoch?;
        let count = current.checked_sub(previous)?;
        if count > MAPPING_VALIDATION_HISTORY as u64 {
            return None;
        }
        let mut walk = MappingChangeWalk { epoch: current, touched: false, walked: 0, locked_at };
        if count == 0 {
            return Some(walk);
        }
        for epoch in previous + 1..=current {
            let change = changes[epoch as usize % MAPPING_VALIDATION_HISTORY]?;
            if change.epoch != epoch {
                return None;
            }
            walk.walked += 1;
            if intersects(change.start..change.end) {
                walk.touched = true;
                return Some(walk);
            }
        }
        Some(walk)
    }

    /// v94 REACH_LOCKLESS: the same walk without the GPUVM exec lock, from
    /// the lock-free history. `current` is the epoch the caller already read
    /// with `mapping_validation_epoch`. None means "use the locked walk".
    pub(crate) fn mapping_changes_since_lockless(
        &self,
        previous: u64,
        current: u64,
        intersects: impl FnMut(Range<u64>) -> bool,
    ) -> Option<MappingChangeWalk> {
        if !crate::g17_mem_path::enabled(crate::g17_mem_path::REACH_LOCKLESS) { return None; }
        self.mapping_validation.as_ref()?.changes_since(previous, current, intersects)
    }

    /// Resolve an IOVA through this VM's current page table. Callers use this
    /// for mapping-identity checks only; it does not read the mapped page.
    pub(crate) fn translate_iova(&self, address: u64) -> Result<PhysicalAddr> {
        let mut inner = self.inner.exec_lock(None, false)?;
        inner.page_table.translate_iova(address)
    }

    /// Resolve one offset of native's low view 0 through this VM's own root
    /// and through the canonical GART address, so a caller can prove -- before
    /// arming hardware -- that the client regime reaches the same physical
    /// resource the GPU is about to write there.
    pub(crate) fn t8140_client_null_view_probe(
        &self,
        offset: u64,
    ) -> Result<(PhysicalAddr, PhysicalAddr)> {
        if offset >= T8140_CLIENT_NULL_VIEW_SIZE as u64 {
            return Err(EINVAL);
        }
        let canonical_address = T8140_CLIENT_NULL_VIEW
            .source_address
            .checked_add(offset)
            .ok_or(EOVERFLOW)?;
        // Compare both aliases from one page-table snapshot, without taking
        // the GPUVM execution lock once per translation.
        let mut inner = self.inner.exec_lock(None, false)?;
        let canonical = inner.page_table.translate_iova(canonical_address)?;
        let client = inner.page_table.translate_iova(
            T8140_CLIENT_NULL_VIEW_START
                .checked_add(offset)
                .ok_or(EOVERFLOW)?,
        )?;
        Ok((canonical, client))
    }

    /// Collect this VM's GPU-visible mappings inside `range`.
    ///
    /// Page-table memory only: this walks ordinary DRAM and never touches GPU
    /// MMIO, so it is safe to call after a faulted submission has let the
    /// cores power-gate.  Returns the number of runs written and whether
    /// `out` was too small to hold them all.
    pub(crate) fn mapped_ranges(
        &self,
        range: Range<u64>,
        out: &mut [pgtable::MappedRange],
    ) -> Result<(usize, bool)> {
        let mut inner = self.inner.exec_lock(None, false)?;
        inner.page_table.collect_mapped_ranges(range, out)
    }

    /// Read GPU-visible bytes out of this VM.
    ///
    /// Fail-closed: the whole span must first pass `covers_range` with GPU
    /// read permission, and every page is then translated and borrowed
    /// individually.  This only ever touches ordinary DRAM backing a GEM
    /// object that is mapped into this VM, never GPU MMIO, so it is safe with
    /// the cores gated.
    ///
    /// It exists so the driver can enumerate the pointers a submission
    /// actually dereferences.  A GMMU page fault names no address on G17P (the
    /// MMIO fault bank is a reset-default phantom), and the UAPI carries only
    /// the control-stream range -- every other pointer the GPU follows lives
    /// *inside* the command stream and the Ioto program, where only a reader
    /// like this can find it.
    pub(crate) fn read_bytes(&self, iova: u64, out: &mut [u8]) -> Result {
        if out.is_empty() {
            return Err(EINVAL);
        }
        if !self.covers_range(iova, out.len() as u64, true, false) {
            return Err(EFAULT);
        }
        let total = out.len();
        let mut done = 0usize;
        while done < total {
            let addr = iova.checked_add(done as u64).ok_or(EOVERFLOW)?;
            let phys = self.translate_iova(addr)?;
            let page_mask = (kernel::page::PAGE_SIZE as PhysicalAddr) - 1;
            let page_phys = phys & !page_mask;
            let offset = (phys - page_phys) as usize;
            let chunk = cmp::min(kernel::page::PAGE_SIZE - offset, total - done);
            // SAFETY: `translate_iova` resolved this address through a leaf
            // PTE of this VM, so it names a page of a GEM object that is
            // mapped here and therefore pinned for the duration of the
            // mapping. `borrow_phys` additionally rejects anything without a
            // struct page.
            let page = unsafe { Page::borrow_phys(&page_phys) }.ok_or(EFAULT)?;
            let dst = &mut out[done..done + chunk];
            page.with_pointer_into_page(offset, chunk, |ptr| {
                // SAFETY: `with_pointer_into_page` bounds-checked
                // `offset..offset + chunk` inside the page.
                let src = unsafe { core::slice::from_raw_parts(ptr as *const u8, chunk) };
                dst.copy_from_slice(src);
                Ok(())
            })?;
            done += chunk;
        }
        Ok(())
    }

    /// Resolve and read one audit region under a single GPUVM execution lock.
    /// Preserve the checks and error order of `translate_iova` followed by
    /// `read_bytes`, including the explicit readable-range proof. The guard
    /// ends here: callers may then compare against a GEM VMap without nesting
    /// a GEM reservation lock inside this execution-lock transaction.
    pub(crate) fn read_bytes_with_physical(
        &self,
        iova: u64,
        out: &mut [u8],
    ) -> Result<PhysicalAddr> {
        let mut inner = self.inner.exec_lock(None, false)?;
        let first_physical = inner.page_table.translate_iova(iova)?;
        if out.is_empty() {
            return Err(EINVAL);
        }
        if !Self::covers_range_locked(&mut inner, iova, out.len() as u64, true, false) {
            return Err(EFAULT);
        }
        let total = out.len();
        let mut done = 0usize;
        while done < total {
            let addr = iova.checked_add(done as u64).ok_or(EOVERFLOW)?;
            let phys = inner.page_table.translate_iova(addr)?;
            let page_mask = (kernel::page::PAGE_SIZE as PhysicalAddr) - 1;
            let page_phys = phys & !page_mask;
            let offset = (phys - page_phys) as usize;
            let chunk = cmp::min(kernel::page::PAGE_SIZE - offset, total - done);
            // SAFETY: The execution guard keeps the resolved GEM mapping
            // stable through this read. borrow_phys also requires struct page.
            let page = unsafe { Page::borrow_phys(&page_phys) }.ok_or(EFAULT)?;
            let dst = &mut out[done..done + chunk];
            page.with_pointer_into_page(offset, chunk, |ptr| {
                // SAFETY: with_pointer_into_page bounds-checks this page span.
                let src = unsafe { core::slice::from_raw_parts(ptr as *const u8, chunk) };
                dst.copy_from_slice(src);
                Ok(())
            })?;
            done += chunk;
        }
        Ok(first_physical)
    }

    /// Physical base of this VM's page-table root.
    ///
    /// This is what a hardware context's TTB0 has to name for the GPU to see
    /// any of this VM's mappings, so it is the other half of a reachability
    /// check that `covers_range` alone cannot make.
    pub(crate) fn page_table_root(&self) -> u64 {
        self.binding.lock().ttb
    }

    /// Snapshot the first `out.len()` hardware context-table (TTBAT) entries
    /// as `(ttb0, ttb1)` pairs.
    ///
    /// The TTBAT is an ordinary host DRAM region, so this reads no GPU MMIO
    /// and is safe with the cores gated.  A G17P compute kick declares
    /// hardware context 2 or 3, so comparing those entries against
    /// [`Vm::page_table_root`] is the direct test of whether the GPU is
    /// walking this VM's tables at all -- something `covers_range` cannot see.
    pub(crate) fn context_roots(&self, out: &mut [(u64, u64)]) -> Result {
        let inner = self.inner.exec_lock(None, false)?;
        let shared = inner.uat_inner.lock();
        let ttbs = shared.ttbs();
        for (slot, entry) in out.iter_mut().enumerate() {
            if slot >= UAT_NUM_CTX {
                break;
            }
            *entry = (
                ttbs[slot].ttb0.load(Ordering::Acquire),
                ttbs[slot].ttb1.load(Ordering::Acquire),
            );
        }
        Ok(())
    }

    /// Map a GEM object (using its `SGTable`) into this Vm at a free address in a given range.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn map_in_range(
        &self,
        gem: &gem::Object,
        object_range: Range<usize>,
        alignment: u64,
        range: Range<u64>,
        prot: Prot,
        guard: bool,
    ) -> Result<KernelMapping> {
        self.map_in_range_with_guard_size(
            gem, object_range, alignment, range, prot,
            if guard { UAT_PGSZ } else { 0 },
        )
    }

    /// Reserve a checked number of trailing unmapped pages for M5 resources.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn map_in_range_with_guards(
        &self,
        gem: &gem::Object,
        object_range: Range<usize>,
        alignment: u64,
        range: Range<u64>,
        prot: Prot,
        guard_pages: usize,
    ) -> Result<KernelMapping> {
        let guard_size = guard_pages.checked_mul(UAT_PGSZ).ok_or(EOVERFLOW)?;
        self.map_in_range_with_guard_size(
            gem, object_range, alignment, range, prot, guard_size,
        )
    }

    /// Reserve an explicitly sized unmapped guard with the mapping. G17 PB
    /// blocks use 32-KiB guards even though the UAT page size is 16 KiB.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn map_in_range_with_guard_size(
        &self,
        gem: &gem::Object,
        object_range: Range<usize>,
        alignment: u64,
        range: Range<u64>,
        prot: Prot,
        guard_size: usize,
    ) -> Result<KernelMapping> {
        if guard_size % UAT_PGSZ != 0 { return Err(EINVAL); }
        let size = object_range.end.checked_sub(object_range.start).ok_or(EINVAL)?;
        if size == 0 || object_range.end > gem.size() { return Err(EINVAL); }
        let reserved_size = size.checked_add(guard_size).ok_or(EOVERFLOW)?;
        let sgt = gem.owned_sg_table()?;
        let mut inner = self.inner.exec_lock(Some(gem), false)?;
        let vm_bo = self.inner.obtain_bo(gem)?;

        let mut vm_bo_guard = vm_bo.inner().inner.lock();
        if vm_bo_guard.sgt.is_none() {
            vm_bo_guard.sgt.replace(sgt);
        }
        core::mem::drop(vm_bo_guard);
        let mut vm_bo = Some(ImmediateGpuVmBoRef::new(vm_bo, self.inner.clone()));

        let uat_inner = inner.uat_inner.clone();
        // Reserve the VA before attaching objects whose destructors take the
        // GEM reservation lock.  The allocator drops its payload internally
        // on an insertion error, while exec_lock still owns that lock.
        let node_result = inner.mm.insert_node_in_range(
            KernelMappingInner {
                owner: self.inner.clone(),
                uat_inner,
                prot,
                bo: None,
                _gem: None,
                offset: object_range.start,
                mapped_size: size,
            },
            reserved_size as u64,
            alignment,
            0,
            range.start,
            range.end,
            mm::InsertMode::Best,
        );
        let mut node = match node_result {
            Ok(node) => node,
            Err(err) => {
                core::mem::drop(inner);
                return Err(err);
            }
        };
        {
            let node_inner = node.as_mut().inner_mut();
            node_inner.bo = vm_bo.take();
            node_inner._gem = Some(gem.into());
        }

        let ret = inner.map_node(&node, prot);
        // Drop the exec_lock first, so that if map_node failed the
        // KernelMappingInner destructur does not deadlock.
        core::mem::drop(inner);
        ret?;
        Ok(KernelMapping(node))
    }

    /// Map a GEM object into this Vm at a specific address.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn map_at(
        &self,
        addr: u64,
        size: usize,
        gem: ARef<gem::Object>,
        prot: Prot,
        guard: bool,
    ) -> Result<KernelMapping> {
        self.map_at_with_guards(addr, size, gem, prot, usize::from(guard))
    }

    /// Map a GEM object at a specific address and reserve `guard_pages`
    /// trailing unmapped pages in the same allocator node, so an alias of a
    /// guarded allocation keeps its hardware guard gap in this Vm too.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn map_at_with_guards(
        &self,
        addr: u64,
        size: usize,
        gem: ARef<gem::Object>,
        prot: Prot,
        guard_pages: usize,
    ) -> Result<KernelMapping> {
        let reserved = guard_pages.checked_mul(UAT_PGSZ)
            .and_then(|guard| size.checked_add(guard)).ok_or(EOVERFLOW)?;
        let sgt = gem.owned_sg_table()?;
        let mut inner = self.inner.exec_lock(Some(&gem), false)?;

        let vm_bo = self.inner.obtain_bo(&gem)?;

        let mut vm_bo_guard = vm_bo.inner().inner.lock();
        if vm_bo_guard.sgt.is_none() {
            vm_bo_guard.sgt.replace(sgt);
        }
        core::mem::drop(vm_bo_guard);
        let mut vm_bo = Some(ImmediateGpuVmBoRef::new(vm_bo, self.inner.clone()));

        let uat_inner = inner.uat_inner.clone();
        // See map_in_range(): keep dma_resv-owning references out of the
        // allocator payload until the fixed-address reservation succeeds.
        let node_result = inner.mm.reserve_node(
            KernelMappingInner {
                owner: self.inner.clone(),
                uat_inner,
                prot,
                bo: None,
                _gem: None,
                offset: 0,
                mapped_size: size,
            },
            addr,
            reserved as u64, // Mapping plus its trailing guard pages
            0,
        );
        let mut node = match node_result {
            Ok(node) => node,
            Err(err) => {
                core::mem::drop(inner);
                return Err(err);
            }
        };
        {
            let node_inner = node.as_mut().inner_mut();
            node_inner.bo = vm_bo.take();
            node_inner._gem = Some(gem.clone());
        }

        let ret = inner.map_node(&node, prot);
        // Drop the exec_lock first, so that if map_node failed the
        // KernelMappingInner destructur does not deadlock.
        core::mem::drop(inner);
        ret?;
        Ok(KernelMapping(node))
    }

    /// Map a range of a GEM object into this Vm using GPUVM.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bind_object(
        &self,
        gem: &gem::Object,
        addr: u64,
        size: u64,
        offset: u64,
        prot: Prot,
        single_page: bool,
    ) -> Result {
        let prepared = self.prepare_bind_object(
            gem, addr, size, offset, prot, single_page,
        )?;
        let mut batch = PreparedUserBindBatch::new(1)?;
        batch.push_map(prepared)?;
        self.commit_prepared_user_bind_batch(&mut batch)
    }

    /// Reserve the BO state, split nodes and scatterlist needed by one
    /// userspace map without taking dma-resv or walking its address range.
    pub(crate) fn prepare_bind_object(
        &self,
        gem: &gem::Object,
        addr: u64,
        size: u64,
        offset: u64,
        prot: Prot,
        single_page: bool,
    ) -> Result<PreparedUserMap> {
        // Mapping needs a complete context
        let mut ctx = StepContext {
            new_va: Some(gpuvm::GpuVa::<VmInner>::new(pin_init::default())?),
            prev_va: Some(gpuvm::GpuVa::<VmInner>::new(pin_init::default())?),
            next_va: Some(gpuvm::GpuVa::<VmInner>::new(pin_init::default())?),
            prot,
            ..Default::default()
        };

        let vm_bo = self.inner.obtain_bo(gem)?;
        {
            let mut vm_bo_guard = vm_bo.inner().inner.lock();
            if vm_bo_guard.sgt.is_none() {
                let sgt = gem.owned_sg_table()?;

                if vm_bo_guard.sg_vec.is_none() {
                    let mut sg_vec = KVVec::new();
                    let mut offset = 0;
                    for range in sgt.iter() {
                        let addr = range.dma_address() as usize;
                        let len = range.dma_len() as usize;
                        sg_vec.push((offset, addr..(addr + len)), GFP_KERNEL)?;
                        offset += len;
                    }
                    vm_bo_guard.sg_vec.replace(sg_vec);
                }
                vm_bo_guard.sgt.replace(sgt);
            }
            core::mem::drop(vm_bo_guard);
        }

        ctx.vm_bo = Some(vm_bo);

        if (addr | size | offset) & (UAT_PGMSK as u64) != 0 {
            return Err(EINVAL);
        }

        Ok(PreparedUserMap {
            gem: gem.into(), ctx, addr, size, offset, single_page,
        })
    }

    /// Wait for healthy retirement before preparing a userspace map. This
    /// runs before taking dma-resv locks, which the retirement drain needs.
    /// A failed or quarantined VM still cannot replace firmware-owned maps.
    pub(crate) fn wait_for_user_map_admission(&self) -> Result {
        let mut gate = self.mapping_gate.state.lock();
        let mut waited = false;
        let mut wait_started = 0u64;
        // Recovery isolation: remaining bounded wait for a healthy VM whose
        // mappings are blocked only by retained (quarantined) work.
        let mut transient_budget: Option<kernel::time::Jiffies> = None;
        loop {
            let state = self.job_lifetime.lock();
            let status = self.status.get();
            let transient = status == 0 && !state.closed
                && (gate.blocked || (state.active != 0 && state.quarantined));
            if transient && crate::g17_recovery_isolation::enabled(
                crate::g17_recovery_isolation::ADMISSION_WAIT)
            {
                let remaining = *transient_budget.get_or_insert_with(|| {
                    crate::g17_recovery_isolation::count(
                        &crate::g17_recovery_isolation::ADMISSION_WAITS);
                    pr_info!(
                        "G17P VM map admission waiting for quarantine release: vm={} blocked={} active={} quarantined={}\n",
                        self.id, gate.blocked, state.active, state.quarantined
                    );
                    kernel::time::msecs_to_jiffies(crate::g17_recovery_isolation::ADMISSION_WAIT_MS)
                });
                if remaining != 0 {
                    core::mem::drop(state);
                    // Woken by the last job guard's drain (gate -> lifetime,
                    // then notify_all), exactly as the draining wait below.
                    match self.mapping_gate.idle.wait_interruptible_timeout(&mut gate, remaining) {
                        kernel::sync::CondVarTimeoutResult::Signal { .. } => return Err(ERESTARTSYS),
                        kernel::sync::CondVarTimeoutResult::Timeout => {
                            transient_budget = Some(0);
                            crate::g17_recovery_isolation::count(
                                &crate::g17_recovery_isolation::ADMISSION_TIMEOUTS);
                        }
                        kernel::sync::CondVarTimeoutResult::Woken { jiffies } => {
                            transient_budget = Some(jiffies);
                        }
                    }
                    continue;
                }
            }
            if status != 0 || state.closed || gate.blocked
                || (state.active != 0 && state.quarantined)
            {
                pr_info!(
                    "G17P VM map admission refused: vm={} status={} blocked={} active={} quarantined={} draining={} closed={}\n",
                    self.id, status, gate.blocked, state.active,
                    state.quarantined, state.draining, state.closed
                );
                return Err(if state.closed { ENOENT } else { EIO });
            }
            if !state.draining {
                if wait_started != 0 { crate::g17_mem_path::ADMISSION_WAIT.since(wait_started); }
                return Ok(());
            }
            if !waited {
                wait_started = crate::g17_mem_path::stamp();
                pr_info!(
                    "G17P VM map admission waiting for retirement: vm={} status={} blocked={} active={} quarantined={} draining={} closed={}\n",
                    self.id, status, gate.blocked, state.active,
                    state.quarantined, state.draining, state.closed
                );
                waited = true;
            }
            core::mem::drop(state);
            // Drop gate atomically with sleeping. The drain clears draining
            // under gate -> lifetime and then notifies this condition.
            if self.mapping_gate.idle.wait_interruptible(&mut gate) {
                return Err(ERESTARTSYS);
            }
        }
    }

    /// Allocate every deferred-unmap entry a batch could need before it
    /// changes a visible mapping.
    pub(crate) fn reserve_deferred_user_unmaps(&self, count: usize) -> Result {
        self.job_lifetime.lock().deferred_unmaps.reserve(count, GFP_KERNEL)?;
        Ok(())
    }

    /// Settle a batch which lost mapping admission while it was waiting for
    /// dma-resv. Maps are refused; teardown unmaps are retained behind the
    /// firmware-owned job. Returns false if the last job finished and reopened
    /// admission before this recheck, in which case the caller may retry.
    ///
    /// The mapping gate stays held through deferred-unmap insertion, making
    /// the append atomic with the last job guard's drain/reopen decision.
    fn defer_blocked_user_bind_batch(
        &self,
        batch: &PreparedUserBindBatch,
    ) -> Result<bool> {
        let gate = self.mapping_gate.state.lock();
        if !gate.blocked {
            return Ok(false);
        }
        let mut lifetime = self.job_lifetime.lock();
        if lifetime.closed {
            return Err(ENOENT);
        }
        if batch
            .ops
            .iter()
            .any(|op| matches!(op, PreparedUserBindOp::Map(_)))
        {
            return Err(EIO);
        }
        if !lifetime.quarantined {
            return Err(EBUSY);
        }
        for op in &batch.ops {
            let PreparedUserBindOp::Unmap(unmap) = op else {
                continue;
            };
            if !lifetime
                .deferred_unmaps
                .iter()
                .any(|range| *range == (unmap.iova, unmap.size))
            {
                lifetime
                    .deferred_unmaps
                    .push((unmap.iova, unmap.size), GFP_KERNEL)?;
            }
        }
        Ok(true)
    }

    /// Commit a map whose fallible backing and page-table resources were
    /// prepared before the containing VM_BIND batch began.
    pub(crate) fn bind_prepared_object(&self, prepared: PreparedUserMap) -> Result {
        let mut batch = PreparedUserBindBatch::new(1)?;
        batch.push_map(prepared)?;
        self.commit_prepared_user_bind_batch(&mut batch)
    }

    /// Enter one already-prepared leaf transaction after its interruptible
    /// dma-resv acquisition. A concurrent lost-job quarantine closes this
    /// gate and waits for the returned guard before publishing its fence.
    fn enter_user_mapping_commit(&self) -> Result<Option<T8140VmMappingCommitGuard>> {
        let mut gate = self.mapping_gate.state.lock();
        if gate.blocked {
            return Ok(None);
        }
        gate.commits = gate.commits.checked_add(1).ok_or(EOVERFLOW)?;
        Ok(Some(T8140VmMappingCommitGuard {
            gate: self.mapping_gate.clone(),
        }))
    }

    /// Commit a fully prepared VM_BIND batch through the kernel ABI's
    /// interruptible drm_exec path. The currently deployed kernel's array
    /// helper rejects its documented zero-fence mode, so use the established
    /// one-object transaction until that core fix is part of the booted Image.
    /// Preparation has already resolved every handle and allocated the host
    /// callback state before this loop begins. Page-table allocation remains
    /// inside `sm_map`, exactly as on the hardware-proven direct path.
    pub(crate) fn commit_prepared_user_bind_batch(
        &self,
        batch: &mut PreparedUserBindBatch,
    ) -> Result {
        // Avoid a reservation wait when quarantine/close was already visible.
        // Keep the gate held through deferred-unmap insertion so the last job
        // guard cannot reopen admission between observation and append.
        if self.defer_blocked_user_bind_batch(batch)? {
            return Ok(());
        }

        let mut index = 0usize;
        while index < batch.ops.len() {
            let mut admission_lost = false;
            let result = (|| -> Result {
                match &mut batch.ops[index] {
                    PreparedUserBindOp::Map(map) => {
                        let mut inner = self.inner.exec_lock(Some(&map.gem), true)?;
                        let Some(commit) = self.enter_user_mapping_commit()? else {
                            core::mem::drop(inner);
                            admission_lost = true;
                            return Ok(());
                        };
                        let (flags, gem_range) = if map.single_page {
                            (gpuvm::GpuVaFlags::REPEAT, UAT_PGSZ as u32)
                        } else {
                            (gpuvm::GpuVaFlags::NONE, 0u32)
                        };
                        mod_dev_dbg!(
                            inner.dev,
                            "MMU: sm_map: {:#x} [{:#x}] -> {:#x}\n",
                            map.offset,
                            map.size,
                            map.addr
                        );
                        let result = inner.sm_map(
                            &mut map.ctx,
                            map.addr,
                            map.size,
                            map.offset,
                            gem_range,
                            flags,
                        );
                        core::mem::drop(inner);
                        core::mem::drop(commit);
                        result
                    }
                    PreparedUserBindOp::Unmap(unmap) => {
                        let mut inner = self.inner.exec_lock(None, true)?;
                        let Some(commit) = self.enter_user_mapping_commit()? else {
                            core::mem::drop(inner);
                            admission_lost = true;
                            return Ok(());
                        };
                        mod_dev_dbg!(
                            inner.dev,
                            "MMU: sm_unmap: {:#x}:{:#x}\n",
                            unmap.iova,
                            unmap.size
                        );
                        let result = inner.sm_unmap(&mut unmap.ctx, unmap.iova, unmap.size);
                        core::mem::drop(inner);
                        core::mem::drop(commit);
                        result
                    }
                }
            })();
            if admission_lost {
                if self.defer_blocked_user_bind_batch(batch)? {
                    return Ok(());
                }
                continue;
            }
            result?;
            index += 1;
        }
        Ok(())
    }

    /// Add a direct MMIO mapping to this Vm at a free address.
    pub(crate) fn map_io(
        &self,
        iova: u64,
        phys: usize,
        size: usize,
        prot: Prot,
    ) -> Result<KernelMapping> {
        let mut inner = self.inner.exec_lock(None, false)?;

        if (iova as usize | phys | size) & UAT_PGMSK != 0 {
            dev_err!(
                inner.dev.as_ref(),
                "MMU: KernelMapping {:#x}:{:#x} -> {:#x} is not page-aligned\n",
                phys,
                size,
                iova
            );
            return Err(EINVAL);
        }

        dev_info!(
            inner.dev.as_ref(),
            "MMU: IO map: {:#x}:{:#x} -> {:#x}\n",
            phys,
            size,
            iova
        );

        let uat_inner = inner.uat_inner.clone();
        let node = inner.mm.reserve_node(
            KernelMappingInner {
                owner: self.inner.clone(),
                uat_inner,
                prot,
                bo: None,
                _gem: None,
                offset: 0,
                mapped_size: size,
            },
            iova,
            size as u64,
            0,
        )?;

        let validation_change = inner.note_mapping_validation_change(iova..iova.checked_add(size as u64).ok_or(EOVERFLOW)?);
        let prepared = inner
            .page_table
            .prepare_map(iova..iova.checked_add(size as u64).ok_or(EOVERFLOW)?);
        if let Err(error) = prepared {
            // Drop the VM lock before the reserved node and its owner refs.
            drop(validation_change);
            core::mem::drop(inner);
            return Err(error);
        }
        let ret = inner.page_table.map_pages(
            iova..(iova + size as u64),
            phys as PhysicalAddr,
            prot,
            false,
        );
        // Drop the exec_lock first, so that if map_node failed the
        // KernelMappingInner destructur does not deadlock.
        drop(validation_change);
        core::mem::drop(inner);
        ret?;
        Ok(KernelMapping(node))
    }

    /// Unmap one explicitly released userspace range.
    ///
    /// DRM VM_BIND callers synchronize healthy ranges before unbinding them,
    /// so unrelated live work must not impose a VM-wide EBUSY gate. A job
    /// which signalled an error while still firmware-owned is different:
    /// userspace may immediately destroy its mappings, and those must remain
    /// until the quarantined job guard is released after processor stop.
    pub(crate) fn unmap_user_range(&self, iova: u64, size: u64) -> Result {
        let prepared = self.prepare_user_unmap(iova, size)?;
        self.unmap_prepared_user_range(prepared)
    }

    /// Commit a preallocated user unmap, or enqueue it behind a quarantined
    /// packet without discarding its retained backing.
    pub(crate) fn unmap_prepared_user_range(
        &self,
        prepared: PreparedUserUnmap,
    ) -> Result {
        let mut batch = PreparedUserBindBatch::new(1)?;
        batch.push_unmap(prepared)?;
        self.commit_prepared_user_bind_batch(&mut batch)
    }

    /// Unmap everything in an address range. Callers must hold the job
    /// lifetime lock or operate on driver-private mappings.
    fn unmap_range(&self, iova: u64, size: u64) -> Result {
        let prepared = self.prepare_user_unmap(iova, size)?;
        self.unmap_prepared_range(prepared)
    }

    /// Reserve the at-most-two split nodes an unmap can require.
    pub(crate) fn prepare_user_unmap(
        &self,
        iova: u64,
        size: u64,
    ) -> Result<PreparedUserUnmap> {
        Ok(PreparedUserUnmap {
            ctx: StepContext {
                prev_va: Some(gpuvm::GpuVa::<VmInner>::new(pin_init::default())?),
                next_va: Some(gpuvm::GpuVa::<VmInner>::new(pin_init::default())?),
                ..Default::default()
            },
            iova,
            size,
        })
    }

    /// Commit an unmap using split nodes allocated during batch preflight.
    pub(crate) fn unmap_prepared_range(
        &self,
        mut prepared: PreparedUserUnmap,
    ) -> Result {
        let mut inner = self.inner.exec_lock(None, false)?;
        mod_dev_dbg!(
            inner.dev,
            "MMU: sm_unmap: {:#x}:{:#x}\n",
            prepared.iova,
            prepared.size
        );
        inner.sm_unmap(&mut prepared.ctx, prepared.iova, prepared.size)
    }

    /// Drop mappings for a given bo.
    pub(crate) fn drop_mappings(&self, gem: &gem::Object) -> Result {
        let mut state = self.job_lifetime.lock();
        if state.active != 0 {
            if !state.closed_objects.iter().any(|object| core::ptr::eq(&**object, gem)) {
                state.closed_objects.push(gem.into(), GFP_KERNEL)?;
                crate::g17_mem_path::count(&crate::g17_mem_path::PARKED, 1);
            }
            return Ok(());
        }
        self.drop_mappings_now(gem)
    }

    /// v94: unmap and release closed objects captured at a VM-idle instant
    /// (no accepted job can reach them), then run deferred GPUVM cleanup.
    fn release_idle_closed_objects(
        &self, objects: KVec<ARef<gem::Object>>, stat: &crate::g17_mem_path::Stat,
    ) {
        let started = crate::g17_mem_path::stamp();
        let released = objects.len() as u64;
        for object in objects.iter() {
            if self.drop_mappings_now(object).is_err() {
                pr_err!("G17P VM idle release: closed GEM unmap failed\n");
            }
        }
        drop(objects);
        self.bo_deferred_cleanup();
        stat.since(started);
        crate::g17_mem_path::count(&crate::g17_mem_path::DRAIN_OBJECTS, released);
    }

    fn drop_mappings_now(&self, gem: &gem::Object) -> Result {
        // Removing whole mappings only does unmaps, so no preallocated VAs
        let mut ctx = Default::default();

        let inner = self.inner.exec_lock(Some(gem), false)?;

        if let Some(bo) = self.inner.find_bo(gem) {
            mod_dev_dbg!(inner.dev, "MMU: bo_unmap\n");
            self.inner.bo_unmap(&mut ctx, &bo)?;
            mod_dev_dbg!(inner.dev, "MMU: bo_unmap done\n");
            // We need to drop the exec_lock first, then the GpuVmBo since that will take the lock itself.
            core::mem::drop(inner);
            core::mem::drop(bo);
        } else {
            core::mem::drop(inner);
        }

        // Match metadata retirement to the actual GPUVM unmap. Handle close
        // may have deferred it behind accepted jobs and their mapping pins.
        self.untrack_t8140_native_context_object(gem);
        Ok(())
    }

    /// Returns the dummy GEM object used to hold the shared DMA reservation locks
    pub(crate) fn get_resv_obj(&self) -> ARef<gem::Object> {
        self.dummy_obj.clone()
    }

    /// Check whether an object is external to this GpuVm
    pub(crate) fn is_extobj(&self, gem: &gem::Object) -> bool {
        self.inner.is_extobj(gem)
    }

    /// Check whether an object is external to this GpuVm
    pub(crate) fn bo_deferred_cleanup(&self) {
        self.inner.bo_deferred_cleanup()
    }
}

impl Drop for VmInner {
    fn drop(&mut self) {
        let mut binding = self.binding.lock();
        assert_eq!(binding.active_users, 0);

        mod_pr_debug!(
            "VmInner::Drop [{}]: bind_token={:?}\n",
            self.id,
            binding.bind_token
        );

        // Make sure this VM is not mapped to a TTB if it was
        let bind_context = binding.bind_context.take();
        if let Some(token) = binding.bind_token.take() {
            // `bind_context` is the authority: on T8140 the slot index and the
            // hardware context are different numbers, so the historical
            // `slot + UAT_USER_CTX_START` is only the fallback for a token
            // that predates a context ever being recorded.
            let _ = &token;
            let idx = bind_context
                .map(|context| context as usize)
                .unwrap_or_else(|| (token.last_slot() as usize) + UAT_USER_CTX_START);
            let ttb = self.ttb() | TTBR_VALID | (idx as u64) << TTBR_ASID_SHIFT;

            // The G17P compute path publishes this VM's root again at the
            // kick's own hardware contexts (see
            // `Uat::install_t8140_compute_context_alias`). Nothing else owns
            // those entries, so they have to be retracted here or the firmware
            // keeps translation roots pointing at freed page tables. Keyed on
            // the value, so a context published for some other VM is left
            // alone and the parameter's setting cannot strand an entry.
            let uat_inner = self.uat_inner.lock();
            uat_inner.handoff().lock();
            let handoff_cur = uat_inner.handoff().current_slot();
            let ttb_cur = uat_inner.ttbs()[idx].ttb0.load(Ordering::SeqCst);
            let inval = ttb_cur == ttb;
            if inval {
                if handoff_cur == Some(idx as u32) {
                    pr_err!(
                        "VmInner::drop owning slot {}, but it is currently in use by the ASC?\n",
                        idx
                    );
                }
                uat_inner.ttbs()[idx].ttb0.store(0, Ordering::SeqCst);
                uat_inner.ttbs()[idx].ttb1.store(0, Ordering::SeqCst);
            }
            let mut dropped_aliases = [false; T8140_COMPUTE_CTXS.len()];
            // Retract only aliases THIS VM published.
            //
            // The old test compared the stored TTB0 against this VM's root
            // *value*. A VM's root is a single page from the page allocator,
            // so the page this VM is about to free can be handed straight back
            // to the next VM created -- and then this drop matches on the new
            // VM's identical root and retracts the aliases that VM just
            // published, leaving hardware contexts 2/3 at zero with no
            // translation regime at all. The kick is then accepted and
            // silently never runs. Keying on the publisher's `Vm::id`, which
            // `install_t8140_compute_context_alias` records, makes the
            // mismatch impossible to construct.
            let alias_owner = self.uat_inner.t8140_alias_owner.load(Ordering::Acquire);
            if alias_owner == self.id {
                for (slot, ctx) in T8140_COMPUTE_CTXS.into_iter().enumerate() {
                    if idx == ctx {
                        continue;
                    }
                    uat_inner.ttbs()[ctx].ttb0.store(0, Ordering::SeqCst);
                    uat_inner.ttbs()[ctx].ttb1.store(0, Ordering::SeqCst);
                    dropped_aliases[slot] = true;
                }
                self.uat_inner.t8140_alias_owner.store(0, Ordering::Release);
            }
            uat_inner.handoff().unlock();
            core::mem::drop(uat_inner);

            // Native publishes the complete GPTBAT pair, executes DMB ISH,
            // and only then invalidates the affected ASID. Keep the pair
            // ordered ahead of every following TLBI on teardown as well.
            fence(Ordering::SeqCst);
            for (slot, ctx) in T8140_COMPUTE_CTXS.into_iter().enumerate() {
                if dropped_aliases[slot] {
                    mem::tlbi_asid(ctx as u8);
                    mem::sync();
                }
            }

            // Say which TTBAT entries this drop retracted.
            //
            // This is the only path that zeroes a context root without logging
            // anything, and a zeroed root is not a quiet condition: a VM with no
            // TTBAT context holding its root has no translation regime, so its
            // kicks are accepted by firmware and silently never run -- which is
            // the wedge, reported two seconds later as a completion timeout with
            // no fault anywhere. Diagnosing one such wedge took four rounds and a
            // live machine precisely because nothing here left a trace. One line
            // per VM teardown that actually retracted something is cheap.
            if inval || dropped_aliases.iter().any(|dropped| *dropped) {
                pr_info!(
                    "MMU: VM {} teardown retracted TTBAT roots: bind-slot {}={}, compute aliases {:?}\n",
                    self.id,
                    idx,
                    inval,
                    dropped_aliases,
                );
            }

            // In principle we dropped all the KernelMappings already, but we might as
            // well play it safe and invalidate the whole ASID.
            if inval {
                mod_pr_debug!(
                    "VmInner::Drop [{}]: need inval for ASID {:#x}\n",
                    self.id,
                    idx
                );
                mem::tlbi_asid(idx as u8);
                mem::sync();
            }
        }
    }
}

impl Uat {
    /// Map a bootloader-preallocated memory region
    fn map_region(
        dev: &device::Device,
        name: &CStr,
        size: usize,
        cached: bool,
    ) -> Result<UatRegion> {
        let of_node = dev.of_node().ok_or(EINVAL)?;
        let res = of_node.reserved_mem_region_to_resource_byname(name)?;
        let base = res.start();
        let res_size = res.size().try_into()?;

        if size > res_size {
            dev_err!(
                dev,
                "Region {} is too small (expected {}, got {})\n",
                name,
                size,
                res_size
            );
            return Err(ENOMEM);
        }

        let flags = if cached {
            io::mem::MemFlag::WB
        } else {
            io::mem::MemFlag::WC
        };

        // SAFETY: The safety of this operation hinges on the correctness of
        // much of this file and also the `pgtable` module, so it is difficult
        // to prove in a single safety comment. Such is life with raw GPU
        // page table management...
        let map = unsafe { io::mem::Mem::try_new(res, flags.into()) }.inspect_err(|_| {
            dev_err!(dev, "Failed to remap {} mem resource\n", name);
        })?;

        Ok(UatRegion { base, map })
    }

    /// Final teardown only: the caller has proved both firmware CPUs stopped
    /// and joined every publisher. Never use this for a restartable recovery.
    pub(crate) fn mark_firmware_cache_stopped(&self) {
        self.inner.firmware_cache_stopped.store(true, Ordering::SeqCst);
    }

    /// Returns a reference to the global kernel (upper half) `Vm`
    pub(crate) fn kernel_vm(&self) -> &Vm {
        &self.kernel_vm
    }

    /// Returns a reference to the local kernel (lower half) `Vm`
    pub(crate) fn kernel_lower_vm(&self) -> &Vm {
        &self.kernel_lower_vm
    }

    /// A memory-layout consumer must not confuse identical root geometry
    /// with target admission. This checks identity only; it does not create
    /// an M5 UAT or authorize an absent firmware handoff protocol.
    pub(crate) fn is_t8142_geometry(&self) -> bool {
        self.cfg.chip_id == 0x8142 && self.cfg.ias == 42 && self.cfg.oas == 42
    }

    /// The T8152 counterpart of [`Uat::is_t8142_geometry`]; identity only.
    /// Consumed by the G18 live-boot module.
    #[allow(dead_code)]
    pub(crate) fn is_t8152_geometry(&self) -> bool {
        self.cfg == UatConfig::T8152
    }

    pub(crate) fn t8140_render_global_vm(&self) -> Result<&Vm> {
        if self.cfg != UatConfig::T8140 {
            return Err(EINVAL);
        }
        self.t8140_render_global_vm.as_ref().ok_or(EIO)
    }

    /// Materialize native's required restricted process-context views in the
    /// retained lower root used by hardware context 0.
    ///
    /// The source VM registry supplies GEM identity and object offsets; the
    /// source and destination page-table walks are then compared at both ends
    /// of every range before either ASID is invalidated. This makes a wrong BO,
    /// stale rebind, or partial slot fail on the host instead of becoming an
    /// opaque dIPP fault.
    pub(crate) fn install_t8140_native_context_aliases(
        &self,
        source: &Vm,
    ) -> Result<T8140NativeContextAliases> {
        if self.cfg != UatConfig::T8140 {
            return Err(EINVAL);
        }
        // Clone the backing references under the small registry lock, then
        // release it before taking GEM reservation and page-table locks. This
        // keeps the lock order independent of future GPUVM teardown changes.
        let (source_generation, snapshot) = {
            let native_bindings = source
                .t8140_native_context_bindings
                .as_ref()
                .ok_or(EINVAL)?;
            let bindings = native_bindings.lock();
            let mut snapshot: KVec<(ARef<gem::Object>, usize)> = KVec::new();
            for (index, view) in T8140_NATIVE_CONTEXT_VIEWS.iter().copied().enumerate() {
                let binding = bindings.slots[index].as_ref().ok_or_else(|| {
                    dev_err!(
                        self.dev.as_ref(),
                        "MMU: T8140 native context view {} source {:#x}+{:#x} -> {:#x} is not bound\n",
                        index,
                        view.source_address,
                        view.size,
                        view.context_address,
                    );
                    ENOENT
                })?;
                snapshot.push((binding.gem.clone(), binding.object_offset), GFP_KERNEL)?;
            }
            (bindings.generation, snapshot)
        };
        let mut aliases = T8140NativeContextAliases {
            source_vm_id: source.id,
            source_generation,
            mappings: KVec::new(),
            client_mapping: None,
            inner: self.inner.clone(),
        };

        for (index, (gem, object_offset)) in snapshot.iter().enumerate() {
            let view = T8140_NATIVE_CONTEXT_VIEWS[index];
            // The native mapInThisGart selector and each resource's map class
            // determine the exact destination leaf. View 0 is restricted
            // class 1 (UAT option 0x108); views 0-4 have captured native raw
            // attributes ...0xc8b, and views 5-6 use the RW high-bit form with
            // the same low attributes. The ordinary source mapping may be more
            // permissive, but it must at least provide the data access that
            // Linux uses to back the restricted view.
            if !source.covers_range(
                view.source_address,
                view.size as u64,
                true,
                view.requires_source_write,
            ) {
                dev_err!(
                    self.dev.as_ref(),
                    "MMU: T8140 native context view {} source {:#x}+{:#x} lacks GPU access (write={})\n",
                    index,
                    view.source_address,
                    view.size,
                    view.requires_source_write,
                );
                return Err(EFAULT);
            }
            let object_end = object_offset.checked_add(view.size).ok_or(EOVERFLOW)?;
            let mapping = self.kernel_lower_vm.map_in_range(
                &**gem,
                *object_offset..object_end,
                UAT_PGSZ as u64,
                view.context_address..view.context_address + view.size as u64,
                view.context_prot,
                false,
            )?;
            aliases.mappings.push(mapping, GFP_KERNEL)?;

            let source_last_address = view.source_address + view.size as u64 - 1;
            let context_last_address = view.context_address + view.size as u64 - 1;
            let source_first = source.translate_iova(view.source_address)?;
            let source_last = source.translate_iova(source_last_address)?;
            let context_first = self
                .kernel_lower_vm
                .translate_iova(view.context_address)?;
            let context_last = self
                .kernel_lower_vm
                .translate_iova(context_last_address)?;
            if source_first != context_first || source_last != context_last {
                dev_err!(
                    self.dev.as_ref(),
                    "MMU: T8140 native context view {} {:#x}->{:#x} backing mismatch source=[{:#x},{:#x}] context0=[{:#x},{:#x}]\n",
                    index,
                    view.source_address,
                    view.context_address,
                    source_first,
                    source_last,
                    context_first,
                    context_last,
                );
                return Err(EFAULT);
            }
            dev_info!(
                self.dev.as_ref(),
                "MMU: T8140 native context view {} source {:#x} -> context0 {:#x}+{:#x} aliases object +{:#x}, PA [{:#x},{:#x}] write={}\n",
                index,
                view.source_address,
                view.context_address,
                view.size,
                object_offset,
                context_first,
                context_last,
                view.requires_source_write,
            );
        }

        // Native also presents view 0 at low VA 0 inside the submitting
        // process's own regime. Hardware asks for it there -- gUPM writes
        // 0x1e00..0x1f80 through the client's own vm-slot -- and userspace
        // cannot supply it, because the published user window starts at
        // IOVA_USER_BASE. Same GEM, same object offset, same physical pages as
        // the canonical high mapping and the context-0 copy.
        {
            let view = T8140_CLIENT_NULL_VIEW;
            let (gem, object_offset) = snapshot.first().ok_or(EINVAL)?;
            if !source.covers_range(
                view.source_address,
                view.size as u64,
                true,
                view.requires_source_write,
            ) {
                dev_err!(
                    self.dev.as_ref(),
                    "MMU: T8140 client null view source {:#x}+{:#x} lacks GPU write access\n",
                    view.source_address,
                    view.size,
                );
                return Err(EFAULT);
            }
            let object_end = object_offset.checked_add(view.size).ok_or(EOVERFLOW)?;
            let mapping = source.map_in_range(
                &**gem,
                *object_offset..object_end,
                UAT_PGSZ as u64,
                view.context_address..view.context_address + view.size as u64,
                view.context_prot,
                false,
            )?;
            aliases.client_mapping = Some(mapping);

            let source_last_address = view.source_address + view.size as u64 - 1;
            let context_last_address = view.context_address + view.size as u64 - 1;
            let source_first = source.translate_iova(view.source_address)?;
            let source_last = source.translate_iova(source_last_address)?;
            let client_first = source.translate_iova(view.context_address)?;
            let client_last = source.translate_iova(context_last_address)?;
            if source_first != client_first || source_last != client_last {
                dev_err!(
                    self.dev.as_ref(),
                    "MMU: T8140 client null view {:#x}->{:#x} backing mismatch source=[{:#x},{:#x}] client=[{:#x},{:#x}]\n",
                    view.source_address,
                    view.context_address,
                    source_first,
                    source_last,
                    client_first,
                    client_last,
                );
                return Err(EFAULT);
            }
            dev_info!(
                self.dev.as_ref(),
                "MMU: T8140 client null view source {:#x} -> vm {} low {:#x}+{:#x} aliases object +{:#x}, PA [{:#x},{:#x}] writable\n",
                view.source_address,
                source.id,
                view.context_address,
                view.size,
                object_offset,
                client_first,
                client_last,
            );
        }

        // A concurrent rebind can race the snapshot but cannot silently win:
        // physical-identity checks above catch changed leaves, and the
        // generation check catches a same-backing unbind/rebind sequence.
        if source
            .t8140_native_context_bindings
            .as_ref()
            .ok_or(EINVAL)?
            .lock()
            .generation
            != source_generation
        {
            dev_err!(
                self.dev.as_ref(),
                "MMU: T8140 native context bindings changed during alias installation\n"
            );
            return Err(EAGAIN);
        }

        // kernel_lower_vm is installed directly in contexts 0/1 rather than
        // through Vm::bind(), so mapping cannot infer a slot and cannot flush a
        // retained negative translation by itself.
        fence(Ordering::SeqCst);
        mem::tlbi_asid(0);
        mem::tlbi_asid(1);
        // The client root is rooted in its render slot and in every live
        // application-GART lease; a stale negative translation in any of them
        // would fault exactly like the missing leaf did.
        let client_contexts = self.inner.t8140_app_contexts.load(Ordering::Acquire);
        for context_id in 0..UAT_NUM_CTX {
            if client_contexts & (1u64 << context_id) != 0 {
                mem::tlbi_asid(context_id as u8);
            }
        }
        mem::sync();
        dev_info!(
            self.dev.as_ref(),
            "MMU: published all {} native context aliases plus the client null view for VM {} generation {} to ASIDs 0/1 and contexts {:#x}\n",
            aliases.mappings.len(),
            source.id,
            aliases.source_generation,
            client_contexts,
        );
        Ok(aliases)
    }

    /// Validate the complete SecureGart low-view closure in one submitted
    /// job's own process root and materialize only the driver-reserved null
    /// view. Unlike `install_t8140_native_context_aliases`, this never mutates
    /// the singleton context-0 lower VM, so two distinct application-GART
    /// contexts can retain different process resources concurrently.
    pub(crate) fn install_t8140_job_context_aliases(
        &self,
        source: &Vm,
    ) -> Result {
        if self.cfg != UatConfig::T8140 {
            return Err(EINVAL);
        }
        let (source_generation, snapshot) = {
            let native_bindings = source
                .t8140_native_context_bindings
                .as_ref()
                .ok_or(EINVAL)?;
            let bindings = native_bindings.lock();
            let mut snapshot: KVec<(ARef<gem::Object>, usize)> = KVec::new();
            for (index, view) in T8140_NATIVE_CONTEXT_VIEWS.iter().copied().enumerate() {
                let binding = bindings.slots[index].as_ref().ok_or_else(|| {
                    dev_err!(
                        self.dev.as_ref(),
                        "MMU: T8140 job context view {} source {:#x}+{:#x} -> {:#x} is not bound\n",
                        index,
                        view.source_address,
                        view.size,
                        view.context_address,
                    );
                    if index == 0 { context_unbound_error() } else { ENOENT }
                })?;
                snapshot.push((binding.gem.clone(), binding.object_offset), GFP_KERNEL)?;
            }
            (bindings.generation, snapshot)
        };

        // Mesa already binds low views 1..6 from the same context BO.  Those
        // GPUVM mappings are the process-GART owner and must never be covered
        // by a second KernelMapping: the two allocators are independent, so a
        // duplicate mapping used to overwrite Mesa's PTEs and later erase
        // them when the duplicate owner dropped. Validate every existing low
        // presentation page-for-page before adding the sole missing view 0.
        for (index, (gem, object_offset)) in snapshot.iter().enumerate().skip(1) {
            let view = T8140_NATIVE_CONTEXT_VIEWS[index];
            let source_covered = source.covers_range(
                view.source_address,
                view.size as u64,
                true,
                view.requires_source_write,
            );
            let context_covered = source.covers_range(
                view.context_address,
                view.size as u64,
                true,
                view.requires_source_write,
            );
            if !source_covered || !context_covered {
                dev_err!(self.dev.as_ref(),
                    "MMU: T8140 job context VM {} view {} incomplete: source={:#x} covered={} low={:#x} covered={} size={:#x} write={}\n",
                    source.id, index, view.source_address, source_covered,
                    view.context_address, context_covered, view.size,
                    view.requires_source_write);
                return Err(EFAULT);
            }
            let object_end = object_offset.checked_add(view.size).ok_or(EOVERFLOW)?;
            if object_end > gem.size() {
                return Err(ERANGE);
            }
            let mut offset = 0u64;
            while offset < view.size as u64 {
                let high = view.source_address.checked_add(offset).ok_or(EOVERFLOW)?;
                let low = view.context_address.checked_add(offset).ok_or(EOVERFLOW)?;
                if source.translate_iova(high)? != source.translate_iova(low)? {
                    dev_err!(
                        self.dev.as_ref(),
                        "MMU: T8140 job context view {} backing mismatch at +{:#x}\n",
                        index,
                        offset,
                    );
                    return Err(EFAULT);
                }
                offset = offset.checked_add(UAT_PGSZ as u64).ok_or(EOVERFLOW)?;
            }
        }

        source.retain_job_context_aliases(|| {
            let mut mappings = KVec::with_capacity(1, GFP_KERNEL)?;
            let view = T8140_CLIENT_NULL_VIEW;
            let (gem, object_offset) = snapshot.first().ok_or(EINVAL)?;
            if !source.covers_range(
                view.source_address,
                view.size as u64,
                true,
                view.requires_source_write,
            ) {
                dev_err!(self.dev.as_ref(),
                    "MMU: T8140 job context VM {} null source={:#x}+{:#x} lacks required permissions (write={})\n",
                    source.id, view.source_address, view.size, view.requires_source_write);
                return Err(EFAULT);
            }
            let object_end = object_offset.checked_add(view.size).ok_or(EOVERFLOW)?;
            if object_end > gem.size() {
                return Err(ERANGE);
            }
            // The null view is VM-owned, not render-storage-owned. A single
            // process may create another render queue while its first storage is
            // still alive, in which case this VM transaction already retains
            // this exact alias. Never create a second KernelMapping over those
            // leaves: map_in_range correctly rejects the overlap, and a duplicate
            // owner would later erase the first storage's live mapping on Drop.
            //
            // Accept only the complete, physically identical presentation. A
            // partial range or another backing at the reserved address remains a
            // hard error rather than being mistaken for idempotent installation.
            let mut existing_pages = 0usize;
            let mut offset = 0u64;
            while offset < view.size as u64 {
                let high = view.source_address.checked_add(offset).ok_or(EOVERFLOW)?;
                let low = view.context_address.checked_add(offset).ok_or(EOVERFLOW)?;
                if source.covers_range(
                    low,
                    UAT_PGSZ as u64,
                    true,
                    view.requires_source_write,
                ) {
                    existing_pages = existing_pages.checked_add(1).ok_or(EOVERFLOW)?;
                    if source.translate_iova(high)? != source.translate_iova(low)? {
                        dev_err!(
                            self.dev.as_ref(),
                            "MMU: T8140 retained client null view backing mismatch at +{:#x}\n",
                            offset,
                        );
                        return Err(EFAULT);
                    }
                }
                offset = offset.checked_add(UAT_PGSZ as u64).ok_or(EOVERFLOW)?;
            }
            let expected_pages = view.size / UAT_PGSZ;
            if existing_pages != 0 {
                if existing_pages != expected_pages {
                    dev_err!(
                        self.dev.as_ref(),
                        "MMU: T8140 retained client null view is only partially mapped ({}/{})\n",
                        existing_pages,
                        expected_pages,
                    );
                    return Err(EFAULT);
                }
                if source
                    .t8140_native_context_bindings
                    .as_ref()
                    .ok_or(EINVAL)?
                    .lock()
                    .generation
                    != source_generation
                {
                    return Err(EAGAIN);
                }
                if crate::util::g17p_verbose() {
                    dev_info!(
                        self.dev.as_ref(),
                        "MMU: T8140 job context reused all {} GPUVM low views including retained null view for VM {} generation {}\n",
                        T8140_NATIVE_CONTEXT_VIEW_COUNT,
                        source.id,
                        source_generation,
                    );
                }
                return Ok(mappings);
            }
            let mapping = source.map_in_range(
                &**gem,
                *object_offset..object_end,
                UAT_PGSZ as u64,
                view.context_address..view.context_address + view.size as u64,
                view.context_prot,
                false,
            )?;
            let mut offset = 0u64;
            while offset < view.size as u64 {
                let high = view.source_address.checked_add(offset).ok_or(EOVERFLOW)?;
                let low = view.context_address.checked_add(offset).ok_or(EOVERFLOW)?;
                if source.translate_iova(high)? != source.translate_iova(low)? {
                    return Err(EFAULT);
                }
                offset = offset.checked_add(UAT_PGSZ as u64).ok_or(EOVERFLOW)?;
            }
            mappings.push(mapping, GFP_KERNEL)?;

            if source
                .t8140_native_context_bindings
                .as_ref()
                .ok_or(EINVAL)?
                .lock()
                .generation
                != source_generation
            {
                return Err(EAGAIN);
            }
            if crate::util::g17p_verbose() {
                dev_info!(
                    self.dev.as_ref(),
                    "MMU: T8140 job context reused {} GPUVM low views and installed only reserved null view for VM {} generation {}\n",
                    T8140_NATIVE_CONTEXT_VIEW_COUNT - 1,
                    source.id,
                    source_generation,
                );
            }
            Ok(mappings)
        })
    }

    /// Invalidate every live T8140 context rooted in `vm` after adding or
    /// replacing job-private aliases in its page tables.
    ///
    /// `KernelMapping` can infer only the ordinary bind context. Render jobs
    /// execute through an independently allocated application context, so a
    /// live VM can otherwise retain a negative or old translation there when
    /// its render-pool generation is replaced. Match the installed TTBAT roots
    /// instead of flushing every client or assuming a fixed context ID.
    /// Which TTBAT contexts currently hold `vm`'s root, as a bitmask.
    ///
    /// Zero is not a benign answer: it means the GPU has no page tables for any
    /// context a descriptor naming this VM could declare, so every submission
    /// against it is undispatchable -- accepted, never executed, and never
    /// completed. Reads only; mutates nothing.
    ///
    /// Split out of `flush_t8140_vm_contexts` so a caller can prove the
    /// condition BEFORE it starts mutating the VM. See `new_user_render_storage`,
    /// where the same lookup used to run several destructive steps too late.
    pub(crate) fn t8140_vm_context_mask(&self, vm: &Vm) -> Result<u64> {
        if self.cfg != UatConfig::T8140 {
            return Err(EINVAL);
        }
        let root = {
            let owner = vm.inner.exec_lock(None, false)?;
            if owner.is_kernel
                || !core::ptr::eq(
                    Arc::as_ptr(&owner.uat_inner),
                    Arc::as_ptr(&self.inner),
                )
            {
                return Err(EINVAL);
            }
            owner.page_table.ttb()
        };
        let inner = self.inner.lock();
        Ok(context_mask_for_root(root, inner.ttbs()))
    }

    /// `ENOENT` unless some TTBAT context holds `vm`'s root.
    ///
    /// The cheap precondition form of `flush_t8140_vm_contexts`, for callers
    /// that must not begin work they cannot finish.
    pub(crate) fn require_t8140_vm_context(&self, vm: &Vm) -> Result {
        if self.t8140_vm_context_mask(vm)? == 0 {
            return Err(ENOENT);
        }
        Ok(())
    }

    pub(crate) fn flush_t8140_vm_contexts(&self, vm: &Vm) -> Result<u64> {
        let contexts = self.t8140_vm_context_mask(vm)?;
        if contexts == 0 {
            return Err(ENOENT);
        }
        fence(Ordering::SeqCst);
        for context_id in 0..UAT_NUM_CTX {
            if contexts & (1u64 << context_id) != 0 {
                mem::tlbi_asid(context_id as u8);
            }
        }
        mem::sync();
        Ok(contexts)
    }

    /// Return a CPU wrapper and the preboot primary-low alias of the persistent
    /// T8140 PM page-metrics object. The firmware-high alias is added only
    /// after firmware has published its upper-root subtree.
    pub(crate) fn t8140_parameter_metrics(&self) -> Result<(gem::ObjectRef, u64)> {
        let low = self
            ._t8140_parameter_metrics_low
            .as_ref()
            .ok_or(ENODEV)?;
        let object = self
            .t8140_parameter_metrics_gem
            .as_ref()
            .ok_or(ENODEV)?;
        Ok((
            gem::ObjectRef::new(object.clone()),
            low.iova(),
        ))
    }

    /// Publish page-table leaves added to T8140's retained lower root after
    /// the firmware processors have started.
    ///
    /// `kernel_lower_vm` is installed directly in contexts 0/1 rather than
    /// acquired through `Vm::bind`, so its runtime map path has no slot from
    /// which to infer an ASID and therefore performs no TLB invalidation.  A
    /// data fence alone leaves the firmware CPU free to retain a level-0 miss.
    pub(crate) fn flush_t8140_runtime_lower_mapping(
        &self,
        address: u64,
        size: usize,
    ) -> Result {
        if self.cfg != UatConfig::T8140 || size == 0 {
            return Err(EINVAL);
        }
        let size_u64 = size as u64;
        let last = address
            .checked_add(size_u64)
            .and_then(|end| end.checked_sub(1))
            .ok_or(EOVERFLOW)?;
        if !self
            .kernel_lower_vm
            .covers_range(address, size_u64, true, true)
        {
            return Err(EFAULT);
        }
        // Resolve both ends before the TLBI so a partially built range fails
        // on the host instead of becoming another opaque firmware exception.
        self.kernel_lower_vm.translate_iova(address)?;
        self.kernel_lower_vm.translate_iova(last)?;

        fence(Ordering::SeqCst);
        // Context 0 owns this root at first work. Context 1 also carries it
        // during earlier phases, so invalidate both ASIDs across the split.
        mem::tlbi_asid(0);
        mem::tlbi_asid(1);
        mem::sync();
        dev_info!(
            self.dev.as_ref(),
            "MMU: flushed T8140 runtime lower mapping {:#x}:{:#x} for ASIDs 0/1\n",
            address,
            size
        );
        Ok(())
    }

    /// Retain a read-only view of the live TTBAT for firmware crash handling.
    pub(crate) fn firmware_context_table_reader(&self) -> FirmwareContextTableReader {
        FirmwareContextTableReader {
            inner: self.inner.clone(),
            cfg: self.cfg,
            t8140_crash_buffers: self.t8140_firmware_crash_gems.as_ref().map(|buffers| {
                [buffers[0].clone(), buffers[1].clone()]
            }),
        }
    }

    /// Return the kernel-half VA range owned by this UAT.
    pub(crate) fn kernel_va_range(&self) -> Result<Range<u64>> {
        iova_kern_range(self.cfg)
    }

    /// Prepare the firmware-published T8140 upper roots for host mappings.
    ///
    /// G17P publishes three primary entries during RTKit boot: two private
    /// code/data entries and the shared L2 entry used by the kernel VA window.
    /// Host mappings must be added beneath that live entry, not beneath a root
    /// image constructed before the processor starts.
    pub(crate) fn prepare_t8140_firmware_high_roots(
        &self,
    ) -> Result<T8140PrimaryRootSnapshot> {
        if !self.cfg.has_g17_private_roots() {
            return Err(EINVAL);
        }
        let primary_phys = self.kernel_vm.ttb();
        let secondary_phys = self.t8140_secondary_ttb1.ok_or(EIO)?;
        let primary = unsafe { Page::borrow_phys(&primary_phys) }.ok_or(EIO)?;
        let secondary = unsafe { Page::borrow_phys(&secondary_phys) }.ok_or(EIO)?;
        let mut primary_entries = [0u64; T8140_FIRMWARE_SHARED_ROOT_ENTRIES];
        let mut secondary_entries = [0u64; T8140_FIRMWARE_SHARED_ROOT_ENTRIES];

        primary.with_pointer_into_page(0, UAT_PGSZ, |pointer| {
            let entries = unsafe {
                core::slice::from_raw_parts_mut(
                    pointer as *mut u64,
                    UAT_PGSZ / core::mem::size_of::<u64>(),
                )
            };
            primary_entries.copy_from_slice(&entries[..T8140_FIRMWARE_SHARED_ROOT_ENTRIES]);
            Ok(())
        })?;
        secondary.with_pointer_into_page(0, UAT_PGSZ, |pointer| {
            let entries = unsafe {
                core::slice::from_raw_parts(
                    pointer as *const u64,
                    UAT_PGSZ / core::mem::size_of::<u64>(),
                )
            };
            secondary_entries.copy_from_slice(&entries[..T8140_FIRMWARE_SHARED_ROOT_ENTRIES]);
            Ok(())
        })?;
        if self.cfg.has_m5_native_topology() {
            let node = self.dev.as_ref().of_node().ok_or(EINVAL)?;
            let shared_l2 = node.reserved_mem_region_to_resource_byname(c_str!("shared-l2"))?;
            let mask = ((1u64 << self.cfg.oas) - 1) & !(UAT_PGMSK as u64);
            if shared_l2.size() != UAT_PGSZ as u64
                || Iterator::chain(primary_entries.iter(), secondary_entries.iter()).any(|pte| pte & 3 != 3)
                || primary_entries[2] & mask != shared_l2.start()
                || secondary_entries[2] & mask != shared_l2.start()
                || primary_entries[0] & mask == secondary_entries[0] & mask
            {
                dev_err!(
                    self.dev.as_ref(),
                    "{} firmware-private roots/shared L2 do not match the native topology\n",
                    (if self.cfg == UatConfig::T8142 { "M5" } else { "T8152" })
                );
                return Err(EIO);
            }
        }
        primary.with_pointer_into_page(0, UAT_PGSZ, |pointer| {
            let entries = unsafe { core::slice::from_raw_parts_mut(
                pointer as *mut u64, UAT_PGSZ / core::mem::size_of::<u64>()) };
            clear_stale_host_root_entries(entries);
            Ok(())
        })?;
        fence(Ordering::SeqCst);
        mem::tlbi_all();
        mem::sync();
        dev_info!(
            self.dev.as_ref(),
            "MMU: GFX high root {:#x} entries [{:#x}, {:#x}, {:#x}]; GFX1 root {:#x} entries [{:#x}, {:#x}, {:#x}]\n",
            primary_phys,
            primary_entries[0],
            primary_entries[1],
            primary_entries[2],
            secondary_phys,
            secondary_entries[0],
            secondary_entries[1],
            secondary_entries[2]
        );
        Ok(T8140PrimaryRootSnapshot {
            entries: primary_entries,
        })
    }

    /// Confirm construction retained the three firmware-published entries.
    pub(crate) fn confirm_t8140_primary_high_root(
        &self,
        snapshot: T8140PrimaryRootSnapshot,
    ) -> Result {
        if !self.cfg.has_g17_private_roots() {
            return Err(EINVAL);
        }
        let primary_phys = self.kernel_vm.ttb();
        let primary = unsafe { Page::borrow_phys(&primary_phys) }.ok_or(EIO)?;
        let mut current = [0u64; T8140_FIRMWARE_SHARED_ROOT_ENTRIES];
        primary.with_pointer_into_page(0, UAT_PGSZ, |pointer| {
            let entries = unsafe {
                core::slice::from_raw_parts(
                    pointer as *const u64,
                    UAT_PGSZ / core::mem::size_of::<u64>(),
                )
            };
            current.copy_from_slice(&entries[..T8140_FIRMWARE_SHARED_ROOT_ENTRIES]);
            Ok(())
        })?;
        if current != snapshot.entries {
            dev_err!(
                self.dev.as_ref(),
                "MMU: GFX high-root firmware entries changed during graph construction: before {:?}, after {:?}\n",
                snapshot.entries,
                current
            );
            return Err(EIO);
        }
        dev_info!(
            self.dev.as_ref(),
            "MMU: GFX high-root firmware entries preserved across graph construction\n"
        );
        Ok(())
    }

    /// Mirror host-visible upper-root entries into GFX1's private top table.
    /// Entries 0 and 1 remain owned by that firmware instance.
    pub(crate) fn mirror_t8140_secondary_high_root(&self) -> Result<usize> {
        if !self.cfg.has_g17_private_roots() {
            return Err(EINVAL);
        }
        let primary_phys = self.kernel_vm.ttb();
        let secondary_phys = self.t8140_secondary_ttb1.ok_or(EIO)?;
        let primary = unsafe { Page::borrow_phys(&primary_phys) }.ok_or(EIO)?;
        let secondary = unsafe { Page::borrow_phys(&secondary_phys) }.ok_or(EIO)?;
        let mirrored = primary.with_pointer_into_page(0, UAT_PGSZ, |primary_pointer| {
            secondary.with_pointer_into_page(0, UAT_PGSZ, |secondary_pointer| {
                let primary_entries = unsafe {
                    core::slice::from_raw_parts(
                        primary_pointer as *const u64,
                        UAT_PGSZ / core::mem::size_of::<u64>(),
                    )
                };
                let secondary_entries = unsafe {
                    core::slice::from_raw_parts_mut(
                        secondary_pointer as *mut u64,
                        UAT_PGSZ / core::mem::size_of::<u64>(),
                    )
                };
                Ok(mirror_host_root_entries(primary_entries, secondary_entries))
            })
        })?;
        fence(Ordering::SeqCst);
        mem::tlbi_all();
        mem::sync();
        dev_info!(
            self.dev.as_ref(),
            "MMU: mirrored {} host high-root entries into GFX1 at {:#x}\n",
            mirrored,
            secondary_phys
        );
        Ok(mirrored)
    }

    /// Arm firmware-backed mapping teardown after the DRM device data slot
    /// contains its live DrmGpu owner. Legacy/Dekker UATs start armed.
    pub(crate) fn mark_firmware_cache_flush_ready(&self) {
        if !self
            .inner
            .firmware_cache_flush_ready
            .swap(true, Ordering::AcqRel)
        {
            dev_info!(self.dev.as_ref(), "MMU: firmware cache flushes armed\n");
        }
    }

    #[cfg(CONFIG_DEV_COREDUMP)]
    pub(crate) fn dump_kernel_pages(&self) -> Result<KVVec<pgtable::DumpedPage>> {
        // Full upper-half range for this SoC's root geometry.
        let full_range = uat_geometry(self.cfg)?.upper_canonical_base()..(!UAT_PGMSK as u64);
        let mut inner = self.kernel_vm.inner.exec_lock(None, false)?;
        inner.page_table.dump_pages(full_range)
    }

    /// Returns the base physical address of the TTBAT region.
    pub(crate) fn ttb_base(&self) -> u64 {
        let inner = self.inner.lock();

        inner.ttbs_rgn.base
    }

    /// Confirm the cold first-render context table before a render VM binds.
    ///
    /// Slots 0 and 1 share the retained low root. Their high roots are either
    /// the source firmware root before first work or the two distinct empty
    /// roots after an earlier render VM has retired.
    pub(crate) fn t8140_partial_opening_kernel_contexts_ready(&self) -> bool {
        if self.cfg != UatConfig::T8140 {
            return false;
        }
        let empty = match self.t8140_empty_high_roots.as_ref() {
            Some(roots) => [roots[0].ttb(), roots[1].ttb()],
            None => return false,
        };
        let low = self.kernel_lower_vm.ttb();
        let source_high = self.kernel_vm.ttb();
        let phase = self.t8140_context_phase.load(Ordering::Acquire);
        let high = match phase {
            T8140_CONTEXT_BOOTSTRAP_SOURCE_HIGH => [source_high, source_high],
            T8140_CONTEXT_BOOTSTRAP_EMPTY_HIGH => empty,
            _ => return false,
        };
        let inner = self.inner.lock();
        let ttbs = inner.ttbs();
        ttbs[0].ttb0.load(Ordering::Acquire) == (low | TTBR_VALID)
            && ttbs[0].ttb1.load(Ordering::Acquire) == (high[0] | TTBR_VALID)
            && ttbs[1].ttb0.load(Ordering::Acquire)
                == (low | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT))
            && ttbs[1].ttb1.load(Ordering::Acquire)
                == (high[1] | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT))
    }

    /// Activate the device-global render FList root before the first user
    /// submission. The boot path has no driver-owned bootstrap render, so the
    /// later bootstrap-retirement restore hook is not reached in the normal
    /// UAPI stage; leaving context 1 on `kernel_lower_vm` makes gUPM's first
    /// operand-table read fault at 0x7000208000.
    pub(crate) fn activate_t8140_render_global_context(&self) -> Result {
        if self.cfg != UatConfig::T8140 {
            return Err(EINVAL);
        }
        let empty = self.t8140_empty_high_roots.as_ref().ok_or(EIO)?;
        let kernel_low = self.kernel_lower_vm.ttb();
        let global_low = self.t8140_render_global_vm()?.ttb();
        let source_high = self.kernel_vm.ttb();
        let phase = self.t8140_context_phase.load(Ordering::Acquire);
        let expected_high = match phase {
            T8140_CONTEXT_BOOTSTRAP_SOURCE_HIGH => source_high,
            T8140_CONTEXT_BOOTSTRAP_EMPTY_HIGH => empty[1].ttb(),
            _ => return Err(EBUSY),
        };
        let inner = self.inner.lock();
        inner.handoff().lock();
        let ttbs = inner.ttbs();
        let ready = ttbs[0].ttb0.load(Ordering::Acquire) == (kernel_low | TTBR_VALID)
            && ttbs[1].ttb0.load(Ordering::Acquire)
                == (kernel_low | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT))
            && ttbs[1].ttb1.load(Ordering::Acquire)
                == (expected_high | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT));
        if !ready {
            inner.handoff().unlock();
            return Err(EIO);
        }
        ttbs[1].ttb0.store(
            global_low | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT),
            Ordering::Release,
        );
        self.t8140_context_phase
            .store(T8140_CONTEXT_USER_OWNED, Ordering::Release);
        inner.handoff().unlock();
        core::mem::drop(inner);

        fence(Ordering::SeqCst);
        mem::tlbi_asid(1);
        mem::sync();
        dev_info!(
            self.dev.as_ref(),
            "MMU: context 1 activated with global render FList root {:#x}\n",
            global_low,
        );
        Ok(())
    }

    /// Confirm the post-opening first-work split. Unlike a steady-state user
    /// bind, the driver-owned bootstrap graph still names firmware-high
    /// objects, so both active slots retain the populated source high root.
    pub(crate) fn t8140_first_work_render_context_ready(
        &self,
        render: &VmBind,
    ) -> bool {
        if self.cfg != UatConfig::T8140
            || render.slot() != 1
            || self.t8140_context_phase.load(Ordering::Acquire) != T8140_CONTEXT_USER_OWNED
        {
            return false;
        }
        let source_high = self.kernel_vm.ttb();
        let expected = t8140_split_render_layout(
            self.kernel_lower_vm.ttb(),
            render.0.ttb(),
            [source_high; 2],
        );
        let inner = self.inner.lock();
        let ttbs = inner.ttbs();
        ttbs[0].ttb0.load(Ordering::Acquire) == expected.low[0]
            && ttbs[0].ttb1.load(Ordering::Acquire) == expected.high[0]
            && ttbs[1].ttb0.load(Ordering::Acquire) == expected.low[1]
            && ttbs[1].ttb1.load(Ordering::Acquire) == expected.high[1]
    }

    /// Confirm the root phase used while firmware consumes the 4/16 opening.
    ///
    /// The native sequence keeps the render VM's low root and the source
    /// firmware high root in both active slots until opcode 0x20 has retired.
    /// The later split is a separate transition at the control-done/work
    /// boundary.
    pub(crate) fn t8140_partial_opening_shared_render_context_ready(
        &self,
        render: &VmBind,
    ) -> bool {
        if self.cfg != UatConfig::T8140
            || render.slot() != 1
            || self.t8140_context_phase.load(Ordering::Acquire)
                != T8140_CONTEXT_OPENING_RENDER_SHARED
        {
            return false;
        }
        let expected =
            t8140_opening_render_layout(render.0.ttb(), self.kernel_vm.ttb());
        let inner = self.inner.lock();
        let ttbs = inner.ttbs();
        ttbs[0].ttb0.load(Ordering::Acquire) == expected.low[0]
            && ttbs[1].ttb0.load(Ordering::Acquire) == expected.low[1]
            && ttbs[0].ttb1.load(Ordering::Acquire) == expected.high[0]
            && ttbs[1].ttb1.load(Ordering::Acquire) == expected.high[1]
    }

    /// Present the render roots in both active slots for the 4/16 opening.
    ///
    /// `render` is first acquired through the normal VM binder so its slot
    /// ownership and lifetime remain governed by [`VmBind`]. No firmware work
    /// is visible between that bind and this root-table transition.
    pub(crate) fn install_t8140_partial_opening_shared_render_roots(
        &self,
        render: &VmBind,
    ) -> Result {
        if self.cfg != UatConfig::T8140 || render.slot() != 1 {
            return Err(EINVAL);
        }
        let empty = self.t8140_empty_high_roots.as_ref().ok_or(EIO)?;
        let retained_low = self.kernel_lower_vm.ttb();
        let render_low = render.0.ttb();
        let empty_high = [empty[0].ttb(), empty[1].ttb()];
        let current = t8140_split_render_layout(retained_low, render_low, empty_high);
        let opening = t8140_opening_render_layout(render_low, self.kernel_vm.ttb());
        let inner = self.inner.lock();
        inner.handoff().lock();
        let ttbs = inner.ttbs();
        let ready = self.t8140_context_phase.load(Ordering::Acquire)
            == T8140_CONTEXT_USER_OWNED
            && ttbs[0].ttb0.load(Ordering::Acquire) == current.low[0]
            && ttbs[1].ttb0.load(Ordering::Acquire) == current.low[1]
            && ttbs[0].ttb1.load(Ordering::Acquire) == current.high[0]
            && ttbs[1].ttb1.load(Ordering::Acquire) == current.high[1];
        if !ready {
            inner.handoff().unlock();
            return Err(EIO);
        }
        ttbs[0].ttb0.store(opening.low[0], Ordering::Release);
        ttbs[1].ttb0.store(opening.low[1], Ordering::Release);
        ttbs[0].ttb1.store(opening.high[0], Ordering::Release);
        ttbs[1].ttb1.store(opening.high[1], Ordering::Release);
        self.t8140_context_phase.store(
            T8140_CONTEXT_OPENING_RENDER_SHARED,
            Ordering::Release,
        );
        inner.handoff().unlock();
        core::mem::drop(inner);

        fence(Ordering::SeqCst);
        mem::tlbi_asid(0);
        mem::tlbi_asid(1);
        mem::sync();
        dev_info!(
            self.dev.as_ref(),
            "MMU: opening roots shared low={:#x?} high={:#x?}\n",
            opening.low,
            opening.high
        );
        Ok(())
    }

    /// Split the shared opening roots into retained context 0 and render VM 1.
    /// Firmware has retired opcode 0x20 and the caller has sent control-done;
    /// no work producer is visible until this transition completes.
    pub(crate) fn split_t8140_partial_opening_render_roots(
        &self,
        render: &VmBind,
    ) -> Result {
        if self.cfg != UatConfig::T8140 || render.slot() != 1 {
            return Err(EINVAL);
        }
        let opening =
            t8140_opening_render_layout(render.0.ttb(), self.kernel_vm.ttb());
        let source_high = self.kernel_vm.ttb();
        let split = t8140_split_render_layout(
            self.kernel_lower_vm.ttb(),
            render.0.ttb(),
            [source_high; 2],
        );
        let inner = self.inner.lock();
        inner.handoff().lock();
        let ttbs = inner.ttbs();
        let ready = self.t8140_context_phase.load(Ordering::Acquire)
            == T8140_CONTEXT_OPENING_RENDER_SHARED
            && ttbs[0].ttb0.load(Ordering::Acquire) == opening.low[0]
            && ttbs[1].ttb0.load(Ordering::Acquire) == opening.low[1]
            && ttbs[0].ttb1.load(Ordering::Acquire) == opening.high[0]
            && ttbs[1].ttb1.load(Ordering::Acquire) == opening.high[1];
        if !ready {
            inner.handoff().unlock();
            return Err(EIO);
        }
        ttbs[0].ttb0.store(split.low[0], Ordering::Release);
        ttbs[1].ttb0.store(split.low[1], Ordering::Release);
        ttbs[0].ttb1.store(split.high[0], Ordering::Release);
        ttbs[1].ttb1.store(split.high[1], Ordering::Release);
        self.t8140_context_phase
            .store(T8140_CONTEXT_USER_OWNED, Ordering::Release);
        inner.handoff().unlock();
        core::mem::drop(inner);

        fence(Ordering::SeqCst);
        mem::tlbi_asid(0);
        mem::tlbi_asid(1);
        mem::sync();
        dev_info!(
            self.dev.as_ref(),
            "MMU: first-work roots split low={:#x?} high={:#x?}\n",
            split.low,
            split.high
        );
        Ok(())
    }

    /// Replace only the two high roots at the measured first-work boundary.
    /// Both low roots remain resident; the source firmware high root remains
    /// retained by `kernel_vm` but is no longer present in the TTBAT.
    pub(crate) fn install_t8140_partial_opening_empty_high_roots(&self) -> Result {
        let empty = self.t8140_empty_high_roots.as_ref().ok_or(EIO)?;
        let low = self.kernel_lower_vm.ttb();
        let source_high = self.kernel_vm.ttb();
        let empty_high = [empty[0].ttb(), empty[1].ttb()];
        let inner = self.inner.lock();
        inner.handoff().lock();
        let ttbs = inner.ttbs();
        let phase = self.t8140_context_phase.load(Ordering::Acquire);
        let low_ready = ttbs[0].ttb0.load(Ordering::Acquire) == (low | TTBR_VALID)
            && ttbs[1].ttb0.load(Ordering::Acquire)
                == (low | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT));
        let changed = match phase {
            T8140_CONTEXT_BOOTSTRAP_SOURCE_HIGH => {
                let source_ready = low_ready
                    && ttbs[0].ttb1.load(Ordering::Acquire) == (source_high | TTBR_VALID)
                    && ttbs[1].ttb1.load(Ordering::Acquire)
                        == (source_high | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT));
                if !source_ready {
                    Err(EIO)
                } else {
                    ttbs[0]
                        .ttb1
                        .store(empty_high[0] | TTBR_VALID, Ordering::Release);
                    ttbs[1].ttb1.store(
                        empty_high[1] | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT),
                        Ordering::Release,
                    );
                    self.t8140_context_phase
                        .store(T8140_CONTEXT_BOOTSTRAP_EMPTY_HIGH, Ordering::Release);
                    Ok(true)
                }
            }
            T8140_CONTEXT_BOOTSTRAP_EMPTY_HIGH => {
                let empty_ready = low_ready
                    && ttbs[0].ttb1.load(Ordering::Acquire) == (empty_high[0] | TTBR_VALID)
                    && ttbs[1].ttb1.load(Ordering::Acquire)
                        == (empty_high[1] | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT));
                if empty_ready {
                    Ok(false)
                } else {
                    Err(EIO)
                }
            }
            _ => Err(EBUSY),
        };
        inner.handoff().unlock();
        core::mem::drop(inner);

        if changed? {
            fence(Ordering::SeqCst);
            mem::tlbi_asid(0);
            mem::tlbi_asid(1);
            mem::sync();
        }
        Ok(())
    }

    /// Install the retained device-global context-1 low root after a completed
    /// driver-owned bootstrap render VM drops. Firmware has retired both
    /// queues before this transition. Ordinary jobs keep context 1 stable and
    /// execute their command data through separately allocated app-GART roots.
    pub(crate) fn restore_t8140_kernel_context_after_bootstrap_render(&self) -> Result {
        if self.cfg != UatConfig::T8140 {
            return Err(EINVAL);
        }
        let empty = self.t8140_empty_high_roots.as_ref().ok_or(EIO)?;
        let context0_low = self.kernel_lower_vm.ttb();
        let context1_low = self.t8140_render_global_vm()?.ttb();
        let inner = self.inner.lock();
        inner.handoff().lock();
        let ttbs = inner.ttbs();
        if self.t8140_context_phase.load(Ordering::Acquire) != T8140_CONTEXT_USER_OWNED
            || ttbs[0].ttb0.load(Ordering::Acquire) != (context0_low | TTBR_VALID)
        {
            inner.handoff().unlock();
            return Err(EIO);
        }
        ttbs[1].ttb0.store(
            context1_low | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT),
            Ordering::Release,
        );
        ttbs[0]
            .ttb1
            .store(empty[0].ttb() | TTBR_VALID, Ordering::Release);
        ttbs[1].ttb1.store(
            empty[1].ttb() | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT),
            Ordering::Release,
        );
        self.t8140_context_phase
            .store(T8140_CONTEXT_BOOTSTRAP_EMPTY_HIGH, Ordering::Release);
        inner.handoff().unlock();
        core::mem::drop(inner);

        fence(Ordering::SeqCst);
        mem::tlbi_asid(0);
        mem::tlbi_asid(1);
        mem::sync();
        Ok(())
    }

    /// Restore the cold T8140 context table after both firmware processors stop.
    ///
    /// The caller must first drop every active [`VmBind`]. Existing user VMs
    /// and their page tables remain allocated; a later bind can install the
    /// same VM into slot 1 again.
    pub(crate) fn restore_t8140_bootstrap_roots_after_processor_stop(&self) -> Result {
        if self.cfg != UatConfig::T8140 {
            return Err(EINVAL);
        }
        let low = self.kernel_lower_vm.ttb();
        let source_high = self.kernel_vm.ttb();
        let inner = self.inner.lock();
        inner.handoff().lock();
        let ttbs = inner.ttbs();
        ttbs[0].ttb0.store(low | TTBR_VALID, Ordering::Release);
        ttbs[0]
            .ttb1
            .store(source_high | TTBR_VALID, Ordering::Release);
        ttbs[1].ttb0.store(
            low | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT),
            Ordering::Release,
        );
        ttbs[1].ttb1.store(
            source_high | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT),
            Ordering::Release,
        );
        self.t8140_context_phase
            .store(T8140_CONTEXT_BOOTSTRAP_SOURCE_HIGH, Ordering::Release);
        inner.handoff().unlock();
        core::mem::drop(inner);

        fence(Ordering::SeqCst);
        mem::tlbi_asid(0);
        mem::tlbi_asid(1);
        mem::sync();
        Ok(())
    }

    /// Publish the compute kick's hardware contexts as aliases of the bound
    /// compute VM.
    ///
    /// The compute kick names GPU context 2 in registers `0x10201`/`0x10428`
    /// and in descriptor `+0x0c`; a second and later workload in the same
    /// session names context 3.  Until this call the T8140 path wrote only
    /// `ttbs[0]` and `ttbs[1]`, so those contexts had **neither** a lower nor
    /// an upper root and the first firmware-half fetch the front end makes on
    /// a kick (unit `0xd9`, gUPM) took a `Page Invalid` read fault with
    /// `vm_slot = 2`.
    ///
    /// m1n1's hardware-proven graph builder installs *both halves* of *each*
    /// context, each tagged with the context as its ASID:
    ///
    /// ```text
    /// CONTEXT = 2                                                     # :24
    /// root = space.uat.ttbr0_base                                     # :1053
    /// space.uat.ttbr1_base = backend.firmware_high_root               # :1062
    /// space.uat.set_l0(CONTEXT,     0, root,                 CONTEXT)     # :1067
    /// space.uat.set_l0(CONTEXT,     1, space.uat.ttbr1_base, CONTEXT)     # :1068
    /// space.uat.set_l0(CONTEXT + 1, 0, root,                 CONTEXT + 1) # :1083
    /// space.uat.set_l0(CONTEXT + 1, 1, space.uat.ttbr1_base, CONTEXT + 1) # :1084
    /// ```
    /// (`m1n1-g17p-linux/proxyclient/experiments/agx_g17p_native_add3.py`;
    /// the same lines are `:1072-1073`/`:1087-1089` in the `g17-upstream`
    /// checkout.)
    ///
    /// `set_l0(ctx, off, base, asid)` writes
    /// `TTBR { BADDR: base >> 1, ASID: asid, VALID: base != 0 }` at
    /// `gpu_region + ctx * 16 + off * 8` (`m1n1/hw/uat.py:303-307`) — exactly
    /// [`t8140_tagged_root`] — and `ttbr1_base` is left at
    /// `backend.firmware_high_root`, the *populated* firmware upper root,
    /// because `distinct_empty_high` is false on the entry path
    /// (`agx_g17p_native_add3.py:970, 1056-1062, 2964`).  That is the root
    /// which maps the firmware-half object the fault named, so these contexts
    /// get [`Uat::kernel_vm`]'s root here and not one of the empty high roots
    /// the opening leaves resident in contexts 0 and 1.  m1n1's shim spells
    /// the split out: *"A split native topology may deliberately install an
    /// empty upper root in the hardware context slot while retaining the
    /// populated firmware root off-table"* (`m1n1/agx/g17p_shim.py:753-760`).
    ///
    /// The compute VM stays *bound* at hardware context 1: the first-partial
    /// opening contract pins the render/descriptor/optional-identity context
    /// there and the root-phase machine reads `ttbs[0]`/`ttbs[1]` back
    /// verbatim, so rebinding would move several measured fields at once.
    /// Aliasing one page-table root into further hardware contexts is m1n1's
    /// own shape for the second workload, so nothing here is invented.
    ///
    /// Gated on `g17p_ctx2_root`, a bitmask over [`T8140_COMPUTE_CTXS`]; with
    /// the parameter at 0 this is a no-op and the driver keeps the previous
    /// (faulting) behaviour.
    pub(crate) fn install_t8140_compute_context_alias(&self, bind: &VmBind) -> Result {
        if self.cfg != UatConfig::T8140 {
            return Err(EINVAL);
        }
        let mask = crate::g17_config::CTX2_ROOT;
        if mask == 0 {
            dev_info!(
                self.dev.as_ref(),
                "MMU: compute context roots suppressed (g17p_ctx2_root=0)\n"
            );
            return Ok(());
        }
        // Contexts 2/3 are the compute kick's DECLARED contexts, and the
        // measured pairing publishes them as aliases of the opening context.
        // A binding is no longer pinned to context 1, but this path is: it is
        // the legacy synchronous compute submit, whose captured kick names
        // context 2 next to an opening at context 1. A caller reaching here
        // from a bind-band context would be a different submission shape and
        // must not silently reuse this one.
        if bind.slot() != 1 {
            return Err(EINVAL);
        }
        let root = bind.0.ttb();
        let high_root = self.kernel_vm.ttb();
        let phase = self.t8140_context_phase.load(Ordering::Acquire);
        for (bit, ctx) in T8140_COMPUTE_CTXS.into_iter().enumerate() {
            if mask & (1 << bit) == 0 {
                continue;
            }
            let low = t8140_tagged_root(root, ctx);
            let high = t8140_tagged_root(high_root, ctx);
            {
                let inner = self.inner.lock();
                inner.handoff().lock();
                if inner.handoff().current_slot() == Some(ctx as u32) {
                    inner.handoff().unlock();
                    core::mem::drop(inner);
                    pr_err!(
                        "Uat: compute context {} is currently in use by the ASC?\n",
                        ctx
                    );
                    return Err(EBUSY);
                }
                let ttbs = inner.ttbs();
                ttbs[ctx].ttb0.store(low, Ordering::Release);
                ttbs[ctx].ttb1.store(high, Ordering::Release);
                inner.handoff().unlock();
            }

            // m1n1 follows its set_l0 calls with flush_dirty()/
            // invalidate_cache() and an explicit
            // `dsb sy; tlbi aside1os, CONTEXT << 48; dsb sy`
            // (`agx_g17p_native_add3.py:1319-1321`). tlbi_asid does the same.
            // These ASIDs have never been invalidated on this machine because
            // they have never been live translation regimes.
            fence(Ordering::SeqCst);
            mem::tlbi_asid(ctx as u8);
            mem::sync();
            dev_info!(
                self.dev.as_ref(),
                "MMU: compute context {} roots low={:#x} high={:#x} (slot-1 alias, phase {}, vm {})\n",
                ctx,
                low,
                high,
                phase,
                bind.0.id
            );
        }
        // Name the publisher so a later `VmInner::drop` can tell "my aliases"
        // from "aliases some other VM published at the same reused root page".
        self.inner
            .t8140_alias_owner
            .store(bind.0.id, Ordering::Release);
        Ok(())
    }

    /// Install one app page-table root in an otherwise driver-reserved TTBAT
    /// context. Render command context IDs and MCache selectors have distinct
    /// native allocators, but both require this same root publication
    /// primitive.
    fn install_t8140_app_context_alias(
        &self,
        bind: &VmBind,
        context_id: usize,
    ) -> Result<(u64, u64)> {
        // The alias target is either this binding's own hardware context, one
        // of the two contexts a compute kick declares, or a dynamically
        // allocated application-GART context. It is no longer "context 1 or
        // above the reserved band", because a binding is no longer pinned to
        // context 1.
        if self.cfg != UatConfig::T8140
            || context_id >= UAT_NUM_CTX
            || (context_id != bind.slot() as usize
                && !T8140_COMPUTE_CTXS.contains(&context_id)
                && context_id < T8140_APP_CONTEXT_START)
        {
            return Err(EINVAL);
        }

        let low_root = bind.0.ttb();
        let high_root = if crate::g17_config::RENDER_POPULATED_HIGH_ROOT != 0 {
            self.kernel_vm.ttb()
        } else {
            self.t8140_empty_high_roots.as_ref().ok_or(EIO)?[1].ttb()
        };
        let low = t8140_tagged_root(low_root, context_id);
        let high = t8140_tagged_root(high_root, context_id);
        {
            let inner = self.inner.lock();
            inner.handoff().lock();
            if inner.handoff().current_slot() == Some(context_id as u32) {
                inner.handoff().unlock();
                return Err(EBUSY);
            }
            let ttbs = inner.ttbs();
            ttbs[context_id].ttb0.store(low, Ordering::Release);
            ttbs[context_id].ttb1.store(high, Ordering::Release);
            inner.handoff().unlock();
        }

        fence(Ordering::SeqCst);
        mem::tlbi_asid(context_id as u8);
        mem::sync();
        if T8140_COMPUTE_CTXS.contains(&context_id) {
            self.inner
                .t8140_alias_owner
                .store(bind.0.id, Ordering::Release);
        }
        Ok((low, high))
    }

    /// Allocate and root one native-style application-GART hardware context.
    ///
    /// IDs 0..4 are never candidates: the opening pair and fixed compute
    /// aliases already give those values architectural meaning on T8140.
    /// The qualified renderer still encodes selector1 and retains this
    /// duplicate root only for its app-context ownership. Reserve slot63 for
    /// that exclusive renderer so eager compute queue reservations cannot
    /// exhaust the root it needs. Compute execution uses independent5..62.
    pub(crate) fn allocate_t8140_app_context(
        &self,
        bind: &VmBind,
    ) -> Result<T8140AppContextLease> {
        if self.cfg != UatConfig::T8140 {
            return Err(EINVAL);
        }

        let context_id = loop {
            let used = self
                .inner
                .t8140_app_contexts
                .load(Ordering::Acquire);
            let available = !used & (1u64 << crate::g17_queue_limits::RENDER_APP_CONTEXT);
            if available == 0 {
                return Err(ENOSPC);
            }
            let context_id = available.trailing_zeros() as usize;
            let claimed = used | (1u64 << context_id);
            if self
                .inner
                .t8140_app_contexts
                .compare_exchange_weak(used, claimed, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break context_id;
            }
        };

        let (low, high) = match self.install_t8140_app_context_alias(bind, context_id) {
            Ok(roots) => roots,
            Err(error) => {
                self.inner
                    .t8140_app_contexts
                    .fetch_and(!(1u64 << context_id), Ordering::AcqRel);
                return Err(error);
            }
        };
        dev_info!(
            self.dev.as_ref(),
            "MMU: allocated T8140 app-GART context {} roots low={:#x} high={:#x} (vm {})\n",
            context_id,
            low,
            high,
            bind.0.id,
        );
        Ok(T8140AppContextLease {
            root: T8140ContextRootLease {
                inner: self.inner.clone(), context_id: context_id as u8, low, high,
                generation: self.inner.t8140_context_generations[context_id]
                    .fetch_add(1, Ordering::Relaxed).wrapping_add(1),
            },
            _bind: bind.clone(),
        })
    }

    /// Reserve a free context without consulting the slot-1 allocator. A
    /// context bit remains claimed until its root lease invalidates that ASID.
    /// A failed allocation or root publication cannot expose a half-owned ID.
    pub(crate) fn allocate_t8140_compute_execution_context(
        &self, vm: &Vm,
    ) -> Result<Arc<T8140ComputeExecutionContext>> {
        if self.cfg != UatConfig::T8140 { return Err(EINVAL); }
        {
            let owner = vm.inner.exec_lock(None, false)?;
            if owner.is_kernel || !core::ptr::eq(
                Arc::as_ptr(&owner.uat_inner), Arc::as_ptr(&self.inner),
            ) { return Err(EINVAL); }
        }
        let context_id = loop {
            let used = self.inner.t8140_app_contexts.load(Ordering::Acquire);
            let free = !used & crate::g17_queue_limits::COMPUTE_CONTEXT_MASK;
            if free == 0 { return Err(ENOSPC); }
            let index = free.trailing_zeros() as usize;
            if self.inner.t8140_app_contexts.compare_exchange_weak(
                used, used | (1u64 << index), Ordering::AcqRel, Ordering::Acquire,
            ).is_ok() { break index; }
        };
        let low = t8140_tagged_root(vm.ttb(), context_id);
        let high = t8140_tagged_root(self.kernel_vm.ttb(), context_id);
        // The context bit is exclusively claimed. Independent context IDs do
        // not share an epoch lock; outstanding jobs retain this immutable lease.
        let generation = self.inner.t8140_context_generations[context_id]
            .fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        // Allocate every host owner before publishing either TTBAT half.
        // Root-lease Drop handles allocation failure with an empty table.
        let context = Arc::new(T8140ComputeExecutionContext {
            root: T8140ContextRootLease {
                inner: self.inner.clone(), context_id: context_id as u8, low, high,
                generation,
            },
            vm: vm.clone(),
        }, GFP_KERNEL)?;
        {
            let inner = self.inner.lock();
            let ttbs = inner.ttbs();
            // This ID is exclusively claimed. Never overwrite unexpected
            // firmware/legacy ownership merely because the bitmap was clear.
            if ttbs[context_id].ttb0.load(Ordering::Acquire) != 0
                || ttbs[context_id].ttb1.load(Ordering::Acquire) != 0
            { return Err(EBUSY); }
            ttbs[context_id].ttb0.store(low, Ordering::Release);
            ttbs[context_id].ttb1.store(high, Ordering::Release);
        }
        fence(Ordering::SeqCst);
        mem::tlbi_asid(context_id as u8);
        mem::sync();
        Ok(context)
    }

    pub(crate) fn install_t8140_render_mcache_context_alias(
        &self,
        bind: &VmBind,
        selector: usize,
    ) -> Result {
        let (low, high) = self.install_t8140_app_context_alias(bind, selector)?;
        dev_info!(
            self.dev.as_ref(),
            "MMU: render MCache selector {} roots low={:#x} high={:#x} (slot-1 GART alias, vm {})\n",
            selector,
            low,
            high,
            bind.0.id,
        );
        Ok(())
    }

    /// Publish this binding's compute-context roots if they are not already
    /// exactly right, and prove it afterwards.
    ///
    /// A kick declares hardware context 2 or 3 (`T8140_COMPUTE_CTXS`).  If
    /// those entries do not name the submitting VM's root the GPU has no
    /// translation regime at all: the work is accepted and silently never
    /// runs, with no fault and no blamed queue, which is indistinguishable
    /// from "the shader did nothing".
    ///
    /// `install_t8140_compute_context_alias` only runs on the bind that
    /// creates a client's retained binding.  Every other submission on that
    /// binding -- every repeat, and every submission after another VM has been
    /// created or destroyed in between -- reaches the kick without anything
    /// re-checking those two entries.  This closes that window: it is cheap
    /// (two loads in the common case) and it is the only thing standing
    /// between a zeroed context and a silent no-op submission.
    pub(crate) fn ensure_t8140_compute_context_alias(&self, bind: &VmBind) -> Result {
        if self.cfg != UatConfig::T8140 {
            return Ok(());
        }
        if crate::g17_config::CTX2_ROOT == 0 {
            return Ok(());
        }
        if self.t8140_compute_context_alias_ready(bind) {
            return Ok(());
        }
        let root = bind.0.ttb();
        let before = self.t8140_compute_context_roots();
        dev_warn!(
            self.dev.as_ref(),
            "MMU: compute context roots stale before kick (vm {} root={:#x}): ctx2 ttb0={:#x} ttb1={:#x}, ctx3 ttb0={:#x} ttb1={:#x}; republishing\n",
            bind.0.id,
            root,
            before[2].0,
            before[2].1,
            before[3].0,
            before[3].1,
        );
        self.install_t8140_compute_context_alias(bind)?;
        if !self.t8140_compute_context_alias_ready(bind) {
            let after = self.t8140_compute_context_roots();
            dev_err!(
                self.dev.as_ref(),
                "MMU: compute context roots STILL wrong after republish (vm {} root={:#x}): ctx1 ttb0={:#x}, ctx2 ttb0={:#x} ttb1={:#x}, ctx3 ttb0={:#x} ttb1={:#x}; refusing the kick\n",
                bind.0.id,
                root,
                after[1].0,
                after[2].0,
                after[2].1,
                after[3].0,
                after[3].1,
            );
            return Err(EFAULT);
        }
        Ok(())
    }

    /// Snapshot the first four hardware context-table entries as
    /// `(ttb0, ttb1)` pairs, for logging.
    pub(crate) fn t8140_compute_context_roots(&self) -> [(u64, u64); 4] {
        let inner = self.inner.lock();
        let ttbs = inner.ttbs();
        core::array::from_fn(|slot| {
            (
                ttbs[slot].ttb0.load(Ordering::Acquire),
                ttbs[slot].ttb1.load(Ordering::Acquire),
            )
        })
    }

    /// Snapshot an arbitrary set of T8140 context-table entries.
    ///
    /// Concurrent render diagnosis needs to compare the opening context, the
    /// QoS-requested context, the command descriptor context and the VM bind
    /// context. Those are allocator results and cannot be represented by the
    /// fixed ctx0..ctx3 snapshot above.
    pub(crate) fn t8140_selected_context_roots(
        &self,
        contexts: [u8; 4],
    ) -> Result<[(u64, u64); 4]> {
        if self.cfg != UatConfig::T8140
            || contexts.iter().any(|context| usize::from(*context) >= UAT_NUM_CTX)
        {
            return Err(EINVAL);
        }
        let inner = self.inner.lock();
        let ttbs = inner.ttbs();
        Ok(core::array::from_fn(|index| {
            let slot = &ttbs[usize::from(contexts[index])];
            (
                slot.ttb0.load(Ordering::Acquire),
                slot.ttb1.load(Ordering::Acquire),
            )
        }))
    }

    /// Confirm the compute aliases are resident and name this binding.
    ///
    /// This also re-reads `ttbs[1]`: m1n1 warns that installing a context's
    /// root *after* its first mapping can leave the address space attached to
    /// an implicitly constructed root that is then lost. Our `Vm` owns its
    /// `UatPageTable` root outright and never derives it from the hardware
    /// context table, so that cannot happen here — but comparing all three
    /// entries against the same `ttb()` is the cheap proof of it.
    pub(crate) fn t8140_compute_context_alias_ready(&self, bind: &VmBind) -> bool {
        // Paired with `install_t8140_compute_context_alias`: this readiness
        // test reads `ttbs[1]` back verbatim, so it only describes a binding
        // that actually sits at the opening context.
        if self.cfg != UatConfig::T8140 || bind.slot() != 1 {
            return false;
        }
        let mask = crate::g17_config::CTX2_ROOT;
        if mask == 0 {
            return false;
        }
        let root = bind.0.ttb();
        let high_root = self.kernel_vm.ttb();
        let inner = self.inner.lock();
        let ttbs = inner.ttbs();
        if ttbs[1].ttb0.load(Ordering::Acquire) != t8140_tagged_root(root, 1) {
            return false;
        }
        for (bit, ctx) in T8140_COMPUTE_CTXS.into_iter().enumerate() {
            if mask & (1 << bit) == 0 {
                continue;
            }
            if ttbs[ctx].ttb0.load(Ordering::Acquire) != t8140_tagged_root(root, ctx)
                || ttbs[ctx].ttb1.load(Ordering::Acquire)
                    != t8140_tagged_root(high_root, ctx)
            {
                return false;
            }
        }
        true
    }

    /// Binds a `Vm` to a slot, preferring the last used one.
    ///
    /// On T8140 this binding is reserved for the driver-owned opening at
    /// context 1. After its retirement context 1 becomes the permanent global
    /// FList root; ordinary queues use application-GART root leases instead.
    ///
    /// Hardware context 2 is *not* a bind target. It is the context the
    /// compute kick declares (`T8140_COMPUTE_CTXS`), published as an alias of
    /// a binding's root by [`Uat::install_t8140_compute_context_alias`] rather
    /// than by the slot allocator -- exactly as m1n1 publishes `CONTEXT = 2`
    /// while keeping its own bootstrap context separate.
    pub(crate) fn bind(&self, vm: &Vm) -> Result<VmBind> {
        // M5 application slots require a private empty upper root and a
        // retained M5ExecutionContext. The legacy slot allocator must not
        // overwrite a slot claimed by that independent lease allocator.
        if self.cfg.has_m5_native_topology() {
            return Err(ENOTSUPP);
        }
        let mut binding = vm.binding.lock();

        if binding.binding.is_none() {
            assert_eq!(binding.active_users, 0);

            let limit = if module_parameters::robust_isolation.value() != 0 {
                // Escape hatch: one bindable context, the pre-pool behaviour.
                NonZeroUsize::new(1)
            } else if self.cfg == UatConfig::T8140 {
                NonZeroUsize::new(T8140_BIND_SLOTS)
            } else {
                None
            };
            self.slots.set_limit(limit);
            // A T8140 slot index names a hardware context, so which one a VM
            // lands on must not depend on allocation history. See
            // `SlotAllocatorInner::lowest_first`.
            self.slots.set_lowest_first(self.cfg == UatConfig::T8140);

            let vm_id = vm.id;
            let slot = match self.slots.get_inner(binding.bind_token, |data, guard| {
                data.record_owner(guard.slot(), vm_id);
                Ok(())
            }) {
                Ok(slot) => slot,
                Err(error) => {
                    self.log_bind_context_owners(vm_id, error);
                    return Err(error);
                }
            };
            let idx = if self.cfg == UatConfig::T8140 {
                match t8140_bind_context(slot.slot()) {
                    Some(idx) => idx,
                    None => {
                        self.log_bind_context_owners(vm_id, EBUSY);
                        return Err(EBUSY);
                    }
                }
            } else {
                (slot.slot() as usize) + UAT_USER_CTX_START
            };
            if slot.changed() {
                mod_pr_debug!("Vm Bind [{}]: bind_token={:?}\n", vm.id, slot.token(),);
                let ttb = binding.ttb | TTBR_VALID | (idx as u64) << TTBR_ASID_SHIFT;

                let uat_inner = self.inner.lock();

                let ttb1 = if self.cfg == UatConfig::T8140 {
                    let high_root = if crate::g17_config::RENDER_POPULATED_HIGH_ROOT
                        != 0
                    {
                        // Diagnostic only: native gives each app GART a distinct,
                        // initially-empty root1 and keeps the populated firmware
                        // root off-table. Exposing the Linux kernel high root here
                        // tests whether we accidentally left a fragment-consumed
                        // pointer in that domain; it is not a final isolation model.
                        self.kernel_vm.ttb()
                    } else {
                        self.t8140_empty_high_roots.as_ref().ok_or(EIO)?[1].ttb()
                    };
                    high_root | TTBR_VALID | (idx as u64) << TTBR_ASID_SHIFT
                } else if uat_inner.map_kernel_to_user {
                    uat_inner.kernel_ttb1 | TTBR_VALID | (idx as u64) << TTBR_ASID_SHIFT
                } else {
                    0
                };

                let ttbs = uat_inner.ttbs();
                uat_inner.handoff().lock();
                let t8140_phase = self.t8140_context_phase.load(Ordering::Acquire);
                if uat_inner.handoff().current_slot() == Some(idx as u32) {
                    pr_err!(
                        "Vm::bind to slot {}, but it is currently in use by the ASC?\n",
                        idx
                    );
                }
                // At the first-work boundary the populated firmware high root
                // leaves contexts 0 and 1. Compute contexts 2/3 retain that
                // root explicitly, while the opening pair becomes the two
                // measured empty-high roots.
                let changed_ctx0 = self.cfg == UatConfig::T8140
                    && idx == 1
                    && t8140_phase == T8140_CONTEXT_BOOTSTRAP_SOURCE_HIGH;
                if changed_ctx0 {
                    let empty = self.t8140_empty_high_roots.as_ref().ok_or(EIO)?;
                    ttbs[0]
                        .ttb1
                        .store(empty[0].ttb() | TTBR_VALID, Ordering::Release);
                }
                ttbs[idx].ttb0.store(ttb, Ordering::Release);
                ttbs[idx].ttb1.store(ttb1, Ordering::Release);
                if self.cfg == UatConfig::T8140 && idx == 1 {
                    self.t8140_context_phase
                        .store(T8140_CONTEXT_USER_OWNED, Ordering::Release);
                    if crate::g17_config::RENDER_POPULATED_HIGH_ROOT != 0 {
                        dev_warn!(
                            self.dev.as_ref(),
                            "MMU: DIAGNOSTIC render context 1 exposes populated high root {:#x}\n",
                            ttb1
                        );
                    }
                }
                uat_inner.handoff().unlock();
                core::mem::drop(uat_inner);

                // Make sure all TLB entries from the previous owner of this ASID are gone.
                fence(Ordering::SeqCst);
                if changed_ctx0 {
                    mem::tlbi_asid(0);
                }
                mem::tlbi_asid(idx as u8);
                mem::sync();
            }

            binding.bind_token = Some(slot.token());
            binding.bind_context = Some(idx as u32);
            binding.binding = Some(slot);
        }

        binding.active_users += 1;

        let slot = binding.bind_context.ok_or(EIO)?;
        mod_pr_debug!("MMU: slot {} active users {}\n", slot, binding.active_users);
        Ok(VmBind(vm.clone(), slot))
    }

    /// Report which UAT bind contexts are occupied and by which VM.
    ///
    /// Called on every failed bind. Without it the only evidence is
    /// slotalloc's "out of slots with a limit of N", which names neither the
    /// hardware contexts involved nor the clients holding them, and a bind
    /// failure is the one error a second GPU client sees first.
    fn log_bind_context_owners(&self, vm_id: u64, error: kernel::error::Error) {
        let busy = self.slots.busy_mask();
        let owners = self.slots.with_inner(|data| data.snapshot());
        let inner = self.inner.lock();
        let ttbs = inner.ttbs();
        // The slot -> context mapping only means anything on T8140; every
        // other configuration binds slot n at context n + 1 and has 63 of
        // them, so there is nothing useful to enumerate.
        let slots = if self.cfg == UatConfig::T8140 {
            T8140_BIND_SLOTS as u32
        } else {
            0
        };
        for slot in 0..slots {
            let Some(context) = t8140_bind_context(slot) else {
                continue;
            };
            let state = if busy & (1u64 << slot) != 0 { "BUSY" } else { "free" };
            let owner = owners.get(slot as usize).copied().unwrap_or(0);
            dev_err!(
                self.dev.as_ref(),
                "MMU: bind slot {} -> hw context {}: {}, owner vm {}, ttb0={:#x} ttb1={:#x}\n",
                slot,
                context,
                state,
                owner,
                ttbs[context].ttb0.load(Ordering::Acquire),
                ttbs[context].ttb1.load(Ordering::Acquire),
            );
        }
        core::mem::drop(inner);
        dev_err!(
            self.dev.as_ref(),
            "MMU: vm {} could not acquire a UAT bind context ({:?}); app-GART contexts in use {:#x}\n",
            vm_id,
            error,
            self.inner.t8140_app_contexts.load(Ordering::Acquire),
        );
    }

    /// Creates a new `Vm` linked to this UAT.
    pub(crate) fn new_vm(&self, id: u64, kernel_range: Range<u64>) -> Result<Vm> {
        Vm::new(
            &self.dev,
            self.inner.clone(),
            kernel_range,
            self.cfg,
            None,
            id,
            false,
        )
    }

    /// Unpublished, driver-private VM for the qualified G18 compute fixture.
    /// Its VA-zero allocation is required by the saved native user payload.
    /// This allocates page tables only; no context slot or TTB root is written.
    pub(crate) fn new_t8152_compute_fixture_vm(&self) -> Result<Vm> {
        if !self.is_t8152_geometry() { return Err(ENODEV); }
        Vm::new(&self.dev, self.inner.clone(),
            IOVA_USER_BASE..uat_geometry(self.cfg)?.user_va_top(),
            self.cfg, None, 0x8152_0001, true)
    }

    /// Creates the reference-counted inner data for a new `Uat` instance.
    #[inline(never)]
    fn make_inner(
        dev: &driver::AsahiDevice,
        cfg: UatConfig,
        handoff_mode: HandoffMode,
    ) -> Result<Arc<UatInner>> {
        let handoff_rgn = Self::map_region(dev.as_ref(), c_str!("handoff"), HANDOFF_SIZE, true)?;
        let ttbs_rgn = Self::map_region(dev.as_ref(), c_str!("ttbs"), SLOTS_SIZE, true)?;

        // SAFETY: The Handoff struct layout matches the firmware's view of memory at this address,
        // and the region is at least large enough per the size specified above.
        let handoff = unsafe { &(handoff_rgn.map.ptr() as *mut Handoff).as_ref().unwrap() };

        dev_info!(dev.as_ref(), "MMU: Initializing kernel page table\n");

        Arc::pin_init(
            try_pin_init!(UatInner {
                firmware_cache_stopped: AtomicBool::new(false),
                firmware_cache_flush_ready: AtomicBool::new(
                    handoff_mode == HandoffMode::FirmwareDekker
                ),
                t8140_alias_owner: AtomicU64::new(0),
                t8142_app_contexts: AtomicU64::new(0b111),
                t8140: cfg == UatConfig::T8140,
                // Contexts 0..4 have fixed architectural meaning, and the
                // bind band belongs to `Uat::bind`. Reserving the band here as
                // well means the two allocators cannot collide even if the
                // masks in `g17_queue_limits` are ever edited apart.
                t8140_app_contexts: AtomicU64::new(
                    T8140_APP_CONTEXT_RESERVED_MASK
                        | crate::g17_queue_limits::BIND_CONTEXT_MASK,
                ),
                t8140_context_generations: core::array::from_fn(|_| AtomicU8::new(0)),
                handoff_flush <- pin_init::pin_init_array_from_fn(|i| {
                    new_mutex!(HandoffFlush(&handoff.flush[i], handoff_mode), "handoff_flush")
                }),
                shared <- new_mutex!(
                    UatShared {
                        kernel_ttb1: 0,
                        map_kernel_to_user: false,
                        handoff_rgn,
                        ttbs_rgn,
                    },
                    "uat_shared"
                ),
            }),
            GFP_KERNEL,
        )
    }

    /// Creates a new `Uat` instance given the relevant hardware config, using
    /// the AGX2-proven firmware Dekker handoff protocol.
    #[inline(never)]
    pub(crate) fn new(
        dev: &driver::AsahiDevice,
        cfg: &'static hw::HwConfig,
        map_kernel_to_user: bool,
    ) -> Result<Self> {
        Self::new_with_handoff_mode(dev, cfg, map_kernel_to_user, HandoffMode::FirmwareDekker)
    }

    /// Create the T8140 UAT owner with the hardware-verified absent-handoff
    /// behavior. This constructor needs only the UAT geometry and chip ID; it
    /// does not fabricate the unrelated AGX2 firmware tuning fields in
    /// [`hw::HwConfig`].
    #[inline(never)]
    pub(crate) fn new_t8140(dev: &driver::AsahiDevice, map_kernel_to_user: bool) -> Result<Self> {
        let mut uat = Self::new_with_config(
            dev,
            UatConfig::T8140,
            map_kernel_to_user,
            HandoffMode::AbsentStandinT8140,
        )?;
        uat.install_t8140_firmware_crash_buffers(dev)?;
        uat.install_t8140_parameter_metrics(dev)?;
        Ok(uat)
    }

    /// M5-only initialization experiment. The caller validates the loaded
    /// firmware profile and retains this UAT across both CPU lifetimes.
    /// No Neo context allocator, metrics page or runtime manager is selected.
    pub(crate) fn new_t8142(dev: &driver::AsahiDevice) -> Result<Self> {
        let node = dev.as_ref().of_node().ok_or(EINVAL)?;
        let mut physical = [0u64; 2];
        for (index, (name, size)) in [
            (c_str!("gfx-data"), 0x134000),
            (c_str!("gfx1-data"), 0x130000),
        ].into_iter().enumerate() {
            let region = node.reserved_mem_region_to_resource_byname(name)?;
            if region.start() < 0x100_0000_0000 || region.size() != size
                || region.start() & UAT_PGMSK as u64 != 0
                || region.start().checked_add(region.size()).ok_or(EOVERFLOW)? > 0x104_0000_0000
            {
                return Err(EINVAL);
            }
            physical[index] = region.start() + 0xa0000;
        }
        if physical[0] + 0x8000 > physical[1] {
            return Err(EINVAL);
        }
        let mut uat = Self::new_with_config(dev, UatConfig::T8142, false,
            HandoffMode::NoPeerT8142)?;
        // Native EP1 requests name the already reserved firmware DATA plus
        // 0xa0000. Keep that same backing when replacing context 0; do not
        // allocate an unrelated buffer for a firmware-owned DVA.
        let map = |pa: u64| uat.kernel_lower_vm.map_io(
            pa & ((1u64 << 40) - 1), pa.try_into()?, 0x8000, PROT_FW_SHARED_RW);
        uat._t8142_firmware_crash_views = Some([map(physical[0])?, map(physical[1])?]);
        Ok(uat)
    }

    /// J873/T8152 (G18G, 26A428) UAT owner for the staged live-boot module.
    ///
    /// This validates the loader-filled boot-local reservations before any
    /// page-table state is touched:
    /// - `gfx-data` is exactly 0xbc000 and `gfx1-data` exactly 0xb4000,
    ///   i.e. the 26A428 __DATA segments;
    /// - `gfx-data-shared-ro` and `gfx1-data-shared-ro` are 0x8000 each.
    ///   They are validated so that Linux never reuses that memory, but they
    ///   are never mapped: the firmware sees them at VA +0x10c000/+0x108000
    ///   through its own roots;
    /// - all four regions are 16 KiB aligned, lie inside J873 DRAM
    ///   (0x100_0000_0000..0x106_0000_0000) and do not overlap.
    ///
    /// Firmware contexts 0..2 stay reserved (the shared UatInner lease mask
    /// starts at 0b111) and application binding is refused, as on M5. The
    /// handoff page runs in [`HandoffMode::NoPeerT8152`].
    ///
    /// `crash_views` gives the DATA-relative offsets of the GFX and GFX1
    /// RTKit crash views. M5 uses +0xa0000; the G18G offset is not yet known,
    /// so pass `None` until the EP1 crashlog request reports it. Each view is
    /// 0x8000 bytes (ADT coredump-copy-buffer) and is mapped read-write into
    /// context 0 at the M5 DVA rule (physical address truncated to 40 bits).
    #[allow(dead_code)] // Called by the G18 live-boot module.
    pub(crate) fn new_t8152(
        dev: &driver::AsahiDevice,
        crash_views: Option<[u64; 2]>,
    ) -> Result<Self> {
        let node = dev.as_ref().of_node().ok_or(EINVAL)?;
        let mut data = [0u64; 2];
        let mut regions = [(0u64, 0u64); 4];
        for (index, (name, size)) in Iterator::chain(
            T8152_FIRMWARE_DATA.into_iter(),
            T8152_FIRMWARE_SHARED_RO,
        )
        .enumerate()
        {
            let region = node.reserved_mem_region_to_resource_byname(name)?;
            let start = region.start();
            let end = start.checked_add(region.size()).ok_or(EOVERFLOW)?;
            if start < T8152_DRAM_BASE
                || region.size() != size
                || start & UAT_PGMSK as u64 != 0
                || end > T8152_DRAM_END
            {
                dev_err!(
                    dev.as_ref(),
                    "MMU: T8152 {} reservation {:#x}+{:#x} does not match the 26A428 layout ({:#x} bytes in J873 DRAM)\n",
                    name,
                    start,
                    region.size(),
                    size
                );
                return Err(EINVAL);
            }
            if regions[..index].iter().any(|&(s, e)| start < e && s < end) {
                dev_err!(dev.as_ref(), "MMU: T8152 {} overlaps another firmware reservation\n", name);
                return Err(EINVAL);
            }
            regions[index] = (start, end);
            if index < data.len() {
                data[index] = start;
            }
        }

        let mut uat = Self::new_with_config(dev, UatConfig::T8152, false, HandoffMode::NoPeerT8152)?;
        if let Some(offsets) = crash_views {
            let mut physical = [0u64; 2];
            for (index, offset) in offsets.into_iter().enumerate() {
                let (_, data_size) = T8152_FIRMWARE_DATA[index];
                if offset & UAT_PGMSK as u64 != 0
                    || offset.checked_add(T8152_CRASH_VIEW_SIZE).ok_or(EOVERFLOW)? > data_size
                {
                    dev_err!(
                        dev.as_ref(),
                        "MMU: T8152 crash view offset {:#x} is outside role {} __DATA\n",
                        offset,
                        index
                    );
                    return Err(EINVAL);
                }
                physical[index] = data[index] + offset;
            }
            let map = |pa: u64| {
                uat.kernel_lower_vm.map_io(
                    pa & ((1u64 << 40) - 1),
                    pa.try_into()?,
                    T8152_CRASH_VIEW_SIZE as usize,
                    PROT_FW_SHARED_RW,
                )
            };
            uat._t8142_firmware_crash_views = Some([map(physical[0])?, map(physical[1])?]);
        }
        Ok(uat)
    }

    /// Back the two nonzero EP1 crash-buffer requests before either firmware
    /// processor starts. m1n1 treats these DVAs as preallocated context-0
    /// mappings and only echoes the request; Linux must provide the same
    /// mapping rather than assuming firmware creates it.
    fn install_t8140_firmware_crash_buffers(&mut self, dev: &driver::AsahiDevice) -> Result {
        if self.cfg != UatConfig::T8140 || self._t8140_firmware_crash_buffers.is_some() {
            dev_err!(
                dev.as_ref(),
                "MMU: firmware crash-buffer setup has invalid T8140 state\n"
            );
            return Err(EINVAL);
        }
        let map = |name: &str,
                   range: &Range<u64>,
                   sentinel: u8|
         -> Result<(KernelMapping, ARef<gem::Object>)> {
            let size_u64 = range.end.checked_sub(range.start).ok_or(EINVAL)?;
            let size: usize = size_u64.try_into()?;
            let mut object = match gem::new_kernel_object_wc(dev, size) {
                Ok(object) => object,
                Err(err) => {
                    dev_err!(
                        dev.as_ref(),
                        "MMU: {} crash-buffer allocation at {:#x}:{:#x} failed ({:?})\n",
                        name,
                        range.start,
                        size,
                        err
                    );
                    return Err(err);
                }
            };
            let mut vmap = match object.vmap() {
                Ok(vmap) => vmap,
                Err(err) => {
                    dev_err!(
                        dev.as_ref(),
                        "MMU: {} crash-buffer CPU map at {:#x}:{:#x} failed ({:?})\n",
                        name,
                        range.start,
                        size,
                        err
                    );
                    return Err(err);
                }
            };
            vmap.memset(sentinel as i32);
            core::mem::drop(vmap);
            let retained_gem = object.gem.clone();
            let mapping = match object.map_at(
                &self.kernel_lower_vm,
                range.start,
                PROT_FW_SHARED_RW,
                false,
            ) {
                Ok(mapping) => mapping,
                Err(err) => {
                    dev_err!(
                        dev.as_ref(),
                        "MMU: {} crash-buffer context-0 map at {:#x}:{:#x} failed ({:?})\n",
                        name,
                        range.start,
                        size,
                        err
                    );
                    return Err(err);
                }
            };
            if mapping.iova_range() != *range {
                dev_err!(
                    dev.as_ref(),
                    "MMU: {} crash-buffer map returned {:#x?}, expected {:#x?}\n",
                    name,
                    mapping.iova_range(),
                    range
                );
                return Err(EFAULT);
            }
            // `covers_range` checks GPU permissions. These buffers are
            // deliberately firmware-only, so require complete leaf coverage
            // without requesting GPU read/write access.
            if !self
                .kernel_lower_vm
                .covers_range(range.start, size_u64, false, false)
            {
                dev_err!(
                    dev.as_ref(),
                    "MMU: {} crash-buffer context-0 leaves missing at {:#x}:{:#x}\n",
                    name,
                    range.start,
                    size
                );
                return Err(EFAULT);
            }
            dev_info!(
                dev.as_ref(),
                "MMU: mapped {} firmware crash buffer in context 0 at {:#x}:{:#x}\n",
                name,
                range.start,
                size
            );
            Ok((mapping, retained_gem))
        };
        let (gfx, gfx_gem) = map(
            "GFX",
            &T8140_FIRMWARE_CRASH_BUFFERS[0],
            T8140_FIRMWARE_CRASH_SENTINELS[0],
        )?;
        let (gfx1, gfx1_gem) = map(
            "GFX1",
            &T8140_FIRMWARE_CRASH_BUFFERS[1],
            T8140_FIRMWARE_CRASH_SENTINELS[1],
        )?;
        self._t8140_firmware_crash_buffers = Some([gfx, gfx1]);
        self.t8140_firmware_crash_gems = Some([gfx_gem, gfx1_gem]);
        Ok(())
    }

    /// Install the global PM page-metrics primary alias before either ASC
    /// starts. Firmware B1 later dereferences the primary VA directly at
    /// `0xbba0`; adding a TTBR0 top-level entry after launch leaves the
    /// firmware CPU on its boot snapshot and produces FSC level 0.
    fn install_t8140_parameter_metrics(&mut self, dev: &driver::AsahiDevice) -> Result {
        if self.cfg != UatConfig::T8140
            || self._t8140_parameter_metrics_low.is_some()
            || self.t8140_parameter_metrics_gem.is_some()
        {
            return Err(EINVAL);
        }
        let mut object = gem::new_kernel_object_wc(dev, T8140_PARAMETER_METRICS_SIZE)?;
        object.vmap()?.memset(0);
        let retained_gem = object.gem.clone();
        let low = object.map_at(
            &self.kernel_lower_vm,
            T8140_PARAMETER_METRICS_LOW_VA,
            PROT_GPU_FW_SHARED_RW,
            false,
        )?;
        if low.iova() != T8140_PARAMETER_METRICS_LOW_VA
            || low.size() != T8140_PARAMETER_METRICS_SIZE
            || !self.kernel_lower_vm.covers_range(
                low.iova(),
                low.size() as u64,
                true,
                true,
            )
        {
            return Err(EFAULT);
        }
        dev_info!(
            dev.as_ref(),
            "MMU: preinstalled T8140 PM page metrics low={:#x} size={:#x}\n",
            low.iova(),
            T8140_PARAMETER_METRICS_SIZE
        );
        self._t8140_parameter_metrics_low = Some(low);
        self.t8140_parameter_metrics_gem = Some(retained_gem);
        Ok(())
    }

    /// Creates a new `Uat` instance with an explicit handoff mode.
    ///
    /// The mode is evidence-gated: `FirmwareDekker` on a 42-bit part remains
    /// fail-closed behind [`AGX3_FIRMWARE_HANDOFF_VALIDATED`] (the PPL layout
    /// needs a live firmware ACK that no AGX3 boot has produced), and
    /// `AbsentStandinT8140` is accepted only on T8140 itself, where "the
    /// firmware never publishes its handoff half" is a live-hardware fact.
    #[inline(never)]
    pub(crate) fn new_with_handoff_mode(
        dev: &driver::AsahiDevice,
        cfg: &'static hw::HwConfig,
        map_kernel_to_user: bool,
        handoff_mode: HandoffMode,
    ) -> Result<Self> {
        Self::new_with_config(
            dev,
            UatConfig::from_hw(cfg),
            map_kernel_to_user,
            handoff_mode,
        )
    }

    #[inline(never)]
    fn new_with_config(
        dev: &driver::AsahiDevice,
        cfg: UatConfig,
        map_kernel_to_user: bool,
        handoff_mode: HandoffMode,
    ) -> Result<Self> {
        dev_info!(dev.as_ref(), "MMU: Initializing...\n");

        // Validate the geometry first: 39- and 42-bit roots are the two
        // recovered layouts, and the page-table walk, kernel-half VA window,
        // and leaf PTE policy are implemented for both (the AGX3 leaf PTE
        // encoder is byte-identical to AGX2). What must remain fail-closed
        // per mode is the firmware-shared state this constructor touches
        // next: the Handoff/FlushInfo protocol is per-firmware-build ABI
        // observed on AGX2 (Dekker) and, separately, observed *absent* on
        // T8140.
        let kernel_range = iova_kern_range(cfg)?;
        match handoff_mode {
            HandoffMode::FirmwareDekker => {
                if cfg.ias != 39 && !AGX3_FIRMWARE_HANDOFF_VALIDATED {
                    dev_err!(
                        dev.as_ref(),
                        "MMU: {}-bit UAT geometry is implemented, but the AGX3 firmware Handoff ABI and kernel-VA map are unvalidated\n",
                        cfg.ias
                    );
                    return Err(ENODEV);
                }
            }
            HandoffMode::AbsentStandinT8140 => {
                // Hardware-verified on T8140 only. Never assume any other
                // part ignores the handoff region.
                if cfg.chip_id != 0x8140 || cfg.ias != 42 {
                    dev_err!(
                        dev.as_ref(),
                        "MMU: absent-handoff stand-in is T8140-only evidence (chip {:#x}, {}-bit)\n",
                        cfg.chip_id,
                        cfg.ias
                    );
                    return Err(ENODEV);
                }
            }
            HandoffMode::NoPeerT8142 => {
                if cfg != UatConfig::T8142 {
                    return Err(ENODEV);
                }
            }
            HandoffMode::NoPeerT8152 => {
                if cfg != UatConfig::T8152 {
                    return Err(ENODEV);
                }
            }
        }

        let inner = Self::make_inner(dev, cfg, handoff_mode)?;

        let of_node = dev.as_ref().of_node().ok_or(EINVAL)?;
        let res = of_node.reserved_mem_region_to_resource_byname(c_str!("pagetables"))?;
        let ttb1 = res.start();
        let ttb1size: usize = res.size().try_into()?;

        let required_pagetables_size = if cfg.has_g17_private_roots() {
            T8140_SECONDARY_SHARED_ROOT_DELTA as usize + UAT_PGSZ
        } else {
            PAGETABLES_SIZE
        };
        if ttb1size < required_pagetables_size {
            dev_err!(dev.as_ref(), "MMU: Pagetables region is too small\n");
            return Err(ENOMEM);
        }

        dev_info!(dev.as_ref(), "MMU: Creating kernel page tables\n");
        let kernel_lower_vm = Vm::new(
            dev,
            inner.clone(),
            lower_vm_start(cfg, true)..uat_geometry(cfg)?.user_va_top(),
            cfg,
            None,
            1,
            true,
        )?;
        let kernel_vm = Vm::new(dev, inner.clone(), kernel_range, cfg, Some(ttb1), 0, false)?;

        dev_info!(dev.as_ref(), "MMU: Kernel page tables created\n");

        let ttb0 = kernel_lower_vm.ttb();
        let t8140_empty_high_roots = if cfg == UatConfig::T8140 {
            Some([
                Vm::new(
                    dev,
                    inner.clone(),
                    IOVA_USER_BASE..uat_geometry(cfg)?.user_va_top(),
                    cfg,
                    None,
                    0x8140_1000,
                    false,
                )?,
                Vm::new(
                    dev,
                    inner.clone(),
                    IOVA_USER_BASE..uat_geometry(cfg)?.user_va_top(),
                    cfg,
                    None,
                    0x8140_1001,
                    false,
                )?,
            ])
        } else {
            None
        };
        let t8140_render_global_vm = if cfg == UatConfig::T8140 {
            Some(Vm::new(
                dev,
                inner.clone(),
                IOVA_USER_BASE..uat_geometry(cfg)?.user_va_top(),
                cfg,
                None,
                0x8140_1002,
                false,
            )?)
        } else {
            None
        };

        let uat = Self {
            dev: dev.into(),
            cfg,
            kernel_vm,
            kernel_lower_vm,
            t8140_render_global_vm,
            _t8140_firmware_crash_buffers: None,
            _t8142_firmware_crash_views: None,
            t8140_firmware_crash_gems: None,
            _t8140_parameter_metrics_low: None,
            t8140_parameter_metrics_gem: None,
            t8140_secondary_ttb1: if cfg.has_g17_private_roots() {
                Some(ttb1 + T8140_SECONDARY_SHARED_ROOT_DELTA)
            } else {
                None
            },
            t8140_empty_high_roots,
            t8140_context_phase: AtomicU8::new(T8140_CONTEXT_BOOTSTRAP_SOURCE_HIGH),
            inner,
            slots: slotalloc::SlotAllocator::new(
                UAT_USER_CTX as u32,
                SlotOwners::new(),
                |_inner, _slot| Some(SlotInner()),
                c_str!("Uat::SlotAllocator"),
                static_lock_class!(),
                static_lock_class!(),
            )?,
        };

        let mut inner = uat.inner.lock();

        inner.map_kernel_to_user = map_kernel_to_user;
        inner.kernel_ttb1 = ttb1;

        inner.handoff().init(handoff_mode)?;

        dev_info!(dev.as_ref(), "MMU: Initializing TTBs\n");

        inner.handoff().lock();

        let ttbs = inner.ttbs();

        ttbs[0].ttb0.store(ttb0 | TTBR_VALID, Ordering::SeqCst);
        ttbs[0].ttb1.store(ttb1 | TTBR_VALID, Ordering::SeqCst);

        for ctx in &ttbs[1..] {
            ctx.ttb0.store(0, Ordering::Relaxed);
            ctx.ttb1.store(0, Ordering::Relaxed);
        }
        if cfg == UatConfig::T8140 {
            ttbs[1].ttb0.store(
                ttb0 | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT),
                Ordering::SeqCst,
            );
            ttbs[1].ttb1.store(
                ttb1 | TTBR_VALID | (1u64 << TTBR_ASID_SHIFT),
                Ordering::SeqCst,
            );
        }

        inner.handoff().unlock();

        core::mem::drop(inner);

        dev_info!(dev.as_ref(), "MMU: initialized\n");

        Ok(uat)
    }
}

impl Drop for Uat {
    fn drop(&mut self) {
        // Make sure we flush the TLBs
        fence(Ordering::SeqCst);
        mem::tlbi_all();
        mem::sync();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_mask_follows_every_published_copy_of_one_vm_root() {
        let root = 0x101_abc0_0000;
        let other = 0x101_def0_0000;
        let ttbs = core::array::from_fn(|_| SlotTTBS {
            ttb0: AtomicU64::new(0),
            ttb1: AtomicU64::new(0),
        });

        for context in [1usize, 5, 17, 62] {
            ttbs[context]
                .ttb0
                .store(t8140_tagged_root(root, context), Ordering::Relaxed);
        }
        ttbs[9]
            .ttb0
            .store(t8140_tagged_root(other, 9), Ordering::Relaxed);
        /* A root tagged for the wrong ASID is not owned by this context. */
        ttbs[23]
            .ttb0
            .store(t8140_tagged_root(root, 22), Ordering::Relaxed);

        assert_eq!(
            context_mask_for_root(root, &ttbs),
            (1u64 << 1) | (1u64 << 5) | (1u64 << 17) | (1u64 << 62)
        );
        assert_eq!(context_mask_for_root(other, &ttbs), 1u64 << 9);
    }

    #[test]
    fn g17p_fixed_pages_and_cl_arena_overlap_the_driver_ledger() {
        let protected = [
            0x10_0019_0000..0x10_0019_4000,
            0x70_0183_8000..0x70_0183_c000,
            0x70_0184_0000..0x70_0184_4000,
            0x70_0000_0000..0x71_0000_0000,
        ];
        for range in protected.clone() {
            assert!(any_range_overlaps(range, protected.clone().into_iter()));
        }
        assert!(!any_range_overlaps(
            0x20_0000_0000..0x20_0000_4000,
            protected.into_iter(),
        ));
    }

    #[test]
    fn g17p_root_lifecycle_preserves_firmware_entries_and_mirrors_host_entries() {
        let mut primary = [0u64; 8];
        primary[..5].copy_from_slice(&[0x101, 0x201, 0x303, 0x403, 0x503]);
        clear_stale_host_root_entries(&mut primary);
        assert_eq!(primary, [0x101, 0x201, 0x303, 0, 0, 0, 0, 0]);

        primary[3] = 0x433;
        primary[5] = 0x533;
        let mut secondary = [0x111, 0x211, 0x999, 0, 0, 0x888, 0, 0];
        assert_eq!(mirror_host_root_entries(&primary, &mut secondary), 3);
        assert_eq!(secondary, [0x111, 0x211, 0x303, 0x433, 0, 0x533, 0, 0]);
    }

    #[test]
    fn g17p_opening_uses_render_roots_in_both_slots_then_splits() {
        let retained_low = 0x1000_1000_0000;
        let render_low = 0x1000_2000_0000;
        let source_high = 0x1000_3000_0000;

        let opening = t8140_opening_render_layout(render_low, source_high);
        assert_eq!(
            opening.low,
            [
                render_low | TTBR_VALID,
                render_low | TTBR_VALID | (1 << TTBR_ASID_SHIFT),
            ]
        );
        assert_eq!(
            opening.high,
            [
                source_high | TTBR_VALID,
                source_high | TTBR_VALID | (1 << TTBR_ASID_SHIFT),
            ]
        );

        let split = t8140_split_render_layout(retained_low, render_low, [source_high; 2]);
        assert_eq!(split.low[0], retained_low | TTBR_VALID);
        assert_eq!(
            split.low[1],
            render_low | TTBR_VALID | (1 << TTBR_ASID_SHIFT)
        );
        assert_eq!(split.high[0], source_high | TTBR_VALID);
        assert_eq!(
            split.high[1],
            source_high | TTBR_VALID | (1 << TTBR_ASID_SHIFT)
        );
        assert_ne!(opening, split);
    }

    #[test]
    fn t8140_client_null_view_covers_the_faulting_gupm_writes() {
        let canonical = T8140_NATIVE_CONTEXT_VIEWS[0];
        let client = T8140_CLIENT_NULL_VIEW;

        // Same resource, same offset, same span as native's canonical view 0:
        // only the regime it is presented in differs.
        assert_eq!(client.source_address, canonical.source_address);
        assert_eq!(client.context_address, canonical.context_address);
        assert_eq!(client.size, canonical.size);
        assert_eq!(T8140_CLIENT_NULL_VIEW_START, client.context_address);
        assert_eq!(T8140_CLIENT_NULL_VIEW_SIZE, client.size);

        // Hardware reported these four as unmapped *writes* (read=0) in the
        // client's own vm-slot, so the leaf has to be GPU-writable and the
        // window has to contain all four.
        assert!(client.requires_source_write);
        assert!(client.context_prot.allows_gpu(true, true));
        let window = T8140_CLIENT_NULL_VIEW_START
            ..T8140_CLIENT_NULL_VIEW_START + T8140_CLIENT_NULL_VIEW_SIZE as u64;
        for address in [0x1e00u64, 0x1e80, 0x1f00, 0x1f80] {
            assert!(window.contains(&address));
        }

        // It begins below the published user base, so no user bind could ever
        // reach the first view; the reservation only protects the tail that
        // does overlap the user window.
        assert_eq!(window.start, 0);
        assert!(window.start < IOVA_USER_BASE);
        assert!(window.end > IOVA_USER_BASE);
    }

    #[test]
    fn t8140_job_root_has_exactly_one_driver_owned_secure_gart_view() {
        let null_end = T8140_CLIENT_NULL_VIEW.context_address
            + T8140_CLIENT_NULL_VIEW.size as u64;
        assert_eq!(T8140_NATIVE_CONTEXT_VIEWS[0].context_address, 0);
        assert_eq!(T8140_NATIVE_CONTEXT_VIEWS[0].size, T8140_CLIENT_NULL_VIEW.size);
        for view in T8140_NATIVE_CONTEXT_VIEWS.iter().skip(1) {
            // These ranges are explicitly supplied by Mesa's GPUVM binds and
            // must only be validated, never acquired by a KernelMapping.
            assert!(null_end <= view.context_address);
            assert!(view.context_address >= IOVA_USER_BASE);
        }
    }

    #[test]
    fn only_t8140_internal_lower_vm_contains_the_null_leaf() {
        assert_eq!(lower_vm_start(UatConfig::T8140, true), 0);
        assert_eq!(lower_vm_start(UatConfig::T8140, false), IOVA_USER_BASE);
        let legacy = UatConfig {
            chip_id: 0x8103,
            ias: 39,
            oas: 42,
        };
        assert_eq!(lower_vm_start(legacy, true), IOVA_USER_BASE);
    }

    #[test]
    fn t8140_native_context_views_match_secure_gart_low32_fanout() {
        let canonical_bo = 0x0010_0000_0000..0x0010_0008_0000;
        let mut previous_context_end = 0;

        for view in T8140_NATIVE_CONTEXT_VIEWS {
            assert_eq!(view.source_address & 0xffff_ffff, view.context_address);
            assert_eq!(view.source_address & UAT_PGMSK as u64, 0);
            assert_eq!(view.context_address & UAT_PGMSK as u64, 0);
            assert_eq!(view.size & UAT_PGMSK, 0);
            assert!(canonical_bo.is_superset(
                view.source_address..view.source_address + view.size as u64
            ));
            assert!(previous_context_end <= view.context_address);
            previous_context_end = view.context_address + view.size as u64;
        }

        assert_eq!(T8140_NATIVE_CONTEXT_VIEWS[0].context_address, 0);
        assert!(!T8140_NATIVE_CONTEXT_VIEWS[4].requires_source_write);
        assert!(T8140_NATIVE_CONTEXT_VIEWS[5].requires_source_write);
        assert!(T8140_NATIVE_CONTEXT_VIEWS[6].requires_source_write);
    }

    #[test]
    fn firmware_ttb_root_decoder_strips_asid_and_control_bits() {
        let cfg = UatConfig::T8140;
        let root = 0x101_abc0_0000;
        let raw = root | TTBR_VALID | (0x2au64 << TTBR_ASID_SHIFT);
        assert_eq!(decode_firmware_ttb_root(raw, cfg), Some(root));
        assert_eq!(decode_firmware_ttb_root(raw & !TTBR_VALID, cfg), None);
        assert_eq!(decode_firmware_ttb_root(TTBR_VALID, cfg), None);
    }

    #[test]
    fn t8140_crash_buffers_match_the_two_rtkit_requests() {
        assert_eq!(T8140_FIRMWARE_CRASH_BUFFERS[0], 0x01e1_c000..0x01e2_4000);
        assert_eq!(T8140_FIRMWARE_CRASH_BUFFERS[1], 0x01f5_0000..0x01f5_8000);
        for range in T8140_FIRMWARE_CRASH_BUFFERS {
            assert_eq!(range.len(), 0x8000);
            assert_eq!((range.start | range.end) & UAT_PGMSK as u64, 0);
        }
        assert!(T8140_FIRMWARE_CRASH_BUFFERS[0].end <= T8140_FIRMWARE_CRASH_BUFFERS[1].start);
        assert!(PROT_FW_SHARED_RW.allows_gpu(false, false));
        assert!(!PROT_FW_SHARED_RW.allows_gpu(false, true));
        assert_ne!(T8140_FIRMWARE_CRASH_SENTINELS[0], 0);
        assert_ne!(T8140_FIRMWARE_CRASH_SENTINELS[1], 0);
        assert_ne!(
            T8140_FIRMWARE_CRASH_SENTINELS[0],
            T8140_FIRMWARE_CRASH_SENTINELS[1]
        );
    }
}
