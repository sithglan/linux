// SPDX-License-Identifier: GPL-2.0-only OR MIT

#![cfg_attr(not(test), allow(dead_code))]

//! T8140/G17P manager construction boundary.
//!
//! This path needs only target identity, init-data geometry, and the retained
//! dual-role resource owner. It does not depend on the AGX2 `HwConfig` and it
//! performs no publication during construction. Explicit queue methods own the
//! resource-0 SKSM configuration and kick-store sequence.

#[cfg(not(test))]
use kernel::prelude::*;
#[cfg(not(test))]
use kernel::sync::Arc;
#[cfg(not(test))]
use kernel::time::{delay::fsleep, Delta};
#[cfg(all(not(test), CONFIG_DEV_COREDUMP))]
use crate::g17_trace_capture::{self as trace, TraceArchive};
#[cfg(all(not(test), CONFIG_DEV_COREDUMP))]
use kernel::time::{ClockSource, Monotonic};

#[cfg(not(test))]
use core::sync::atomic::{fence, AtomicBool, AtomicU32, Ordering};

fn g17p_mixed_resource_sample(mode: u32, ordinal: u32, fault: bool) -> bool {
    match mode {
        1 => fault,
        2 => fault || ordinal <= 1 || ordinal % 16 == 0,
        _ => false,
    }
}

#[cfg(not(test))]
pub(crate) const G17P_SUBMIT_PHASE_IDLE: u32 = 0;
#[cfg(not(test))]
pub(crate) const G17P_SUBMIT_PHASE_B2: u32 = 1;
#[cfg(not(test))]
pub(crate) const G17P_SUBMIT_PHASE_TAG15: u32 = 2;
#[cfg(not(test))]
pub(crate) const G17P_SUBMIT_PHASE_QID: u32 = 3;
#[cfg(not(test))]
pub(crate) const G17P_SUBMIT_PHASE_SKSM_ENTRY: u32 = 4;
#[cfg(not(test))]
pub(crate) const G17P_SUBMIT_PHASE_TAG16: u32 = 5;
#[cfg(not(test))]
pub(crate) const G17P_SUBMIT_PHASE_ADD_KICKS: u32 = 6;
#[cfg(not(test))]
pub(crate) const G17P_SUBMIT_PHASE_CL2: u32 = 7;

#[cfg(not(test))]
use crate::{
    driver::AsahiDevice, g17_adt_j700, g17_completion, g17_compute, g17_initdata, g17_lifecycle,
    g17_render, g17_resources, g17_submission, g17_uapi, hw::agx3, identity, mmu,
    module_parameters, pgtable, regs,
};
#[cfg(test)]
use super as g17_submission;
#[cfg(test)]
#[path = "g17_initdata.rs"]
mod g17_initdata;
#[cfg(test)]
#[path = "identity.rs"]
mod identity;

use g17_initdata::InstanceRole;
use identity::{FirmwareRoleTopology, GpuGen, GpuHalGeneration, GpuVariant, SubmissionTransport};

static G17P_STATE_DUMPED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// One-shot guard for the hardware-data dump: it must fire on the first
/// render checkpoint whatever the label, since gating it on a specific label
/// simply never matched.
static G17P_HW_DATA_DUMPED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

// Kext RE: AsyncGrowEngine::updateSubmitInfo copies AGXUSCPrivMemFList +0xb0
// into opcode-0x20 +0x0c, and the 3D submit path copies that same field into
// tag-15 +0x3e. Valid cold native opcode-0x20 and qid0/qid1 tag-15 records
// both publish generation zero; retained generations advance from there.
const G17P_BOOTSTRAP_UMA_SEQUENCE: u64 =
    g17_submission::g17p_render_flist_generation(0) as u64;
const G17P_BOOTSTRAP_UMA_HARDWARE_BUFFER_ID: u32 =
    g17_initdata::PARTIAL_OPENING_FREELIST_HARDWARE_BUFFER_ID;
const G17P_BOOTSTRAP_UMA_STAMP: i32 = 1;
// B1's type-13 completion echoes the submitted USC FList end address and the
// record's fixed +0x2c count. Keep the retained owner matched to those wire
// values rather than the adjacent operand-buffer cookie used by the older
// opening graph.
const G17P_BOOTSTRAP_UMA_COOKIE: u64 = g17_submission::G17P_USC_FREELIST_LOW_END;
const G17P_BOOTSTRAP_UMA_COUNT: u32 =
    g17_submission::G17P_USC_FREELIST_GROW_DESCRIPTOR_BYTES;
// Direct 2026-09-01 entry-ring captures of ordinary Metal TA and 3D kicks
// both carry this exact value at KSM entry +0x178.  It is selected by the
// descriptor wrapper immediately before encodeKickEntry; it is not the host
// physical-counter sample used by retained compute submissions.
const G17P_NATIVE_RENDER_KICK_AUXILIARY: u64 = 0x003f_ffff_ffff_ffff;

// Honeykrisp splits its T8140 main allocation heap into read/write and
// read-only sparse-shadow halves at bit 40. A PDM fault in the upper half is
// ambiguous unless recovery also checks the corresponding lower allocation.
const G17P_HONEYKRISP_SPARSE_RO_BIT: u64 = 1 << 40;

const fn g17p_fault_sparse_ro_lower(address: u64) -> Option<u64> {
    if address & G17P_HONEYKRISP_SPARSE_RO_BIT == 0 {
        None
    } else {
        Some(address & !G17P_HONEYKRISP_SPARSE_RO_BIT)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum G17PNativeRenderPublishPhase {
    FragmentVisible,
    TilingPrefix,
    TilingSksm,
    TilingKick,
    TilingOuter,
    PoolTransition,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct G17PNativeRenderPublishOrder {
    phase: G17PNativeRenderPublishPhase,
}

impl G17PNativeRenderPublishOrder {
    const fn after_fragment_doorbell() -> Self {
        Self {
            phase: G17PNativeRenderPublishPhase::FragmentVisible,
        }
    }

    fn advance(
        &mut self,
        next: G17PNativeRenderPublishPhase,
    ) -> core::result::Result<(), ()> {
        let expected = match self.phase {
            G17PNativeRenderPublishPhase::FragmentVisible => {
                G17PNativeRenderPublishPhase::TilingPrefix
            }
            G17PNativeRenderPublishPhase::TilingPrefix => {
                G17PNativeRenderPublishPhase::TilingSksm
            }
            G17PNativeRenderPublishPhase::TilingSksm => {
                G17PNativeRenderPublishPhase::TilingKick
            }
            G17PNativeRenderPublishPhase::TilingKick => {
                G17PNativeRenderPublishPhase::TilingOuter
            }
            G17PNativeRenderPublishPhase::TilingOuter => {
                G17PNativeRenderPublishPhase::PoolTransition
            }
            G17PNativeRenderPublishPhase::PoolTransition => return Err(()),
        };
        if next != expected {
            return Err(());
        }
        self.phase = next;
        Ok(())
    }
}

fn g17p_native_fragment_only_visible(
    previous_tiling_inner: u32,
    tiling_inner: u32,
    fragment_inner: u32,
    published_prefix: u32,
    fragment_outer: u32,
    fragment_outer_next: u32,
    tiling_outer: u32,
    tiling_outer_slot: u8,
) -> bool {
    tiling_inner == previous_tiling_inner
        && fragment_inner == published_prefix
        && fragment_outer == fragment_outer_next
        && tiling_outer == u32::from(tiling_outer_slot)
}

fn g17p_native_tiling_fully_visible(
    tiling_inner: u32,
    published_prefix: u32,
    tiling_outer: u32,
    tiling_outer_next: u32,
) -> bool {
    tiling_inner == published_prefix && tiling_outer == tiling_outer_next
}

fn g17p_native_fragment_first_mode(mode: u32) -> bool {
    mode == 2 || mode == 3
}

fn g17p_render_deferred_outer_mode(
    native_doorbell_mode: u32,
    split_launch: bool,
) -> g17_resources::G17PDeferredOuterPublication {
    if split_launch {
        // The TA group is complete, but the USC FList control notification
        // can wake it before the intended initial doorbell. Retain both here;
        // split launch publishes only TA after the FList transaction retires.
        g17_resources::G17PDeferredOuterPublication::Both
    } else if g17p_native_fragment_first_mode(native_doorbell_mode) {
        g17_resources::G17PDeferredOuterPublication::Tiling
    } else if native_doorbell_mode == 0 {
        g17_resources::G17PDeferredOuterPublication::Both
    } else {
        g17_resources::G17PDeferredOuterPublication::None
    }
}

fn g17p_native_fragment_preack_mode(mode: u32) -> bool {
    mode == 3
}

fn g17p_native_fragment_outer_consumed(counters: [u32; 3], expected: u32) -> bool {
    counters[0] == expected && counters[2] == expected
}

/// Native TA hardware-buffer ID namespace (`AGXAccelerator + 0x13380`).
///
/// `AGXHardwareBufferIDManager::init` receives 0x7f. Its initial free stack
/// contains 126..0 and is popped from the end, so allocations are exactly
/// 0, 1, ... 126. The bitmap fallback runs only after that stack is empty.
/// The kext's owner array, reference counts, bitmap, and free stack are CPU
/// bookkeeping, but the allocated number has five coupled firmware-visible
/// consumers: TA register 0x1c830, command +0x18, ConfigUpdate +0x7e,
/// PBState +0x0c, and the PBDesc ring index.
const G17P_TA_HARDWARE_BUFFER_ID_COUNT: usize = 0x7f;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum G17PHardwareBufferIdError {
    Exhausted,
    InvalidId,
    DoubleFree,
    NotReserved,
}

/// `g17p_render_slot_globals` bit: derive the outer ring entry's "channel
/// started" byte from the selected native queue pair's retained channel state.
pub(crate) const G17P_RENDER_SLOT_GLOBAL_CHANNEL_STARTED: u32 = 1;
/// `g17p_render_slot_globals` bit: tag-15 `+0x3e` publishes the selected
/// render owner's FList lifecycle sequence rather than its submit ordinal.
pub(crate) const G17P_RENDER_SLOT_GLOBAL_FLIST_GENERATION: u32 = 2;

fn g17p_render_slot_globals() -> u32 {
    crate::g17_config::RENDER_SLOT_GLOBALS
}

fn g17p_render_pair_first_outer_submit(started: &[bool], slot: u8) -> Option<bool> {
    started.get(usize::from(slot)).map(|started| !*started)
}


#[derive(Debug, Clone)]
struct G17PHardwareBufferIdAllocator {
    free: [u64; 2],
    refs: [u8; G17P_TA_HARDWARE_BUFFER_ID_COUNT],
    tokens: [u64; G17P_TA_HARDWARE_BUFFER_ID_COUNT],
    stack: [u8; G17P_TA_HARDWARE_BUFFER_ID_COUNT],
    stack_len: usize,
    next_parameter_buffer_token: u64,
    /// IDs owned by a live retained render storage. A storage's
    /// parameter-management graph and PBDesc slot are encoded against one ID
    /// for the storage's whole lifetime (`prepare_retained_scene` refuses any
    /// other), so between that storage's submissions the ID is free but must
    /// never be handed to a different owner: it leaves this set only when the
    /// storage is dropped.
    reserved: [u64; 2],
    /// Graph lifetime holds, separate from all 255 submission-reference slots.
    owner_refs: [u64; 2],
}

impl G17PHardwareBufferIdAllocator {
    fn new() -> Self {
        let mut stack = [0u8; G17P_TA_HARDWARE_BUFFER_ID_COUNT];
        for (index, slot) in stack.iter_mut().enumerate() {
            *slot = (G17P_TA_HARDWARE_BUFFER_ID_COUNT - 1 - index) as u8;
        }
        Self {
            // 127 valid bits: IDs 0..126.
            free: [u64::MAX, u64::MAX >> 1],
            refs: [0; G17P_TA_HARDWARE_BUFFER_ID_COUNT],
            tokens: [0; G17P_TA_HARDWARE_BUFFER_ID_COUNT],
            stack,
            stack_len: G17P_TA_HARDWARE_BUFFER_ID_COUNT,
            next_parameter_buffer_token: 0,
            reserved: [0; 2],
            owner_refs: [0; 2],
        }
    }

    fn is_free(&self, id: u32) -> bool {
        let index = id as usize;
        index < G17P_TA_HARDWARE_BUFFER_ID_COUNT
            && self.free[index / 64] & (1u64 << (index % 64)) != 0
    }

    fn is_reserved(&self, id: u32) -> bool {
        let index = id as usize;
        index < G17P_TA_HARDWARE_BUFFER_ID_COUNT
            && self.reserved[index / 64] & (1u64 << (index % 64)) != 0
    }

    fn push_stack(&mut self, id: u32) {
        if self.stack[..self.stack_len].contains(&(id as u8)) {
            return;
        }
        if self.stack_len < self.stack.len() {
            self.stack[self.stack_len] = id as u8;
            self.stack_len += 1;
        }
    }

    fn remove_from_stack(&mut self, id: u32) {
        if let Some(at) = self.stack[..self.stack_len].iter().position(|entry| *entry == id as u8) {
            self.stack.copy_within(at + 1..self.stack_len, at);
            self.stack_len -= 1;
        }
    }

    /// Hand out an ID no live storage owns, in native order.
    fn allocate(&mut self) -> core::result::Result<u32, G17PHardwareBufferIdError> {
        let mut popped = None;
        while self.stack_len != 0 {
            self.stack_len -= 1;
            let candidate = self.stack[self.stack_len] as u32;
            // A reserved ID is removed from the stack when reserved; skipping
            // one here is defence in depth, never the normal path.
            if !self.is_reserved(candidate) {
                popped = Some(candidate);
                break;
            }
        }
        let id = if let Some(id) = popped {
            id
        } else {
            let available = [
                self.free[0] & !self.reserved[0],
                self.free[1] & !self.reserved[1],
            ];
            if available[0] != 0 {
                available[0].trailing_zeros()
            } else if available[1] != 0 {
                64 + available[1].trailing_zeros()
            } else {
                return Err(G17PHardwareBufferIdError::Exhausted);
            }
        };
        if !self.is_free(id) {
            return Err(G17PHardwareBufferIdError::DoubleFree);
        }
        let index = id as usize;
        self.free[index / 64] &= !(1u64 << (index % 64));
        self.refs[index] = 1;
        Ok(id)
    }

    /// Allocate a retained compute owner from the same HardwareBufferID
    /// namespace as render. ID zero belongs to the bootstrap/render owner;
    /// native's captured retained CL queues then receive 1, 2 and 3 in
    /// creation order. A compute owner keeps its ID until the firmware session
    /// is stopped because its tag-15 record and QoS scheduler-state slot both
    /// retain that identity.
    fn allocate_compute(&mut self) -> core::result::Result<u32, G17PHardwareBufferIdError> {
        for id in 1..G17P_TA_HARDWARE_BUFFER_ID_COUNT as u32 {
            if self.is_free(id) && !self.is_reserved(id) {
                let index = id as usize;
                self.free[index / 64] &= !(1u64 << (index % 64));
                self.refs[index] = 1;
                self.remove_from_stack(id);
                return Ok(id);
            }
        }
        Err(G17PHardwareBufferIdError::Exhausted)
    }

    /// Legacy reservation without a counted owner, retained for diagnostics.
    fn reserve(&mut self, id: u32) -> core::result::Result<(), G17PHardwareBufferIdError> {
        self.reserve_with_owner_ref(id, false)
    }

    /// Reserve the ID for this graph and optionally count its existing owner
    /// lifetime. This does not reserve another ID or another GPU allocation.
    /// The mode is captured here: later changes to the diagnostic mask cannot
    /// change how this graph's last reference is released.
    fn reserve_with_owner_ref(&mut self, id: u32, counted: bool)
        -> core::result::Result<(), G17PHardwareBufferIdError>
    {
        let index = id as usize;
        if index >= G17P_TA_HARDWARE_BUFFER_ID_COUNT {
            return Err(G17PHardwareBufferIdError::InvalidId);
        }
        if self.is_reserved(id) { return Ok(()); }
        if counted {
            if self.is_free(id) {
                self.free[index / 64] &= !(1u64 << (index % 64));
                self.tokens[index] = self.next_token();
            }
            self.owner_refs[index / 64] |= 1u64 << (index % 64);
        }
        self.reserved[index / 64] |= 1u64 << (index % 64);
        self.remove_from_stack(id);
        Ok(())
    }

    /// End the retained graph's ownership at its existing retirement/stop
    /// boundary. Outstanding submission holds, if any, still keep the ID live.
    fn unreserve(&mut self, id: u32) {
        if !self.is_reserved(id) { return; }
        let index = id as usize;
        let bit = 1u64 << (index % 64);
        self.reserved[index / 64] &= !bit;
        self.owner_refs[index / 64] &= !bit;
        if self.refs[index] == 0 {
            self.free[index / 64] |= bit;
            self.push_stack(id);
        }
    }

    /// Take the ID a retained storage already owns for one more submission.
    fn acquire_reserved(&mut self, id: u32) -> core::result::Result<u32, G17PHardwareBufferIdError> {
        if !self.is_reserved(id) {
            return Err(G17PHardwareBufferIdError::NotReserved);
        }
        let index = id as usize;
        if self.refs[index] == u8::MAX {
            return Err(G17PHardwareBufferIdError::Exhausted);
        }
        if self.is_free(id) {
            self.free[index / 64] &= !(1u64 << (index % 64));
        } else if self.refs[index] == 0
            && self.owner_refs[index / 64] & (1u64 << (index % 64)) == 0
        {
            return Err(G17PHardwareBufferIdError::DoubleFree);
        }
        self.refs[index] += 1;
        Ok(id)
    }

    fn next_token(&mut self) -> u64 {
        let token = if self.next_parameter_buffer_token == u64::MAX {
            0
        } else {
            self.next_parameter_buffer_token
        };
        self.next_parameter_buffer_token = token.wrapping_add(1);
        token
    }

    /// `acquire_reserved` plus a fresh PB owner lease token, exactly as
    /// `allocate_with_token` pairs them for a fresh ID.
    fn acquire_reserved_with_token(
        &mut self,
        id: u32,
    ) -> core::result::Result<(u32, u64), G17PHardwareBufferIdError> {
        let was_free = self.is_free(id);
        let id = self.acquire_reserved(id)?;
        let index = id as usize;
        if was_free {
            self.tokens[index] = self.next_token();
        }
        Ok((id, self.tokens[index]))
    }

    /// Acquire a new PB owner lease, not a render-ordinal-derived generation.
    /// Native IDManager::alloc calls HWPB::prepareBufferResources(true) only
    /// on the reference-count 0 -> 1 transition (8a66f24..8a66f6c). That
    /// prepare uses a shared u64 counter, skipping -1 (8a741f0..8a7421c).
    /// Our serialized allocator has no already-referenced-ID acquisition;
    /// every successful allocation is that transition. Keep the counter
    /// across releases and processor-stop recovery, independently of Scene,
    /// queue ordinals and the separate FList generation namespace.
    fn allocate_with_token(
        &mut self,
    ) -> core::result::Result<(u32, u64), G17PHardwareBufferIdError> {
        let id = self.allocate()?;
        let token = self.next_token();
        self.tokens[id as usize] = token;
        Ok((id, token))
    }

    /// Return a submission's hold on `id`. A reserved ID stays with its
    /// storage -- off the free list -- until that storage unreserves it.
    fn release(&mut self, id: u32) -> core::result::Result<(), G17PHardwareBufferIdError> {
        let index = id as usize;
        if index >= G17P_TA_HARDWARE_BUFFER_ID_COUNT {
            return Err(G17PHardwareBufferIdError::InvalidId);
        }
        // Submission references cannot consume the separate graph hold.
        if self.is_free(id) || self.refs[index] == 0 {
            return Err(G17PHardwareBufferIdError::DoubleFree);
        }
        self.refs[index] -= 1;
        if self.refs[index] != 0
            || self.owner_refs[index / 64] & (1u64 << (index % 64)) != 0
        {
            return Ok(());
        }
        self.free[index / 64] |= 1u64 << (index % 64);
        if !self.is_reserved(id) {
            self.push_stack(id);
        }
        Ok(())
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17PBootstrapUmaCompletion {
    pub(crate) stamp: i32,
    pub(crate) hardware_buffer_id: u32,
    pub(crate) shared_control: u64,
    pub(crate) operand_table: u64,
    pub(crate) cookie: u64,
    pub(crate) count: u32,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17PBootstrapUmaRelease {
    pub(crate) sequence: u64,
    pub(crate) hardware_buffer_id: u32,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17PTvbGrowDisposition {
    IgnoredStale,
    Published,
    Deferred,
    PreparationDeferred,
}

pub(crate) enum G17PPendingPrimaryControlFlush {
    Empty,
    Blocked,
    Published(g17_resources::G17PDeviceControlPublication),
}

struct G17PPendingPrimaryControl {
    record: [u8; g17_initdata::CONTROL_RECORD_SIZE],
    recovery_generation: u64,
    worker_attempts: u16,
}

const G17P_PRIMARY_CONTROL_RETRY_LIMIT: u16 = 1000;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum G17PBootstrapUmaRequestState {
    Submitted,
    Ready,
    Published,
    Sent,
    Completed,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum G17PBootstrapUmaLifecycleError {
    WrongPhase,
    WrongCompletion,
    WrongOwner,
    WrongReferenceCount,
    WrongPendingCount,
}

/// A control is consumed when its producer-after cursor is no longer inside
/// the ring's current `(consumer, producer]` pending interval. This remains
/// exact when firmware consumes several records between host observations and
/// across the masked 8-bit wrap; equality alone would strand earlier owners.
fn g17p_control_ring_target_consumed(
    consumer: u32,
    producer: u32,
    target: u32,
) -> core::result::Result<bool, G17PBootstrapUmaLifecycleError> {
    let mask = g17_initdata::CONTROL_RING_INDEX_MASK;
    if consumer > mask || producer > mask || target > mask {
        return Err(G17PBootstrapUmaLifecycleError::WrongCompletion);
    }
    let pending = producer.wrapping_sub(consumer) & mask;
    let target_ahead = target.wrapping_sub(consumer) & mask;
    Ok(target_ahead == 0 || target_ahead > pending)
}

/// v96: firmware TVB grows taken by the allocation worker (log budget).
static G17P_GROW_TAKEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// v96: FList-ID takeovers observed (diagnostic log budget).
static G17P_FLIST_TAKEOVERS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[cfg(not(test))]
fn g17p_new_render_flist_owners() -> Result<KVec<Option<G17PBootstrapUmaLifecycle>>> {
    let mut owners = KVec::with_capacity(256, GFP_KERNEL)?;
    for _ in 0..256 { owners.push(None, GFP_KERNEL)?; }
    Ok(owners)
}

/// One command-pair lease on a retained FList. Pool bytes outlive this lease.
/// Native updateSubmitInfo increments pending command updates independently
/// of optional async growth; final reference retirement emits one 0x2e using
/// the command's captured generation (KEXT 8aa1804/8aa5340).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct G17PBootstrapUmaLifecycle {
    sequence: u64,
    hardware_buffer_id: u32,
    shared_control: u64,
    operand_table: u64,
    cookie: u64,
    hardware_buffer_owned: bool,
    hardware_buffer_references: u32,
    pending_release_count: u32,
    release_producer_after: Option<u32>,
    /// Control-ring producer cursor of the previous lease's opcode-0x2e
    /// release when this lease was acquired before firmware consumed it.
    /// Native releaseResources emits 0x2e and never waits for it; the next
    /// kick's tag-15 rebinds the owner. The cursor is retained only so the
    /// consumption can still be observed. Cursors retire in ring order, so
    /// the latest one covers every earlier release on this pair.
    carried_release_producer_after: Option<u32>,
    request: Option<G17PBootstrapUmaCompletion>,
    /// One retained grow request can coexist with many command references.
    /// Reacquisition carries it without emitting opcode 0x20 a second time.
    request_published: bool,
    state: G17PBootstrapUmaRequestState,
    /// Host-only serial of the pool object this lease registered (0 for the
    /// bootstrap pool); never sent to firmware. See `G17PRenderFlist::serial`.
    pool_serial: u64,
}

impl G17PBootstrapUmaLifecycle {
    /// The owner registered exactly this pool object, not merely a pool that
    /// recycled its FList ID and VAs.
    fn same_pool_object(&self, serial: u64) -> bool {
        self.pool_serial == serial
    }

    fn matches_owner(
        &self,
        hardware_buffer_id: u32,
        shared_control: u64,
        operand_table: u64,
        cookie: u64,
    ) -> bool {
        self.hardware_buffer_id == hardware_buffer_id
            && self.shared_control == shared_control
            && self.operand_table == operand_table
            && self.cookie == cookie
    }

    const fn new() -> Self {
        Self {
            sequence: G17P_BOOTSTRAP_UMA_SEQUENCE,
            hardware_buffer_id: G17P_BOOTSTRAP_UMA_HARDWARE_BUFFER_ID,
            shared_control: g17_initdata::CONTROL_SHARED_ADDRESS,
            operand_table: g17_initdata::CONTROL_OPERAND_TABLE_ADDRESS,
            cookie: G17P_BOOTSTRAP_UMA_COOKIE,
            hardware_buffer_owned: true,
            // prepareBufferResources, Async updateSubmitInfo, TA and 3D each
            // hold one reference. Publication retires prepare; type 13 retires
            // only the async-grow request. Metal keeps the two command refs
            // until normal paired render completion, so type 13 is not the
            // point at which opcode 0x2e may be emitted.
            hardware_buffer_references: 4,
            pending_release_count: 2,
            release_producer_after: None,
            carried_release_producer_after: None,
            request: Some(G17PBootstrapUmaCompletion {
                stamp: G17P_BOOTSTRAP_UMA_STAMP,
                hardware_buffer_id: G17P_BOOTSTRAP_UMA_HARDWARE_BUFFER_ID,
                shared_control: g17_initdata::CONTROL_SHARED_ADDRESS,
                operand_table: g17_initdata::CONTROL_OPERAND_TABLE_ADDRESS,
                cookie: G17P_BOOTSTRAP_UMA_COOKIE,
                count: G17P_BOOTSTRAP_UMA_COUNT,
            }),
            request_published: false,
            state: G17PBootstrapUmaRequestState::Submitted,
            pool_serial: 0,
        }
    }

    /// The current fixed pool has one cold grow request. Acquiring the
    /// unchanged pool creates command references, not another grow request.
    /// Native AGXHardwareBufferIDManager advances the FList cookie only on a
    /// new 0 -> 1 owner reference. Overlapping references to the same retained
    /// owner must keep its cookie stable; otherwise B1 treats the new cookie as
    /// a replacement registration and removes the still-live QID membership.
    /// B1 12278..12298 calls 1b230 from warm tag-15 even with install=0;
    /// this rebinds the owner/generation invalidated by the preceding 0x2e.
    fn acquire(
        previous: Option<&Self>,
        sequence: u64,
    ) -> core::result::Result<Self, G17PBootstrapUmaLifecycleError> {
        let Some(previous) = previous else {
            return if sequence == G17P_BOOTSTRAP_UMA_SEQUENCE {
                Ok(Self::new())
            } else {
                Err(G17PBootstrapUmaLifecycleError::WrongOwner)
            };
        };
        if previous.hardware_buffer_id != G17P_BOOTSTRAP_UMA_HARDWARE_BUFFER_ID
            || previous.sequence.checked_add(1) != Some(sequence)
        {
            return Err(G17PBootstrapUmaLifecycleError::WrongOwner);
        }
        let overlapping = previous.state == G17PBootstrapUmaRequestState::Ready
            && previous.hardware_buffer_owned
            && previous.hardware_buffer_references >= 2
            && previous.pending_release_count >= 2;
        let reacquire = previous.released_for_reacquire();
        if !overlapping && !reacquire {
            return Err(G17PBootstrapUmaLifecycleError::WrongPhase);
        }
        Ok(Self {
            sequence: if overlapping {
                previous.sequence
            } else {
                sequence
            },
            hardware_buffer_id: previous.hardware_buffer_id,
            shared_control: previous.shared_control,
            operand_table: previous.operand_table,
            cookie: previous.cookie,
            hardware_buffer_owned: true,
            hardware_buffer_references: previous
                .hardware_buffer_references
                .checked_add(3)
                .ok_or(G17PBootstrapUmaLifecycleError::WrongReferenceCount)?,
            pending_release_count: previous
                .pending_release_count
                .checked_add(2)
                .ok_or(G17PBootstrapUmaLifecycleError::WrongPendingCount)?,
            release_producer_after: None,
            carried_release_producer_after: previous.outstanding_release_witness(),
            request: None,
            request_published: false,
            state: G17PBootstrapUmaRequestState::Submitted,
            pool_serial: previous.pool_serial,
        })
    }

    fn acquire_owner(
        previous: Option<&Self>,
        sequence: u64,
        hardware_buffer_id: u32,
        shared_control: u64,
        operand_table: u64,
        cookie: u64,
        needs_grow: bool,
    ) -> core::result::Result<Self, G17PBootstrapUmaLifecycleError> {
        let new_request = || G17PBootstrapUmaCompletion {
            stamp: G17P_BOOTSTRAP_UMA_STAMP,
            hardware_buffer_id,
            shared_control,
            operand_table,
            cookie,
            count: G17P_BOOTSTRAP_UMA_COUNT,
        };
        let Some(previous) = previous else {
            if sequence != G17P_BOOTSTRAP_UMA_SEQUENCE { return Err(G17PBootstrapUmaLifecycleError::WrongOwner); }
            // A pre-populated pool has no async-grow reference: prepare, TA
            // and 3D only, like the warm first CL kick in the 25G83 trace.
            return Ok(Self {
                sequence,
                hardware_buffer_id,
                shared_control,
                operand_table,
                cookie,
                hardware_buffer_owned: true,
                hardware_buffer_references: if needs_grow { 4 } else { 3 },
                pending_release_count: 2,
                release_producer_after: None,
                carried_release_producer_after: None,
                request: needs_grow.then(new_request),
                request_published: false,
                state: G17PBootstrapUmaRequestState::Submitted,
                pool_serial: 0,
            });
        };
        if previous.hardware_buffer_id != hardware_buffer_id
            || previous.sequence.checked_add(1) != Some(sequence)
        {
            return Err(G17PBootstrapUmaLifecycleError::WrongOwner);
        }
        let same_owner = previous.shared_control == shared_control
            && previous.operand_table == operand_table
            && previous.cookie == cookie;
        let pending_grow = previous.state == G17PBootstrapUmaRequestState::Submitted
            && previous.request.is_some() && previous.request_published;
        let overlapping = same_owner
            && (previous.state == G17PBootstrapUmaRequestState::Ready || pending_grow)
            && previous.hardware_buffer_owned
            && previous.hardware_buffer_references >= 2
            && previous.pending_release_count >= 2;
        let reacquire = previous.released_for_reacquire();
        if !overlapping && !reacquire {
            return Err(G17PBootstrapUmaLifecycleError::WrongPhase);
        }
        let cold_owner = reacquire && !same_owner;
        let request_grow = cold_owner && needs_grow;
        Ok(Self {
            sequence: if overlapping { previous.sequence } else { sequence },
            hardware_buffer_id,
            shared_control,
            operand_table,
            cookie,
            hardware_buffer_owned: true,
            hardware_buffer_references: if request_grow { 4 } else {
                previous.hardware_buffer_references.checked_add(3)
                    .ok_or(G17PBootstrapUmaLifecycleError::WrongReferenceCount)?
            },
            pending_release_count: previous.pending_release_count.checked_add(2)
                .ok_or(G17PBootstrapUmaLifecycleError::WrongPendingCount)?,
            release_producer_after: None,
            carried_release_producer_after: previous.outstanding_release_witness(),
            request: if request_grow { Some(new_request()) }
                else if overlapping && pending_grow { previous.request } else { None },
            request_published: overlapping && pending_grow,
            state: G17PBootstrapUmaRequestState::Submitted,
            pool_serial: previous.pool_serial,
        })
    }

    fn retire_prepare_reference(
        &mut self,
    ) -> core::result::Result<(), G17PBootstrapUmaLifecycleError> {
        if self.state != G17PBootstrapUmaRequestState::Submitted
            || !self.hardware_buffer_owned
            || self.hardware_buffer_references < if self.request.is_some() { 4 } else { 3 }
        {
            return Err(G17PBootstrapUmaLifecycleError::WrongPhase);
        }
        self.hardware_buffer_references -= 1;
        if self.request.is_none() {
            self.state = G17PBootstrapUmaRequestState::Ready;
        }
        Ok(())
    }

    fn match_completion(
        &mut self,
        completion: G17PBootstrapUmaCompletion,
    ) -> core::result::Result<(), G17PBootstrapUmaLifecycleError> {
        if self.state != G17PBootstrapUmaRequestState::Submitted {
            return Err(G17PBootstrapUmaLifecycleError::WrongPhase);
        }
        if Some(completion) != self.request {
            return Err(G17PBootstrapUmaLifecycleError::WrongCompletion);
        }
        if !self.hardware_buffer_owned
            || completion.hardware_buffer_id != self.hardware_buffer_id
        {
            return Err(G17PBootstrapUmaLifecycleError::WrongOwner);
        }
        if self.hardware_buffer_references != self.pending_release_count.checked_add(1)
            .ok_or(G17PBootstrapUmaLifecycleError::WrongReferenceCount)? {
            return Err(G17PBootstrapUmaLifecycleError::WrongReferenceCount);
        }
        if self.pending_release_count == 0 {
            return Err(G17PBootstrapUmaLifecycleError::WrongPendingCount);
        }

        // Type 13 owns one reference, independently of queued commands.
        self.hardware_buffer_references -= 1;
        self.state = G17PBootstrapUmaRequestState::Ready;
        Ok(())
    }

    fn retire_render_references(
        &mut self,
    ) -> core::result::Result<Option<G17PBootstrapUmaRelease>, G17PBootstrapUmaLifecycleError> {
        if self.state != G17PBootstrapUmaRequestState::Ready
            || !self.hardware_buffer_owned
            || self.hardware_buffer_references < 2
        {
            return Err(G17PBootstrapUmaLifecycleError::WrongPhase);
        }
        if self.pending_release_count < 2 {
            return Err(G17PBootstrapUmaLifecycleError::WrongPendingCount);
        }
        self.hardware_buffer_references -= 2;
        self.pending_release_count -= 2;
        if self.hardware_buffer_references != 0 {
            return Ok(None);
        }
        self.hardware_buffer_owned = false;
        Ok(Some(G17PBootstrapUmaRelease {
            sequence: self.sequence,
            hardware_buffer_id: self.hardware_buffer_id,
        }))
    }

    fn pending_release(&self) -> Option<G17PBootstrapUmaRelease> {
        (self.state == G17PBootstrapUmaRequestState::Ready
            && self.hardware_buffer_references == 0)
        .then_some(
            G17PBootstrapUmaRelease {
                sequence: self.sequence,
                hardware_buffer_id: self.hardware_buffer_id,
            },
        )
    }

    fn mark_published(
        &mut self,
        release: G17PBootstrapUmaRelease,
    ) -> core::result::Result<(), G17PBootstrapUmaLifecycleError> {
        if self.pending_release() != Some(release) {
            return Err(G17PBootstrapUmaLifecycleError::WrongPhase);
        }
        self.state = G17PBootstrapUmaRequestState::Published;
        Ok(())
    }

    fn mark_sent(
        &mut self,
        producer_after: u32,
    ) -> core::result::Result<(), G17PBootstrapUmaLifecycleError> {
        if self.state != G17PBootstrapUmaRequestState::Published
            || self.release_producer_after.is_some()
        {
            return Err(G17PBootstrapUmaLifecycleError::WrongPhase);
        }
        self.release_producer_after = Some(producer_after);
        self.state = G17PBootstrapUmaRequestState::Sent;
        Ok(())
    }

    fn release_sent(&self) -> bool {
        self.state == G17PBootstrapUmaRequestState::Sent
            && self.release_producer_after.is_some()
    }

    fn mark_completed(
        &mut self,
        producer_after: u32,
    ) -> core::result::Result<(), G17PBootstrapUmaLifecycleError> {
        if !self.release_sent() || self.release_producer_after != Some(producer_after) {
            return Err(G17PBootstrapUmaLifecycleError::WrongPhase);
        }
        self.release_producer_after = None;
        self.state = G17PBootstrapUmaRequestState::Completed;
        Ok(())
    }

    fn observe_release_consumer(
        &mut self,
        consumer: u32,
        producer: u32,
    ) -> core::result::Result<bool, G17PBootstrapUmaLifecycleError> {
        let target = self
            .release_producer_after
            .ok_or(G17PBootstrapUmaLifecycleError::WrongPhase)?;
        if !self.release_sent() {
            return Err(G17PBootstrapUmaLifecycleError::WrongPhase);
        }
        if !g17p_control_ring_target_consumed(consumer, producer, target)? {
            return Ok(false);
        }
        self.mark_completed(target)?;
        Ok(true)
    }

    /// A lease whose final reference has been dropped and whose 0x2e release
    /// is published: the pair may be acquired again immediately. Native
    /// never waits for firmware to consume the release before the next kick;
    /// a still-unconsumed cursor is carried by the successor as a witness.
    fn released_for_reacquire(&self) -> bool {
        matches!(
            self.state,
            G17PBootstrapUmaRequestState::Completed | G17PBootstrapUmaRequestState::Sent
        ) && !self.hardware_buffer_owned
            && self.hardware_buffer_references == 0
            && self.pending_release_count == 0
    }

    /// The latest release cursor this lease still has to see consumed: its
    /// own sent release, else one carried from a predecessor.
    fn outstanding_release_witness(&self) -> Option<u32> {
        if self.release_sent() {
            self.release_producer_after
        } else {
            self.carried_release_producer_after
        }
    }

    /// Whether any release cursor on this lease is still unobserved.
    fn has_release_witness(&self) -> bool {
        self.release_sent() || self.carried_release_producer_after.is_some()
    }

    /// Settle every unobserved release cursor against one ring snapshot.
    /// Returns how many cursors were observed consumed by this call.
    fn settle_release_witnesses(
        &mut self,
        consumer: u32,
        producer: u32,
    ) -> core::result::Result<u32, G17PBootstrapUmaLifecycleError> {
        let mut settled = 0;
        if let Some(target) = self.carried_release_producer_after {
            if g17p_control_ring_target_consumed(consumer, producer, target)? {
                self.carried_release_producer_after = None;
                settled += 1;
            }
        }
        if self.release_sent() && self.observe_release_consumer(consumer, producer)? {
            settled += 1;
        }
        Ok(settled)
    }
}

// Pinned J700 T8140 GFX A000 scheduler root used by the exact EP type-4
// thunk. The extracted image has SHA-256
// 3e2821bb40d3abffe30488a06ede5f523cfa47544d6a0ad7654a08be6b5dc30f.
// Its __DATA vmaddr/size are 0xfffffc000004c000/0x134000, exactly matching
// the J700 gfx-data carveout size. These addresses are diagnostic only.
const G17P_GFX_LINK_DATA_BASE: u64 = 0xffff_fc00_0004_c000;
const G17P_GFX_LINK_DATA_END: u64 = 0xffff_fc00_0018_0000;
const G17P_GFX_PHYSICAL_DATA_BASE: u64 = 0x0000_0100_01d7_c000;

/// Translate a GFX firmware LINK address into the physical address the host
/// can map. Same arithmetic the render status-page probes use.
pub(crate) fn g17p_gfx_link_to_physical(link: u64) -> u64 {
    G17P_GFX_PHYSICAL_DATA_BASE + (link - G17P_GFX_LINK_DATA_BASE)
}
/// Firmware per-QID dependency records: `0x1261f0 + qid*0x18`, valid byte at
/// `+0`, completed stamp at `+8`. Firmware DATA segment, so host-readable.
const G17P_DEPENDENCY_RECORD_LINK_BASE: u64 = 0xffff_fc00_0012_61f0;
/// GDirector per-HardwareBufferID shadow, `{u8 priority, u8 share}` per entry.
/// `FUN_1451c` writes `sgx+0xd058 = prio | (share << 8)` and mirrors it here,
/// where `share = ((weight << 5) / sum_weights) - 1` clamped at 0 and `0xff`
/// means never programmed. Encoded share 0 does not by itself prove zero service.
const G17P_GDIRECTOR_SHADOW_LINK_BASE: u64 = 0xffff_fc00_0012_f560;
/// Submit-time firmware DRAM the tag-15 handler maintains. These are the only
/// structures the KSM front-end plausibly scans that have never been diffed
/// between a queue that launches and one that does not.
/// Per-DM active-queue bitmap, `0x128a30 + dm*0x10` (+`0x08` is its sibling).
/// The scheduler pass ANDs against exactly these before launching.
const G17P_DM_ACTIVE_BITMAP_LINK_BASE: u64 = 0xffff_fc00_0012_8a30;
/// Per-queue state, `0x127628 + qid*0x28` (written at fw `0x119b4`).
const G17P_QUEUE_STATE_LINK_BASE: u64 = 0xffff_fc00_0012_7628;
/// KSM park record, `0x58010 + qid*0x28`: byte A at +0x418, B at +0x419,
/// C at +0x41a, deadline at +0x400. FW_PROVEN: the scheduler pass compares
/// A and B at 0x13100 and PARKS the queue when they are equal -- KTrace 0x42,
/// runnable bit cleared, then a status-4 NOP retire. A is written only by the
/// admit block at 0x12e38 (which also zeroes B and C); B only by the retire
/// path at 0xcf0c. Both start at zero, so a queue whose admit never ran is
/// parked by construction. Nothing has ever read these bytes from the host.
const G17P_KSM_PARK_RECORD_LINK_BASE: u64 = 0xffff_fc00_0005_8010;
/// B1 scheduler-global masks surrounding the park records. The scheduler pass
/// at `0x12d00` reads the requested pair at `+0x3c8`, stores its sampled
/// hardware-status pair at `+0x3d8`, and keeps blocked/deferred state at
/// `+0x3e8`; `+0x3f8` is the following pair used by that same pass. These are
/// firmware DATA. Never probe the corresponding live MMIO status registers
/// from the host.
const G17P_KSM_SCHEDULER_MASKS_LINK: u64 = 0xffff_fc00_0005_83c8;
/// Per-QID runnable mask updated by the park path at `0x131cc..0x131e4`.
const G17P_KSM_RUNNABLE_MASK_LINK: u64 = 0xffff_fc00_0005_9938;
/// B1 IRQ-to-scheduler handoff words. The hardware IRQ sampler, atomic pending
/// transfer and dispatcher use these in order. Capturing all five after a
/// fault distinguishes an absent IRQ from a pending RTOS wake or dispatch.
const G17P_KSM_IRQ_HANDOFF_LINKS: [u64; 5] = [
    0xffff_fc00_0017_7100,
    0xffff_fc00_0017_7188,
    0xffff_fc00_0017_7190,
    0xffff_fc00_0017_7200,
    0xffff_fc00_0017_7288,
];
/// FList worker busy flag and the four worker-ring state blocks identified by
/// the B1 worker. Fault-only snapshots retain the raw qwords; their individual
/// producer/consumer fields are not inferred here.
const G17P_FLIST_WORKER_BUSY_LINK: u64 = 0xffff_fc00_0017_9598;
const G17P_FLIST_WORKER_RING_LINKS: [u64; 4] = [
    0xffff_fc00_0017_a5e0,
    0xffff_fc00_0017_b5f0,
    0xffff_fc00_0017_c600,
    0xffff_fc00_0017_d610,
];
/// Cached hardware-register bookkeeping mask (fw `0x11b5c`, `0xe77c`).
/// This is not the runnable mask; parking and retirement also set its bits.
const G17P_READY_MASK_LINK_BASE: u64 = 0xffff_fc00_0017_6ec0;
/// `0x175aa0 + qid*0x28 + 0x40` (fw `0x11b3c`).
const G17P_QUEUE_FLAG_LINK_BASE: u64 = 0xffff_fc00_0017_5aa0;
/// B1's private FList registration rows. Row `id` starts at base + id*0xe8;
/// registration stores the exact FList pointer at +0, cookie at +8 and
/// context at +0x10. These are firmware DATA, never live TEXT.
const G17P_FLIST_ROW_LINK_BASE: u64 = 0xffff_fc00_0005_b778;
const G17P_FLIST_ROW_STRIDE: u64 = 0xe8;
/// B1 HWPB registration rows (fw 0x122a4..0x12358), independent of FList IDs.
const G17P_HWPB_ROW_LINK_BASE: u64 = 0xffff_fc00_0006_bd18;
const G17P_HWPB_ROW_STRIDE: u64 = 0x50;

fn g17p_binding_row_address(base: u64, id: u32, stride: u64) -> Option<u64> {
    if stride == 0 || stride & 7 != 0 { return None; }
    let start = base.checked_add(u64::from(id).checked_mul(stride)?)?;
    let end = start.checked_add(stride)?;
    if start < G17P_GFX_LINK_DATA_BASE || start & 7 != 0 || end > G17P_GFX_LINK_DATA_END {
        return None;
    }
    Some(start)
}

/// Static B1 callback/waiter namespace. No firmware pointer is followed.
/// Native 23230/aaf0/cfd8; see NATIVE-PRIORITY-AND-DISPATCH.md read plan.
fn g17p_render_callback_addresses(hwpb_id: u32, fragment_qid: u16) -> Option<[u64; 6]> {
    if hwpb_id >= 128 || fragment_qid >= 128 { return None; }
    let qid = u64::from(fragment_qid);
    Some([
        0xffff_fc00_0005_9948 + u64::from(hwpb_id) * 0x38,
        0xffff_fc00_0012_6e20 + qid * 0x10,
        0xffff_fc00_0012_f6f0 + qid * 8,
        G17P_DEPENDENCY_RECORD_LINK_BASE + qid * 0x18 + 8,
        0xffff_fc00_0012_5df0 + qid * 8,
        0xffff_fc00_0017_7190,
    ])
}

/// Fault-only firmware DATA, using the same GFX mapping as the CL row census.
/// Stable identity does not imply an atomic snapshot of mutable membership.
fn g17p_read_binding_row<const N: usize>(base: u64, id: u32, stride: u64) -> Result<[u64; N]> {
    if N < 3 || (N as u64).checked_mul(8) != Some(stride) { return Err(EINVAL); }
    let start = g17p_binding_row_address(base, id, stride).ok_or(EINVAL)?;
    let read = |index: usize| -> Result<u64> {
        pgtable::LiveFirmwareU64Probe::new(g17p_gfx_link_to_physical(start + index as u64 * 8))
            .and_then(|probe| probe.observe_mask(u64::MAX, 1))
            .map(|sample| sample.first)
    };
    let mut row = [0; N];
    for (index, value) in row.iter_mut().enumerate() { *value = read(index)?; }
    for index in 0..3 {
        if read(index)? != row[index] { return Err(EAGAIN); }
    }
    Ok(row)
}

/// Explicit diagnostic read of retained GFX accounting clocks. No MMIO and no
/// firmware writes. Caller requires a frozen, nonempty G17 timing capture.
/// Sample timestamps can advance independently; frequency/epoch fields must
/// remain stable throughout the read or the result is rejected.
pub(crate) fn diagnostic_accounting_clocks() -> Result<[u64; 8]> {
    const OFFSETS: [u64; 8] = [
        0x124ad8, 0x124af8, 0x124b18, 0x124b38,
        0x6e598, 0x6e5b0, 0x6e5d0, 0x179320,
    ];
    let read = |index: usize| -> Result<u64> {
        let link = 0xffff_fc00_0000_0000u64.checked_add(OFFSETS[index]).ok_or(EOVERFLOW)?;
        let start = g17p_binding_row_address(link, 0, 8).ok_or(EINVAL)?;
        pgtable::LiveFirmwareU64Probe::new(g17p_gfx_link_to_physical(start))
            .and_then(|probe| probe.observe_mask(u64::MAX, 1))
            .map(|sample| sample.first)
    };
    let mut values = [0; 8];
    for index in 0..8 { values[index] = read(index)?; }
    for index in 4..8 {
        if read(index)? != values[index] { return Err(EAGAIN); }
    }
    Ok(values)
}

/// Retained gUPM grow-worker request, copied by firmware 1a88c before the
/// 14020/14018 launch. Read DATA only; never read or write those MMIO ports.
/// Two equal observations reduce replacement ambiguity, not prove atomicity.
/// Completion clears active early, so an inactive record is historical data.
fn diagnostic_gupm_request() -> Result<[u64; 8]> {
    const LINK: u64 = 0xffff_fc00_0017_9598;
    let first = g17p_read_binding_row::<8>(LINK, 0, 0x40)?;
    let second = g17p_read_binding_row::<8>(LINK, 0, 0x40)?;
    if first != second { return Err(EAGAIN); }
    Ok(first)
}

/// Four-qword (256-pool) bitmap used by B1's FList worker at 1c888..1c8e0.
/// This differs from each pool's two-qword (128-QID) membership bitmap.
const G17P_FLIST_ACTIVE_BITMAP_LINK: u64 = G17P_FLIST_ROW_LINK_BASE + 0x1_0440;
/// Tag-15's per-QID owner pointer table (`base + qid*8`).
const G17P_QID_OWNER_LINK_BASE: u64 = 0xffff_fc00_0012_59d8;
const G17P_DEPENDENCY_RECORD_STRIDE: u64 = 0x18;
const G17P_GFX1_LINK_DATA_BASE: u64 = 0xffff_fc00_0004_c000;
const G17P_GFX1_LINK_DATA_END: u64 = 0xffff_fc00_0017_8000;
const G17P_GFX1_PHYSICAL_DATA_BASE: u64 = 0x0000_0100_01eb_0000;
const G17P_SCHEDULER_ROOT_LINK_ADDRESS: u64 = 0xffff_fc00_0017_7190;
const G17P_SCHEDULER_ROOT_PHYSICAL_ADDRESS: u64 = 0x0000_0100_01ea_7190;
const G17P_SCHEDULER_ROOT_SNAPSHOT_SIZE: usize = 0x80;
const G17P_SCHEDULER_ROOT_PENDING_OFFSET: usize = 0x00;
const G17P_SCHEDULER_ROOT_DEFERRED_OFFSET: usize = 0x50;
const G17P_SCHEDULER_ROOT_STATE_OFFSET: usize = 0x68;
const G17P_RUNTIME_MAIN_GLOBAL_LINK_ADDRESS: u64 = 0xffff_fc00_0017_7030;
const G17P_RUNTIME_MAIN_GLOBAL_PHYSICAL_ADDRESS: u64 = 0x0000_0100_01ea_7030;
const G17P_TYPE4_EP_HANDLER_LINK_ADDRESS: u64 = 0xffff_fc00_0002_2e24;
const G17P_TYPE4_THUNK_LINK_ADDRESS: u64 = 0xffff_fc00_0002_635c;
const G17P_TYPE4_DRAIN_LINK_ADDRESS: u64 = 0xffff_fc00_0000_3a74;
const G17P_TYPE4_PENDING_MASK: u64 = 0x0002_0000;
const G17P_SCHEDULER_DEFERRED_REQUEUE_LINK_ADDRESS: u64 = 0xffff_fc00_0002_5ea4;
const G17P_TYPE4_MAIN_CONTROL_POINTER_OFFSETS: [usize; 3] = [0x1a0, 0x1a8, 0x1b0];
const G17P_MBI_GLOBAL_LINK_ADDRESS: u64 = 0xffff_fc00_0006_e840;
const G17P_MBI_STATIC_ARRAY_LINK_ADDRESS: u64 = 0xffff_fc00_0006_e800;
const G17P_MBI_GROUP_COUNT: u32 = 8;
const G17P_MBI_TRACE_CAPACITY: u32 = 128;
const G17P_MBI_TRACE_RECORD_SIZE: usize = 0x18;
// Primary A000 0x19184..0x191a0 initializes the adjacent endpoint handles;
// EP21 type 9 increments the lease at 0x22de4..0x22df8.
const G17P_PRIMARY_PEER_LEASE_LINK_ADDRESS: u64 = 0xffff_fc00_0017_7798;
const G17P_PRIMARY_PEER_LEASE_PHYSICAL_ADDRESS: u64 = 0x0000_0100_01ea_7798;
// Exact J700 GFX1 A000 (SHA-256 0caf8165...17fd63) initializes handles at
// 0x1bde0..0x1bdfc, increments this lease at 0x26738..0x26748, and decrements
// it only on ACTIVE->SLEEP at 0x25e70..0x25e7c. ACTIVE->NAP bypasses the write.
const G17P_GFX1_PEER_LEASE_LINK_ADDRESS: u64 = 0xffff_fc00_0016_d1c8;
const G17P_GFX1_PEER_LEASE_PHYSICAL_ADDRESS: u64 = 0x0000_0100_01fd_11c8;
const G17P_RTKIT_PRIVATE_ADDRESS_MASK: u64 = (1u64 << 42) - 1;

#[cfg(not(test))]
const _: () = {
    assert!(
        G17P_SCHEDULER_ROOT_PHYSICAL_ADDRESS
            == g17_adt_j700::J700_GFX_DATA.base
                + (G17P_SCHEDULER_ROOT_LINK_ADDRESS - G17P_GFX_LINK_DATA_BASE)
    );
    assert!(
        G17P_SCHEDULER_ROOT_PHYSICAL_ADDRESS + G17P_SCHEDULER_ROOT_SNAPSHOT_SIZE as u64
            <= g17_adt_j700::J700_GFX_DATA.base + g17_adt_j700::J700_GFX_DATA.size
    );
    assert!(G17P_SCHEDULER_ROOT_PHYSICAL_ADDRESS & 7 == 0);
    assert!(G17P_SCHEDULER_ROOT_SNAPSHOT_SIZE & 7 == 0);
    assert!(G17P_SCHEDULER_ROOT_STATE_OFFSET + 1 <= G17P_SCHEDULER_ROOT_SNAPSHOT_SIZE);
    assert!(G17P_GFX_LINK_DATA_END - G17P_GFX_LINK_DATA_BASE == 0x13_4000);
    assert!(G17P_GFX1_LINK_DATA_END - G17P_GFX1_LINK_DATA_BASE == 0x12_c000);
    assert!(G17P_GFX_PHYSICAL_DATA_BASE == g17_adt_j700::J700_GFX_DATA.base);
    assert!(G17P_GFX1_PHYSICAL_DATA_BASE == g17_adt_j700::J700_GFX1_DATA.base);
    assert!(
        G17P_RUNTIME_MAIN_GLOBAL_PHYSICAL_ADDRESS
            == g17_adt_j700::J700_GFX_DATA.base
                + (G17P_RUNTIME_MAIN_GLOBAL_LINK_ADDRESS - G17P_GFX_LINK_DATA_BASE)
    );
    assert!(
        G17P_RUNTIME_MAIN_GLOBAL_PHYSICAL_ADDRESS + core::mem::size_of::<u64>() as u64
            <= g17_adt_j700::J700_GFX_DATA.base + g17_adt_j700::J700_GFX_DATA.size
    );
    assert!(
        G17P_PRIMARY_PEER_LEASE_PHYSICAL_ADDRESS
            == G17P_GFX_PHYSICAL_DATA_BASE
                + (G17P_PRIMARY_PEER_LEASE_LINK_ADDRESS - G17P_GFX_LINK_DATA_BASE)
    );
    assert!(
        G17P_GFX1_PEER_LEASE_PHYSICAL_ADDRESS
            == G17P_GFX1_PHYSICAL_DATA_BASE
                + (G17P_GFX1_PEER_LEASE_LINK_ADDRESS - G17P_GFX1_LINK_DATA_BASE)
    );
};

/// Grounded manager inputs for T8140. Power and tuning values are excluded.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct T8140ManagerConfig {
    pub(crate) chip_id: u32,
    pub(crate) base_clock_hz: u32,
    pub(crate) hw_family: u8,
    pub(crate) hw_variant: u8,
    pub(crate) num_dies: u32,
    pub(crate) uat_ias: u8,
    pub(crate) uat_oas: u32,
    pub(crate) uat_page_size: u16,
    pub(crate) uat_page_bits: u8,
    pub(crate) uat_levels: [(u8, u16); g17_initdata::UAT_LEVEL_COUNT],
    pub(crate) initdata_version: [u16; 4],
    pub(crate) root_sizes: [usize; 2],
    pub(crate) secondary_root_delta: u64,
    pub(crate) secondary_shared_region_delta: u64,
    pub(crate) shared_cluster_size: usize,
    pub(crate) work_channel_count: usize,
    pub(crate) submission_backend: identity::SubmissionBackendSelection,
}

/// Exact facts needed to construct, but not execute, a T8140 manager.
pub(crate) const T8140_MANAGER_CONFIG: T8140ManagerConfig = T8140ManagerConfig {
    chip_id: 0x8140,
    base_clock_hz: 24_000_000,
    hw_family: 0x0a,
    hw_variant: 0,
    num_dies: 1,
    uat_ias: 42,
    uat_oas: 42,
    uat_page_size: g17_initdata::UAT_PAGE_SIZE,
    uat_page_bits: g17_initdata::UAT_PAGE_BITS,
    uat_levels: g17_initdata::UAT_LEVELS,
    initdata_version: g17_initdata::INITDATA_VERSION,
    root_sizes: [
        g17_initdata::ROOT_SIZE_PRIMARY,
        g17_initdata::ROOT_SIZE_SECONDARY,
    ],
    secondary_root_delta: g17_initdata::SECONDARY_ROOT_DELTA,
    secondary_shared_region_delta: g17_initdata::SECONDARY_SHARED_REGION_DELTA,
    shared_cluster_size: g17_initdata::NATIVE_SHARED_CLUSTER_SIZE,
    work_channel_count: g17_initdata::WORK_CHANNEL_COUNT,
    submission_backend: identity::T8140_G17P_SUBMISSION_BACKEND,
};

/// Platform-derived SKSM scratch offsets within the mapped accelerator
/// register window.
///
/// Apple's host loads these three values from accelerator object fields
/// `+0x10730`, `+0x10734`, and `+0x10738`. Those are object offsets, not SGX
/// MMIO offsets. This type carries the loaded values and no physical base.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17SksmScratchGeometry {
    pub(crate) write0_base: u32,
    pub(crate) write1_base: u32,
    pub(crate) stride: u32,
}

/// The two offsets used by the fixed G17P scratch FIFO slot.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17SksmScratchOffsets {
    pub(crate) write0: u32,
    pub(crate) write1: u32,
}

impl G17SksmScratchGeometry {
    /// Apply the exact host equation for fixed FIFO slot 9.
    ///
    /// The slot is independent of the SKSM queue ID carried in each write.
    pub(crate) const fn fifo_offsets(
        self,
    ) -> core::result::Result<G17SksmScratchOffsets, ConstructionError> {
        const FIFO_SLOT: u32 = 9;

        let fifo_offset = match self.stride.checked_mul(FIFO_SLOT) {
            Some(offset) => offset,
            None => return Err(ConstructionError::SksmScratchOffsetOverflow),
        };
        let write0 = match self.write0_base.checked_add(fifo_offset) {
            Some(offset) => offset,
            None => return Err(ConstructionError::SksmScratchOffsetOverflow),
        };
        let write1 = match self.write1_base.checked_add(fifo_offset) {
            Some(offset) => offset,
            None => return Err(ConstructionError::SksmScratchOffsetOverflow),
        };

        Ok(G17SksmScratchOffsets { write0, write1 })
    }
}

/// Target configuration supplied by platform discovery.
///
/// There is no default T8140 scratch geometry. A caller must recover the
/// three values through the platform configuration path before construction.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17PPlatformConfig {
    pub(crate) sksm_scratch: G17SksmScratchGeometry,
    pub(crate) sksm_queue_geometry: g17_submission::G17SksmQueueGeometry,
}

/// G17P values written by `AGXAcceleratorG16::configureDevice`.
///
/// At `0xfffffe0008a55674..0xfffffe0008a55684`, the host copies the constant
/// at `0xfffffe000714b348` (`00 40 00 00 08 40 00 00`) into accelerator
/// fields `+0x10730/+0x10734`, then writes `0x10` to `+0x10738`.
pub(crate) const T8140_G17P_PLATFORM_CONFIG: G17PPlatformConfig = G17PPlatformConfig {
    sksm_scratch: G17SksmScratchGeometry {
        write0_base: 0x4000,
        write1_base: 0x4008,
        stride: 0x10,
    },
    sksm_queue_geometry: g17_submission::T8140_G17P_HAL200_SKSM_QUEUE_GEOMETRY,
};

/// SoC and register identity presented to the manager constructor.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct TargetObservation {
    pub(crate) chip_id: u32,
    pub(crate) declared_gen: GpuGen,
    pub(crate) declared_variant: GpuVariant,
    pub(crate) hw_family: u8,
    pub(crate) hw_variant: u8,
    pub(crate) num_dies: u32,
    pub(crate) uat_ias: u8,
    pub(crate) uat_oas: u32,
}

/// Ownership and placement facts copied from the retained resource owner.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct ResourceObservation {
    pub(crate) uat_ttb_base: u64,
    pub(crate) primary_uat_owner: u64,
    pub(crate) secondary_uat_owner: u64,
    pub(crate) primary_role: InstanceRole,
    pub(crate) secondary_role: InstanceRole,
    pub(crate) primary_root: u64,
    pub(crate) secondary_root: u64,
    pub(crate) shared_cluster_size: usize,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum ConstructionError {
    WrongChip,
    WrongDeclaredIdentity,
    WrongHardwareIdentity,
    WrongUatGeometry,
    UndecodableIdentity,
    WrongDecodedIdentity,
    WrongSubmissionBackend,
    NullUatOwner,
    SplitUatOwner,
    WrongRoleTopology,
    WrongRootPlacement,
    WrongSharedClusterSize,
    WrongSksmScratchGeometry,
    WrongSksmQueueGeometry,
    SksmScratchOffsetOverflow,
}

/// Validate the exact T8140 identity without consulting an AGX2 `HwConfig`.
pub(crate) fn validate_target(
    target: TargetObservation,
) -> core::result::Result<(), ConstructionError> {
    let config = &T8140_MANAGER_CONFIG;

    if target.chip_id != config.chip_id {
        return Err(ConstructionError::WrongChip);
    }
    if target.declared_gen != GpuGen::G17 || target.declared_variant != GpuVariant::P {
        return Err(ConstructionError::WrongDeclaredIdentity);
    }
    if target.hw_family != config.hw_family
        || target.hw_variant != config.hw_variant
        || target.num_dies != config.num_dies
    {
        return Err(ConstructionError::WrongHardwareIdentity);
    }
    if target.uat_ias != config.uat_ias || target.uat_oas != config.uat_oas {
        return Err(ConstructionError::WrongUatGeometry);
    }

    let num_dies =
        u8::try_from(target.num_dies).map_err(|_| ConstructionError::UndecodableIdentity)?;
    let decoded = identity::decode_gpu_identity(target.hw_family, target.hw_variant, num_dies)
        .ok_or(ConstructionError::UndecodableIdentity)?;
    if decoded.gpu_gen != GpuGen::G17
        || decoded.gpu_variant != GpuVariant::P
        || decoded.usc_generation != 3
        || decoded.gpu_hal_generation != GpuHalGeneration::Hal200
        || decoded.firmware_roles != FirmwareRoleTopology::Dual
        || decoded.submission_transport != SubmissionTransport::ClassicAndSksm
        || decoded.uat_input_address_bits != config.uat_ias
    {
        return Err(ConstructionError::WrongDecodedIdentity);
    }
    let submission_backend = identity::require_executable_submission_backend(decoded)
        .map_err(|_| ConstructionError::WrongSubmissionBackend)?;
    if submission_backend != config.submission_backend {
        return Err(ConstructionError::WrongSubmissionBackend);
    }

    Ok(())
}

/// Validate that both roles are retained under one UAT with native placement.
pub(crate) fn validate_resources(
    resources: ResourceObservation,
) -> core::result::Result<(), ConstructionError> {
    if resources.uat_ttb_base == 0
        || resources.primary_uat_owner == 0
        || resources.secondary_uat_owner == 0
    {
        return Err(ConstructionError::NullUatOwner);
    }
    if resources.uat_ttb_base != resources.primary_uat_owner
        || resources.uat_ttb_base != resources.secondary_uat_owner
    {
        return Err(ConstructionError::SplitUatOwner);
    }
    if resources.primary_role != InstanceRole::Primary
        || resources.secondary_role != InstanceRole::Secondary
    {
        return Err(ConstructionError::WrongRoleTopology);
    }
    let secondary_root = resources
        .primary_root
        .checked_add(T8140_MANAGER_CONFIG.secondary_root_delta)
        .ok_or(ConstructionError::WrongRootPlacement)?;
    if resources.secondary_root != secondary_root {
        return Err(ConstructionError::WrongRootPlacement);
    }
    if resources.shared_cluster_size != T8140_MANAGER_CONFIG.shared_cluster_size {
        return Err(ConstructionError::WrongSharedClusterSize);
    }

    Ok(())
}

/// Admit only the exact geometry produced by the pinned G17P host.
pub(crate) fn validate_platform(
    platform: G17PPlatformConfig,
) -> core::result::Result<(), ConstructionError> {
    if platform.sksm_scratch != T8140_G17P_PLATFORM_CONFIG.sksm_scratch {
        return Err(ConstructionError::WrongSksmScratchGeometry);
    }
    if platform.sksm_queue_geometry
        != T8140_G17P_PLATFORM_CONFIG.sksm_queue_geometry
    {
        return Err(ConstructionError::WrongSksmQueueGeometry);
    }
    platform.sksm_scratch.fifo_offsets()?;
    platform
        .sksm_queue_geometry
        .validate()
        .map_err(|_| ConstructionError::WrongSksmQueueGeometry)?;
    Ok(())
}

#[cfg(not(test))]
fn target_observation(soc: &'static agx3::SocConfig) -> TargetObservation {
    TargetObservation {
        chip_id: soc.chip_id,
        declared_gen: soc.gpu_gen,
        declared_variant: soc.gpu_variant,
        hw_family: soc.hw_family,
        hw_variant: soc.hw_variant,
        num_dies: soc.num_dies,
        uat_ias: soc.uat_ias,
        uat_oas: soc.uat_oas,
    }
}

#[cfg(not(test))]
fn resource_observation(
    handoff: &g17_resources::G17ResourceHandoff,
    uat: &mmu::Uat,
) -> ResourceObservation {
    ResourceObservation {
        uat_ttb_base: uat.ttb_base(),
        primary_uat_owner: handoff.primary.uat_owner,
        secondary_uat_owner: handoff.secondary.uat_owner,
        primary_role: handoff.primary.role,
        secondary_role: handoff.secondary.role,
        primary_root: handoff.primary.instance.root,
        secondary_root: handoff.secondary.instance.root,
        shared_cluster_size: handoff.shared.hw_data_bundle_alloc,
    }
}

/// Retained construction state for both T8140 firmware roles.
///
/// Creating this object allocates and validates resources. It performs no
/// firmware publication, ASC start, MMIO write, or submission.
#[cfg(not(test))]
pub(crate) struct G17PManagerConstruction {
    resources: KBox<g17_resources::G17ResourceOwner>,
    /// One bounded first-render archive. Allocated before initdata publication.
    /// It belongs to the session until a render prepares it; then it is
    /// attached to that render's slot (`G17PRenderSlotState::trace_capture`)
    /// and exported by ownership transfer only after a terminal observation.
    #[cfg(CONFIG_DEV_COREDUMP)]
    trace_capture: Option<KBox<G17PTraceDump>>,
    platform: G17PPlatformConfig,
    num_clusters: u32,
    /// Cached SGX 0xe01480 bits 19:16 from the pre-ASC GPU-ID probe.
    gpc_perf_state_map: u32,
    /// Cached SGX 0xe01480 bits 3:0 from the same probe.
    gpc_perf_state_map_low: u32,
    /// Cached SGX 0xe0141c bit 0 from the same probe.
    gpc_perf_state_control: u32,
    /// Number of successfully completed context-update groups on each physical
    /// render pair. Retired graphs name the next epoch as their release fence.
    render_context_epochs: [u64; 64],
    /// Closed logical queues whose firmware scheduler state has not been
    /// released and acknowledged yet, each with any render graph a successor
    /// already displaced from a pair. Nothing here outlives the firmware's
    /// consumption of the queue's `0x14` release record; see
    /// `G17PLogicalQueueTeardown`.
    logical_queue_teardowns: KVec<G17PLogicalQueueTeardown>,
    render_hardware_buffer_ids: G17PHardwareBufferIdAllocator,
    /// Unpublished or partially published state leases. Exact published
    /// commands transfer them to their retirement trackers, not a global pool.
    render_command_buffer_state_pairs: [Option<Arc<g17_resources::G17PWorkStateLease>>; 64],
    /// Whether each native TA/3D channel pair has published in this firmware
    /// session. `encodeAcceleratorRingCommand` reads this state from the
    /// selected AGXChannel, so pair 1 starts independently of pair 0.
    render_outer_channels_started: [bool; 64],
    /// Dedicated bootstrap owner pinned by B2 +0x808 for this session. Its
    /// firmware-private consumer remains unresolved; logical queue switching
    /// must never retarget that global field.
    compute_channel_control: Option<g17_resources::G17PClChannelControl>,
    logical_qos_ids: Option<Arc<kernel::sync::Mutex<u128>>>,
    /// The computed B2 page belongs to CL_2, not to an individual QID. Its
    /// counters and channel-control pointer are installed exactly once per
    /// firmware session.
    compute_b2_activation: g17_submission::G17PClB2ActivationState,
    /// Next HWPB-manager generation in this firmware session. A replacement
    /// PM graph must never reuse its predecessor's Scene-owner generation.
    next_render_pm_manager_generation: u64,
    /// `render_pool_id()` of the storage the device-global parameter-buffer
    /// loader currently points at, or 0 for none.
    ///
    /// The PB page-list page and the PBDesc table live at fixed native
    /// addresses and are per-DEVICE, but each retained storage owns its own
    /// parameter-management graph. Only one of them can be installed at a time,
    /// so record which, and re-point the loader when a different owner
    /// submits.
    render_parameter_buffer_owner: u64,
    /// Every physical render pair's retained state. The manager's render
    /// methods operate on the one its cursor selects, which keeps the
    /// byte-critical publication code single-copy; the other pairs stay in
    /// place while their independent queue pairs execute.
    render_slots: G17PRenderSlots,
    /// One authoritative lifecycle per independent FList ID. This table stores
    /// no mappings: logical contexts and teardown graphs retain their pools
    /// until the exact release consumer (or both firmware CPUs stopped).
    render_flist_owners: KVec<Option<G17PBootstrapUmaLifecycle>>,
    render_flist_scan: [u64; 4],
    /// Heap scratch for the per-publication copy of the translated render
    /// command (its HardwareBufferID and context generation are overridden).
    /// Allocated once with the manager; keeps ~1.1 KiB off the publication
    /// stack. Taken for the duration of `begin_translated_render`.
    render_command_scratch: Option<KBox<core::mem::MaybeUninit<g17_uapi::TranslatedRenderCommand>>>,
    render_channel_leases: [Option<[crate::g17_channel::Lease; 2]>; 64],
    accepted_firmware_recovery_generation: u64,
    /// Coalesced firmware recovery: host generation, record count, and the
    /// exact EventMachine stamp/QID blamed by type 4 when one was supplied.
    /// Keep the blame with the deferred recovery work so the worker can settle
    /// the affected output fence after the firmware's final clear.
    pending_primary_recovery: Option<(u64, u32, Option<u8>)>,
    /// A mandatory TVB reply whose event-ring record is already consumed.
    /// Stop both event-ring consumers until this exact encoded reply has a
    /// device-control credit; replaying the request would grow storage twice.
    pending_primary_control: Option<G17PPendingPrimaryControl>,
    /// Full decoded requests already consumed from either event ring. A busy
    /// pair gets no synthetic failure reply; other pairs continue draining.
    pending_render_grows: KVec<g17_completion::G17PFirmwareTvbGrowEvent>,
    active_render_grow: Option<g17_completion::G17PFirmwareTvbGrowEvent>,
    /// Worker retry budget for an unpublished FList release retained in the
    /// bootstrap lifecycle while the device-control ring is full.
    pending_flist_release_worker_attempts: u16,
    /// Tag-14 AddKicks held back until the firmware recovery/power-up closes.
    pending_add_kicks: Option<[u8; g17_submission::G17P_KSM_ADD_KICKS_COMMAND_SIZE]>,
    /// Descriptor address published by the most recent compute submission.
    /// The KSM completion record echoes it at +0x30, so it identifies which
    /// submission a populated record belongs to.
    last_published_descriptor: u64,
    /// One namespace for firmware stamp/QID leases. Existing render graph
    /// reservations are entered here too; they are not a hardware partition.
    /// Published channels remain retained until verified detach or CPU stop.
    channel_ids: crate::g17_channel::ChannelTable,
    /// Host channel identities never alias a previous graph within a session.
    next_channel_identity: u64,
    /// End timestamp of the last completion already handed to a waiter. The
    /// KSM record is a single overwritten slot, so without this the record
    /// left behind by submission N is indistinguishable from submission N+1's
    /// and every later wait would be satisfied instantly by stale data.
    last_completion_end: u64,
    /// Preallocated per-QID index for one shared completion-ring observation.
    /// Kept in the heap-resident manager so event service neither allocates
    /// under the runtime mutex nor consumes several KiB of kernel stack.
    compute_completion_batch: g17_resources::G17PComputeCompletionBatch,
    /// Set once a submission has zeroed the queue's completion stamp, so a
    /// retire knows the word is meaningful. Cleared only by rebuilding the
    /// manager.
    compute_stamp_armed: bool,
    /// How many retires have actually had to wait for the completion stamp.
    /// Counted rather than logged per submission, because a per-submission
    /// console write costs about a millisecond on a dockchannel console and
    /// would pad the very timing this gate exists to measure.
    compute_stamp_waits: AtomicU32,
    /// How many retires the completion stamp never arrived for. Hardware says
    /// this is a real minority case (19 of 300), not a modelling error, so the
    /// warning is rate-limited rather than removed: a per-submission console
    /// write costs about a millisecond and would itself distort the timing.
    compute_stamp_misses: AtomicU32,
}

/// Everything reachable by one physical TA/3D pair's submitted work. One of
/// these per pair; the manager's render methods operate on the pair its
/// cursor selects (`G17PRenderSlots`).
#[cfg(not(test))]
#[derive(Default)]
struct G17PRenderSlotState {
    /// The first-render archive once a render on this pair prepared it.
    #[cfg(CONFIG_DEV_COREDUMP)]
    trace_capture: Option<KBox<G17PTraceDump>>,
    // Rust drops fields in declaration order. Retract both roots before any
    // storage/SKSM backing reachable through them can be released.
    bind: Option<G17PRenderRootOwner>,
    /// Root replaced by an in-place same-VM context update. The replacement
    /// descriptor is not a release witness until the pair carrying that update
    /// completes and its EventControl record is unlinked.
    context_retirement: Option<G17PRenderContextRetirement>,
    /// Native secure-GART view of the active render process resources in
    /// hardware context 0. It is installed only for the VM which owns the
    /// retained render graph. Replaced only after paired completion plus
    /// FList release, or after both firmware CPUs stop.
    context_aliases: Option<mmu::T8140NativeContextAliases>,
    storage: Option<KBox<g17_resources::G17PUserRenderStorage>>,
    /// Storage is owned by a host preparer; completion/grow/teardown must defer.
    preparing: bool,
    restaged: Option<G17PRenderReservation>,
    /// The scheduler-state page of the last owner whose kicks were published
    /// on this pair. Its `0x14` release may already have been acknowledged,
    /// but the pair's installed QIDs still name it until the next owner's
    /// context update replaces it, so it stays mapped until then. Replaced,
    /// never accumulated: one page per pair at most.
    channel_control: Option<Arc<g17_resources::G17PClChannelControl>>,
    /// The SKSM half of the render path: one KSM kick queue per render data
    /// master. Present only while `g17p_render_sksm` is armed.
    sksm: Option<G17PRenderSksmQueues>,
    tracker: Option<G17PPartialOpeningRenderTracker>,
    queued_tracker: g17_completion::PairFifo<G17PPartialOpeningRenderTracker, { g17_resources::G17P_RENDER_QUEUED_PER_PAIR }>,
    submission_ordinal: u64,
    /// Per-command host timestamp shared by the TA and 3D KSM entries. Native
    /// prepares it once in the combined render descriptor, before either
    /// stage wrapper encodes its entry.
    auxiliary_timestamp: u64,
    /// ID currently referenced by the submitted TA command. Native releases
    /// this from `AGXTACommandDescriptor::complete`; Linux's paired completion
    /// is the first safe semantic completion point currently exposed here.
    hardware_buffer_id: Option<u32>,
    /// Set immediately after the outer command pair becomes firmware-visible.
    /// A pre-publication construction failure can return its ID immediately;
    /// a post-publication failure must retain it until the CPUs stop.
    hardware_buffer_published: bool,
    /// Pool identity follows the graph even while its storage is borrowed.
    flist_index: usize,
    /// Unpublished snapshot; read, encode and commit stay under Runtime.
    prepared_render_flist: Option<G17PBootstrapUmaLifecycle>,
    /// Exact publication membership: another pair's references cannot satisfy
    /// this pair's completion, nor can duplicate retirement consume its next.
    flist_leases: g17_completion::PairFifo<(u64, u64, u32), { g17_resources::G17P_RENDER_QUEUED_PER_PAIR + 1 }>,
    flist_retired_ordinal: Option<u64>,

}

#[cfg(not(test))]
impl G17PRenderSlotState {
    fn new() -> Result<KBox<Self>> {
        KBox::init(try_init!(Self {
            #[cfg(CONFIG_DEV_COREDUMP)]
            trace_capture: None,
            bind: None,
            context_retirement: None,
            context_aliases: None,
            storage: None,
            preparing: false,
            restaged: None,
            channel_control: None,
            sksm: None,
            tracker: None,
            queued_tracker: g17_completion::PairFifo::new(),
            submission_ordinal: 0,
            auxiliary_timestamp: 0,
            hardware_buffer_id: None,
            hardware_buffer_published: false,
            flist_index: 0,
            prepared_render_flist: None,
            flist_leases: g17_completion::PairFifo::new(),
            flist_retired_ordinal: None,
        }), GFP_KERNEL)
    }
}

#[cfg(not(test))]
pub(crate) struct G17PRenderDependencies {
    pub(crate) entries: [g17_submission::G17PClBarrierDependency;
        g17_submission::G17P_CL_EXPLICIT_DEPENDENCIES_MAX],
    pub(crate) count: usize,
}

#[cfg(not(test))]
impl G17PRenderDependencies {
    pub(crate) const EMPTY_ENTRY: g17_submission::G17PClBarrierDependency =
        g17_submission::G17PClBarrierDependency {
            native_timestamp: None, event_word: 0, shared_stamp: 0, queue_id: 0,
        };

    pub(crate) const fn empty() -> Self {
        Self { entries: [Self::EMPTY_ENTRY; g17_submission::G17P_CL_EXPLICIT_DEPENDENCIES_MAX], count: 0 }
    }
}

#[cfg(not(test))]
#[derive(Clone, Copy)]
struct G17PRenderPreparedSksm {
    descriptor_contexts: [u32; 2],
    before: [g17_submission::G17PClKickProducerState; 2],
    after: [g17_submission::G17PClKickProducerState; 2],
}

/// Exact host reservation for one unpublished command. Firmware cookies are
/// not host reservation IDs; the pair and monotonic ordinal identify this lease.
#[cfg(not(test))]
struct G17PRenderReservation {
    hardware_buffer_id: u32,
    parameter_buffer_token: u64,
    command_buffer_state_word: u32,
    descriptor_context_id: u16,
    execution_context: Arc<g17_resources::G17PLogicalQueueContext>,
    queue_pair: g17_resources::G17PRenderQueuePair,
    ordinal: u64,
    prepared_sksm: Option<G17PRenderPreparedSksm>,
    host_tvb_grow: Option<g17_resources::G17PHostTvbGrow>,
}

/// An installed graph remains owned while its next generation is prepared.
/// The caller must return it under Runtime before releasing its epoch lease,
/// including failed/cancelled preparation: older firmware work still names it.
#[cfg(not(test))]
pub(crate) struct G17PRenderRestage {
    work_state: Arc<g17_resources::G17PWorkStateLease>,
    storage: KBox<g17_resources::G17PUserRenderStorage>,
    reservation: G17PRenderReservation,
    num_clusters: u32,
    uat: Arc<KBox<mmu::Uat>>,
    producers: Option<[g17_submission::G17PClKickProducerState; 2]>,
    dependencies: KBox<G17PRenderDependencies>,
}

#[cfg(not(test))]
impl G17PRenderRestage {
    pub(crate) fn prepare(&mut self, dev: &AsahiDevice, vm: &mmu::Vm,
        command: &g17_uapi::TranslatedRenderCommand,
        stages: &mut crate::g17_perf::RenderStageTimes) -> Result {
        if let Some(grow) = self.reservation.host_tvb_grow.as_mut() {
            grow.grow(dev, &mut self.storage, vm)?;
        }
        let r = &self.reservation;
        let mut command = *command;
        command.parameters.ta_hardware_buffer_id = r.hardware_buffer_id;
        command.parameters.context_generation = r.execution_context.generation();
        let page_lists = self.storage.parameter_buffer_page_list_vas()?;
        let started = stages.enabled.then(kernel::time::Instant::<kernel::time::Monotonic>::now);
        let result = self.storage.restage_command(dev, vm, r.descriptor_context_id,
            page_lists, &command, self.num_clusters,
            [r.command_buffer_state_word; 2], r.ordinal, true);
        if let Some(started) = started { stages.restage_ns = started.elapsed().as_nanos().max(0) as u64; }
        stages.retained = true;
        result?;
        // The pair and this exact maintenance lease are pinned by the epoch.
        // Context-local TSQ assignment and descriptor writes need no Runtime.
        self.storage.bind_shared_work_state(&self.work_state)?;
        let started = stages.enabled.then(kernel::time::Instant::<kernel::time::Monotonic>::now);
        let unreachable = self.storage.first_unreachable_cached(&self.uat, vm);
        if let Some(started) = started {
            stages.reachability_ns = started.elapsed().as_nanos().max(0) as u64;
            stages.reachability_cache_hit = self.storage.last_reachability_cache_hit();
        }
        if let Some(failed) = unreachable {
            self.storage.log_reachability(dev, &self.uat, vm);
            dev_err!(dev.as_ref(), "G17P render preparation: unreachable storage predicate {}\n", failed);
            return Err(EFAULT);
        }
        if let Some(mut before) = self.producers {
            let started = stages.enabled.then(kernel::time::Instant::<kernel::time::Monotonic>::now);
            let qos_id = u8::try_from(self.storage.qos_hardware_buffer_id()?).map_err(|_| EOVERFLOW)?;
            for producer in &mut before { producer.qos.queue_byte_32 = qos_id; }
            let fragment_barrier = [g17_submission::g17p_render_barrier_from_timestamp(
                before[0].current_timestamp, before[0].queue_id,
            ).map_err(|_| EINVAL)?];
            let fragment_barriers = if crate::g17_config::RENDER_NO_BARRIER != 0 {
                &[][..]
            } else { &fragment_barrier[..] };
            let (fragment_queue, tiling_queue) = self.storage.queue_gpu_vas();
            let mut after = before;
            after[1] = prepare_render_sksm_entry(&mut self.storage, dev, false,
                before[1], r.ordinal, G17P_NATIVE_RENDER_KICK_AUXILIARY,
                fragment_queue, fragment_barriers)?;
            after[0] = prepare_render_sksm_entry(&mut self.storage, dev, true,
                before[0], r.ordinal, G17P_NATIVE_RENDER_KICK_AUXILIARY,
                tiling_queue, &self.dependencies.entries[..self.dependencies.count])?;
            // No producer changes until both entries are complete. Cancellation
            // leaves only unreachable entry bytes, reusable at the same cursor.
            let descriptor_contexts = self.storage.descriptor_context_ids()?;
            self.reservation.prepared_sksm = Some(G17PRenderPreparedSksm {
                before, after, descriptor_contexts,
            });
            if let Some(started) = started {
                stages.restage_ns = stages.restage_ns.saturating_add(
                    started.elapsed().as_nanos().max(0) as u64);
            }
        }
        Ok(())
    }
}

/// One firmware-requested grow owns its graph until reply commit. The epoch
/// pins the session while the separate allocation worker releases Runtime.
#[cfg(not(test))]
pub(crate) struct G17PRenderGrow {
    request: g17_completion::G17PFirmwareTvbGrowEvent,
    pair_slot: Option<u8>,
    storage: Option<KBox<g17_resources::G17PUserRenderStorage>>,
    vm: Option<mmu::Vm>,
}

#[cfg(not(test))]
impl G17PRenderGrow {
    pub(crate) fn grow(&mut self, dev: &AsahiDevice) -> Result<bool> {
        g17_resources::G17ResourceOwner::grow_render_tvb_offlock(
            dev, self.storage.as_deref_mut().ok_or(ENOENT)?, self.vm.as_ref().ok_or(ENOENT)?)
    }
}

/// One idle pair's cold construction. The epoch is held by the caller;
/// cancellation returns its retained SKSM backing before dropping the graph.
#[cfg(not(test))]
pub(crate) struct G17PWarmRenderGraph {
    pub(crate) installed: bool,
    graph: Option<g17_resources::G17PUninstalledRenderGraph>,
    retained_sksm_backing: Option<g17_resources::G17PRenderSksmBacking>,
    parameter_metrics: Option<g17_resources::G17PPmPageMetricsBinding>,
    pub(crate) queue_pair: g17_resources::G17PRenderQueuePair,
    ordinal: u64,
    hardware_buffer_id: Option<u32>,
    pm_generation: Option<u64>,
    context: Arc<g17_resources::G17PLogicalQueueContext>,
    work_state: Arc<g17_resources::G17PWorkStateLease>,
    vm: mmu::Vm,
    command: g17_uapi::TranslatedRenderCommand,
    num_clusters: u32,
    uat: Arc<KBox<mmu::Uat>>,
}

#[cfg(not(test))]
impl G17PWarmRenderGraph {
    #[inline(never)]
    pub(crate) fn build(&mut self, dev: &AsahiDevice) -> Result {
        let context_id = u16::try_from(self.context.context_id()).map_err(|_| EOVERFLOW)?;
        let root_slot = u8::try_from(self.context.context_id()).map_err(|_| EOVERFLOW)?;
        let word = self.work_state.word();
        if word == u32::MAX { return Err(EINVAL); }
        // Built in place in this heap-allocated request, its storage in the
        // box that becomes the slot's storage: no stack copy.
        let storage_box = KBox::new_uninit(GFP_KERNEL)?;
        g17_resources::G17PUninstalledRenderGraph::build(
            dev, &self.vm, self.uat.clone(), self.parameter_metrics.take().ok_or(EIO)?,
            self.queue_pair, Some(self.context.clone()), context_id, Some(root_slot),
            &self.command, self.num_clusters, [word; 2], self.ordinal,
            self.pm_generation, &mut self.retained_sksm_backing, storage_box, &mut self.graph,
        )?;
        Ok(())
    }
}

/// The exact front command's graph, borrowed only for CPU completion reads.
/// Other pairs can prepare, publish and retire while this owner is observed.
/// Return the graph before testing errors or the preparation epoch: firmware
/// may still own it, including when the observation itself fails.
#[cfg(not(test))]
pub(crate) struct G17PRenderObservation {
    storage: KBox<g17_resources::G17PUserRenderStorage>,
    pub(crate) pair_slot: u8,
    ordinal: u64,
    windows: [g17_completion::RenderQueueWindow; 2],
    tiling_previous: g17_completion::QueueIndices,
    fragment_previous: g17_completion::QueueIndices,
}

#[cfg(not(test))]
impl G17PRenderObservation {
    pub(crate) fn observe(&mut self) -> Result<(
        g17_resources::G17PUserRenderQueueState,
        g17_completion::PairedRenderCompletion,
    )> {
        // The same bounded retry used by the locked path: separately sampled
        // firmware cursors may straddle an update. No wait, sleep or mutation.
        for _ in 0..2 {
            fence(Ordering::Acquire);
            let state = self.storage.snapshot(self.ordinal)?;
            let observed = g17_completion::observe_paired_render_windows_depth(
                self.windows, g17_resources::G17P_RENDER_MAX_IN_FLIGHT as u32,
                self.tiling_previous, state.tiling,
                self.fragment_previous, state.fragment,
                g17_completion::RenderExecutionState {
                    job_list_empty: state.job_list_empty,
                    tiling_timestamps: state.tiling_timestamps,
                    fragment_timestamps: state.fragment_timestamps,
                },
            );
            if let Ok(completion) = observed { return Ok((state, completion)); }
        }
        Err(EAGAIN)
    }
}

/// Encode and write one reserved pair's entry without a device-global lock.
/// The caller owns the graph and the exact physical producer through commit.
#[cfg(not(test))]
#[inline(never)]
fn prepare_render_sksm_entry(
    storage: &mut g17_resources::G17PUserRenderStorage, dev: &AsahiDevice,
    tiling: bool, producer: g17_submission::G17PClKickProducerState,
    ordinal: u64, auxiliary_timestamp: u64, queue_record_gpu_va: u64,
    barriers: &[g17_submission::G17PClBarrierDependency],
) -> Result<g17_submission::G17PClKickProducerState> {
    // Mode 2 publishes a bare entry -- no RCE bindings or event mask -- as
    // a retained diagnostic. The default is the byte-exact render shape
    // recovered from the valid cold macOS SKSM entry-ring capture.
    let sksm_mode = crate::g17_config::RENDER_SKSM;
    let minimal = sksm_mode == 2;
    // Mode 3 is the shape a real Metal render pass uses, read out of the
    // macOS witnesses (recs2048/recs4096/recsmax-gfx.txt):
    //
    //   AGFIKSMEventFlagMask ... 32 bytes, ALL ZERO on both queues
    //   AGXKSMKickQueueEntryRCEInfo:
    //     TA  kind 0, one RegisterArray at descriptor +0x60, tag 0x47
    //     3D  kind 0, four descriptor-embedded RegisterArrays at
    //         descriptor +0xa0/+0x7c0/+0xee0/+0x1600,
    //         tags 0x56/0x10/0x17/0x0a
    //
    // RCEInfo +0x04 contains the active array count (1/4); native takes the
    // kind from `(RCEInfo +0x00) & 3`, which is zero in every render dump.
    // TA and 3D are both descriptor-local. 3D's +0x7a0/+0xec0/+0x15e0/
    // +0x1d00 words are each phase's +0x700 self-pointer trailer, not
    // pointers to an external allocation. Mode 4 remains an alias for the
    // old diagnostic command line, but emits this corrected native shape.
    let native_shaped = sksm_mode == 3 || sksm_mode == 4;
    let descriptor_high = storage.descriptor_high_va(tiling)?;
    let descriptor_low = storage.descriptor_low_va(tiling)?;
    let work_items = storage.work_item_gpu_vas(tiling)?;
    // Native mcacheApertureBufferSetup supplies the KSM aperture from the
    // descriptor table: host FRG +0xa98/+0xaa0/+0xaa4 hold its pointer,
    // context and count. encode3DKickEntry consumes that exact structure.
    // The count is +0xaa4, not +0xaa8. Saved final native benchmark commands
    // have pointer/count zero, even though old table bytes remain nonzero;
    // see NATIVE-COMPLETE-RENDER-COMPARISON.md. General aperture/coherency
    // policy remains separate from the explicitly opt-in benchmark probe.
    let mcache = if crate::g17_config::RENDER_KSM_MCACHE == 0
        || crate::g17_timing_buffer::diag_knob(crate::g17_timing_buffer::DIAG_NO_KSM_MCACHE)
    {
        None
    } else if tiling {
        storage.tiling_mcache()
    } else {
        storage.fragment_mcache()
    };
    let empty_binding = g17_submission::G17PClRceBinding { address: 0, tag: 0 };
    let command_state = if minimal {
        g17_submission::G17PComputeClCommandState {
            event_mask: [0; 4],
            rce_bindings: [empty_binding; 4],
        }
    } else if native_shaped {
        g17_submission::prepare_g17p_render_cl_command_state(
            descriptor_low,
            tiling,
        )
            .map_err(|_| G17PManagerConstruction::render_stage_fail("sksm-render-command-state", EINVAL))?
    } else {
        g17_submission::prepare_g17p_compute_cl_command_state(descriptor_low, ordinal, 0x22)
            .map_err(|_| G17PManagerConstruction::render_stage_fail("sksm-command-state", EINVAL))?
    };

    let prepared = g17_submission::prepare_g17p_cl_kick_entry(
        producer,
        g17_submission::G17PClKickEntryOperands {
            // These are host-side predicates, not firmware-descriptor
            // bytes. Bit1 is 3D-descriptor-specific; bit61 is a shared
            // device-global mode whose captured/default value is clear.
            // Keep both independently bisectable until their semantic
            // sources are modeled instead of forced.
            descriptor_flag_4c: !tiling
                && (crate::g17_config::RENDER_HEADER_FLAGS & 1 != 0),
            descriptor_flag_5c8:
                crate::g17_config::RENDER_HEADER_FLAGS & 2 != 0,
            converted_command_timestamp: producer.current_timestamp,
            barriers,
            mcache,
            payload: [descriptor_high, queue_record_gpu_va],
            event_mask: command_state.event_mask,
            rce_kind: 0,
            rce_bindings: command_state.rce_bindings,
            auxiliary: auxiliary_timestamp,
        },
    )
    .map_err(|_| G17PManagerConstruction::render_stage_fail("sksm-kick-entry", EINVAL))?;
    storage
        .write_sksm_entry(
            tiling,
            prepared.entry_offset as usize,
            prepared.zero_length as usize,
            &prepared.entry,
        )
        .map_err(|error| G17PManagerConstruction::render_stage_fail("sksm-write-entry", error))?;
    fence(Ordering::SeqCst);
    let stamp = prepared.current_timestamp;
    let mut producer = prepared.producer_after;
    producer.last_add_kicks_timestamp = stamp;
    if crate::util::g17p_verbose() {
        dev_info!(
            dev.as_ref(),
            "G17P render SKSM: {} qid {} entry_offset {:#x} entry_low {:#x} descriptor=[{:#x},{:#x}] items=[{:#x},{:#x},{:#x}] stamp {:#x} barriers {} mcache-ranges {} submission {}\n",
            if tiling { "TA" } else { "3D" },
            producer.queue_id,
            prepared.entry_offset,
            storage.sksm_entry_low_va(tiling),
            descriptor_high,
            descriptor_low,
            work_items[0],
            work_items[1],
            work_items[2],
            stamp,
            barriers.len(),
            mcache.map_or(0, |value| value.count),
            ordinal
        );
    }
    Ok(producer)
}


/// The retained state of every physical render pair, and the cursor naming
/// the pair the manager's render methods operate on.
///
/// Slot 0 is allocated with the manager; every other slot is allocated when
/// it is first selected, and a slot that was never selected has no state to
/// inspect or retire. The selected slot is always allocated. Selection only
/// moves the cursor: no state is copied, and every pair's graph stays in
/// place while its independent queue pair executes.
#[cfg(not(test))]
struct G17PRenderSlots {
    slots: KVec<Option<KBox<G17PRenderSlotState>>>,
}

#[cfg(not(test))]
impl G17PRenderSlots {
    fn new() -> Result<Self> {
        let mut slots = KVec::with_capacity(g17_resources::G17PRenderQueuePair::SLOT_COUNT as usize, GFP_KERNEL)?;
        for _ in 0..g17_resources::G17PRenderQueuePair::SLOT_COUNT {
            slots.push(None, GFP_KERNEL)?;
        }
        slots[0] = Some(G17PRenderSlotState::new()?);
        Ok(Self { slots })
    }

    /// The explicitly named pair's state.
    fn get(&self, slot: u8) -> &G17PRenderSlotState {
        self.slots[usize::from(slot)]
            .as_deref()
            .expect("the requested render slot is allocated")
    }

    fn get_mut(&mut self, slot: u8) -> &mut G17PRenderSlotState {
        self.slots[usize::from(slot)]
            .as_deref_mut()
            .expect("the requested render slot is allocated")
    }

    /// EINVAL for a slot outside the pair space, ENODEV for an unallocated pair.
    fn allocated(&self, slot: u8) -> Result<&G17PRenderSlotState> {
        self.slots
            .get(usize::from(slot))
            .ok_or(EINVAL)?
            .as_deref()
            .ok_or(ENODEV)
    }

    /// Every allocated pair other than the explicitly named one, in pair order.
    fn parked(&self, slot: u8) -> impl Iterator<Item = &G17PRenderSlotState> {
        let active = usize::from(slot);
        self.slots
            .iter()
            .enumerate()
            .filter(move |(index, _)| *index != active)
            .filter_map(|(_, state)| state.as_deref())
    }

    fn parked_mut(&mut self, slot: u8) -> impl Iterator<Item = &mut G17PRenderSlotState> {
        let active = usize::from(slot);
        self.slots
            .iter_mut()
            .enumerate()
            .filter(move |(index, _)| *index != active)
            .filter_map(|(_, state)| state.as_deref_mut())
    }

    /// Take every other pair's state out, in pair order. The named pair stays.
    fn take_parked(&mut self, slot: u8) -> impl Iterator<Item = KBox<G17PRenderSlotState>> + '_ {
        let active = usize::from(slot);
        self.slots
            .iter_mut()
            .enumerate()
            .filter(move |(index, _)| *index != active)
            .filter_map(|(_, state)| state.take())
    }

    /// Every pair with its index, allocated or not, in pair order.
    fn iter_mut(&mut self) -> impl Iterator<Item = (u8, Option<&mut G17PRenderSlotState>)> {
        self.slots
            .iter_mut()
            .enumerate()
            .map(|(index, state)| (index as u8, state.as_deref_mut()))
    }

    /// Allocate a physical pair on first use, without changing any other pair.
    fn ensure(&mut self, slot: u8) -> Result {
        if slot >= g17_resources::G17PRenderQueuePair::SLOT_COUNT {
            return Err(EINVAL);
        }
        let index = usize::from(slot);
        if self.slots[index].is_none() {
            self.slots[index] = Some(G17PRenderSlotState::new()?);
        }
        Ok(())
    }

}

#[cfg(not(test))]
/// A render graph taken off its physical pair, whole, while its owner's
/// scheduler-state release is still outstanding. Dropped -- root first, then
/// the client aliases, then every kernel backing (TVB, RT memory, render
/// state, descriptors, PM, support, FList) -- once the firmware has consumed
/// that release.
pub(crate) struct G17PRetiredRenderGraph {
    // Rust drops fields in declaration order. Retract roots and their aliases
    // before releasing any storage reachable through them.
    bind: G17PRenderRootOwner,
    context_aliases: Option<mmu::T8140NativeContextAliases>,
    storage: KBox<g17_resources::G17PUserRenderStorage>,
}

/// Where one closed logical queue is in its firmware detach.
#[cfg(not(test))]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum G17PLogicalQueueTeardownPhase {
    /// The DRM queue is still open; only graphs it abandoned (a desync
    /// rebuild) are parked here, and they wait for the queue to close.
    Deferred,
    /// The DRM queue closed. Its scheduler-state release is published once
    /// every render pair that still carries its graph is idle, its FList
    /// release has been consumed, and no compute command of its is in flight.
    Draining,
    /// The `0x14` release record is in the primary control ring; the graph
    /// and the queue context are freed when the consumer passes
    /// `producer_after` (`g17p_control_ring_target_consumed`).
    Released { producer_after: u32 },
}

/// One closed logical queue's resources between queue close and the firmware
/// acknowledgement of its scheduler-state release -- the lifetime
/// `AGXCommandQueue::freeReclaimableResources` gives them on macOS:
/// `submitReleaseResource` (opcode `0x14`, identity bytes from the live
/// scheduler state, a synchronous wait for the control-ring consumer), then
/// the work queues and their parameter management are freed.
///
/// The `context` Arc pins the queue's execution context (VM root, scheduler
/// page, QoS lease) for exactly this window. Graphs still installed on a pair
/// stay there until the release is consumed, so a new queue in the same VM
/// can still retarget them in place; graphs a foreign successor displaced
/// from a pair wait in `graphs`. No entry survives the acknowledgement, and
/// nothing is ever retained until processor stop or forgotten on overflow.
#[cfg(not(test))]
pub(crate) struct G17PLogicalQueueTeardown {
    // Drop order: graphs (root, aliases, storage) before the context that
    // owns the root lease and the scheduler page.
    graphs: KVec<G17PRetiredRenderGraph>,
    context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
    descriptor_context_id: u16,
    phase: G17PLogicalQueueTeardownPhase,
    armed: kernel::time::Instant<kernel::time::Monotonic>,
    warned: bool,
}

#[cfg(not(test))]
impl G17PLogicalQueueTeardown {
    pub(crate) fn descriptor_context_id(&self) -> u16 { self.descriptor_context_id }
    pub(crate) fn graph_count(&self) -> usize { self.graphs.len() }

    /// Called only for acknowledged teardowns returned to the event worker,
    /// after it releases the runtime mutex. Exact identity permits a successor
    /// to install its own pool without a late cleanup removing its aliases.
    pub(crate) fn release_retired_vm_mappings(&self) {
        for graph in self.graphs.iter() {
            graph.bind.vm().clear_render_pool_mappings_if_owner(
                graph.storage.queue_pair().slot(), graph.storage.render_pool_id(),
            );
        }
    }
}

/// Logical queues can be created faster than firmware acknowledges their
/// releases; the QoS lease space bounds live contexts at 128, and a teardown
/// pins one, so this is the hard bound on outstanding teardowns.
#[cfg(not(test))]
const G17P_MAX_LOGICAL_QUEUE_TEARDOWNS: usize = 128;
/// A release the control-ring consumer has not passed after this long is
/// reported once. The resources stay retained: nothing is freed without the
/// firmware witness, and processor stop drains whatever is left.
#[cfg(not(test))]
const G17P_LOGICAL_QUEUE_RELEASE_WARN_NS: i64 = 2_000_000_000;
/// While a teardown is younger than this the event worker re-polls it at
/// millisecond cadence; older ones are re-checked on ordinary event turns.
#[cfg(not(test))]
pub(crate) const G17P_LOGICAL_QUEUE_TEARDOWN_POLL_NS: i64 = 2_000_000_000;

#[cfg(not(test))]
struct G17PRenderContextRetirement {
    /// One Arc here keeps the replaced TTBAT root installed. The storage's old
    /// command-context Arc may be replaced immediately because this owner
    /// remains until the superseding group completes.
    bind: G17PRenderRootOwner,
    _context_aliases: Option<mmu::T8140NativeContextAliases>,
    retire_after_context_epoch: u64,
}

#[cfg(not(test))]
enum G17PRenderRootOwner {
    Bootstrap(mmu::VmBind),
    Job {
        context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
    },
}

#[cfg(not(test))]
impl G17PRenderRootOwner {
    fn matches(&self, vm: &mmu::Vm) -> bool {
        match self {
            Self::Bootstrap(bind) => bind.matches(vm),
            Self::Job { context } => context.matches(vm),
        }
    }

    fn root(&self) -> u64 {
        match self {
            Self::Bootstrap(bind) => bind.root(),
            Self::Job { context } => context.root(),
        }
    }

    fn vm_id(&self) -> u64 {
        match self {
            Self::Bootstrap(bind) => bind.vm_id(),
            Self::Job { context } => context.vm_id(),
        }
    }

    fn vm(&self) -> &mmu::Vm {
        match self {
            Self::Bootstrap(bind) => bind.vm(),
            Self::Job { context } => context.vm(),
        }
    }

    fn bootstrap(&self) -> Option<&mmu::VmBind> {
        match self { Self::Bootstrap(bind) => Some(bind), Self::Job { .. } => None }
    }

    /// TTBAT application-GART context which owns this graph's VM root.
    ///
    /// Context 1 separately retains the global FList/operand closure. Command
    /// descriptors and MCache ranges name this queue-owned context.
    fn root_slot(&self) -> Result<u8> {
        let slot = match self {
            Self::Bootstrap(bind) => bind.slot(),
            Self::Job { context } => context.context_id(),
        };
        u8::try_from(slot).map_err(|_| EIO)
    }
}

#[cfg(not(test))]
impl G17PRenderSlotState {
    fn scheduler_active(&self) -> bool {
        self.preparing || self.storage.is_some()
            || self.tracker.is_some()
            || self.queued_tracker.is_some()
            || self.bind.is_some()
            || self.context_retirement.is_some()
            || self.submission_ordinal != 0
    }
}

/// A preallocated evidence buffer, not a live mapping exposed to readers.
/// `used` remains zero until the archive is sealed; the only export path takes
/// this owner out of the manager so later submits cannot overwrite evidence.
#[cfg(all(not(test), CONFIG_DEV_COREDUMP))]
pub(crate) struct G17PTraceDump {
    arena: KVVec<u8>,
    used: usize,
    phase_sequence: u64,
    prepared: bool,
    job_stamp: u64,
}

#[cfg(all(not(test), CONFIG_DEV_COREDUMP))]
impl G17PTraceDump {
    fn new(resources: &mut g17_resources::G17ResourceOwner) -> Result<KBox<Self>> {
        let mut arena = KVVec::from_elem(0, trace::DEFAULT_CAPACITY, GFP_KERNEL)?;
        let mut archive = TraceArchive::new(&mut arena).map_err(|_| EINVAL)?;
        if module_parameters::g17p_binary_capture.value() & 2 != 0 {
            // HAZARD: mapped firmware TEXT reads can still panic hardware.
            // Mapping/attribute checks are not a safety proof. Leave this
            // explicit diagnostic off; prefer existing offline captures.
            // Inspect only the mapped code window preceding the established
            // GFX DATA link base. The read-only walker rejects unmapped pages
            // and non-memory attributes; never guess its physical address.
            // This boot-time diagnostic may allocate/map, never retirement.
            let mut meta = trace::Meta::new(
                trace::Kind::FirmwareCodeWindow, trace::Phase::BootConstructed, 1,
            );
            meta.context = 0;
            meta.root = trace::ROOT_CONTEXT_HIGH;
            meta.dva = 0xffff_fc00_0000_0000;
            meta.expected_len = 0x4_c000;
            let reader = resources.uat().firmware_context_table_reader();
            let root = reader.context0_upper_root();
            archive.capture(meta, &mut || Monotonic::ktime_get() as u64, |output| {
                let root = root.ok_or(trace::Omission::Unavailable)?;
                let bytes = root.copy_firmware_range(0, output.len())
                    .map_err(|_| trace::Omission::ReadError)?;
                output.copy_from_slice(&bytes);
                Ok(())
            }).map_err(|_| EINVAL)?;
        }
        if resources.capture_owned_trace(
            &mut archive, trace::Phase::BootConstructed, 1,
            &mut || Monotonic::ktime_get() as u64,
        ).is_err() {
            archive.omit(
                trace::Meta::new(trace::Kind::CaptureBoundary, trace::Phase::BootConstructed, 1),
                trace::Omission::ReadError,
            ).map_err(|_| EINVAL)?;
        }
        Ok(KBox::new(Self {
            arena, used: 0, phase_sequence: 1, prepared: false, job_stamp: 0,
        }, GFP_KERNEL)?)
    }

    pub(crate) fn len(&self) -> usize { self.used }
}

#[cfg(all(not(test), CONFIG_DEV_COREDUMP))]
impl kernel::devcoredump::DevCoreDump for G17PTraceDump {
    fn read(&self, output: &mut [u8], offset: usize) -> Result<usize> {
        // No caller is allowed to expose an in-progress archive.
        if self.used == 0 { return Err(EINVAL); }
        trace::read_sealed_prefix(&self.arena[..self.used], output, offset).map_err(|_| EINVAL)
    }
}

/// One record a steady-state submission publishes into the item ring. Every
/// record body lives in its own per-publication storage (the descriptor slot,
/// or one of the queue's channel-record slots), so the ring carries pointers.
enum G17PRepeatRecord {
    Pointer(u64),
}

/// Records one steady-state submission appends to the queue's item ring:
/// the tag-3 command pointer, tag 15, tag 14, tag 16.
const G17P_COMPUTE_REPEAT_RING_RECORDS: u32 = 4;
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum G17PSksmQueueHardwareState {
    PreparedMemory,
    HardwareRegistered,
}

/// One render KSM kick queue -- the SKSM half of a render data master.
///
/// The two render queues are QID 0 (TA, data master 0) and QID 1 (3D, data
/// master 1), a consecutive TA-first pair exactly as the twelve captured macOS
/// render passes use. Each one's entry ring is the fixed 0x20000 bootstrap
/// context peer whose low VA the render tag-15 ConfigUpdate already publishes
/// at +0x08.
#[cfg(not(test))]
struct G17PRenderSksmQueue {
    queue_id: u8,
    /// True for the TA/tiling queue, false for the 3D/fragment queue. Selects
    /// which bootstrap context peer backs the entry ring.
    tiling: bool,
    entry_low: u64,
    entry_high: u64,
    config: g17_submission::PreparedG17PSksmQueueConfig,
    producer: g17_submission::G17PClKickProducerState,
    /// Set only after the tag-15 install and complete outer group are visible
    /// to firmware. It never represents a host MMIO write.
    registered: bool,
    /// The KSMFE already has this QID enabled (a host direct configure landed
    /// this session). Native
    /// `configureHardware` tracks the same thing in `AGXKSMKickQueue+0xa8`
    /// and writes the disable mask before reprogramming such a queue.
    hardware_enabled: bool,
}

/// The render pair, TA first. Held next to `render_storage` and released with
/// it, since the entry rings live in resources the storage object aliases.
#[cfg(not(test))]
struct G17PRenderSksmQueues {
    scratch: G17SksmScratchGeometry,
    uat_owner: u64,
    tiling: G17PRenderSksmQueue,
    fragment: G17PRenderSksmQueue,
    /// Present only between retired client graphs. KSMFE retains these ring
    /// addresses for the lifetime of the QID pair, so a handoff may replace
    /// their client-low aliases but never their backing/global aliases.
    backing: Option<g17_resources::G17PRenderSksmBacking>,
}

/// How many times the device-global parameter-buffer loader has been handed
/// from one client's retained storage to another's. Rate-limits its own log.
static G17P_PARAMETER_BUFFER_RETARGETS: AtomicU32 = AtomicU32::new(0);

/// Session-wide counters for the four decisive state-machine events.
///
/// Printed on EVERY `G17PMARK` line. The kernel log ring wraps easily on a
/// verbose boot -- especially with `g17p_fw_trace=1` -- and the two rarest
/// markers (`tag15-install`, fired once at the very first submit of a boot)
/// are exactly the ones that get evicted first. Carrying the counts on every
/// marker line means any single surviving line reconstructs the whole state
/// machine: how many QIDs were installed, how many zero-duration retires were
/// caught, how many sessions were rebuilt, how many handoffs retained.
/// One `context/qos` control sample per boot, taken from a render that
/// completed, so the wedge sample has something to be compared against.
static RENDER_CONTEXT_WITNESS_LOGGED: AtomicBool = AtomicBool::new(false);
pub(crate) static G17P_MARK_TAG15_INSTALLS: AtomicU32 = AtomicU32::new(0);
pub(crate) static G17P_MARK_ZERO_DURATION: AtomicU32 = AtomicU32::new(0);
/// Compute kicks retired on firmware counters and stamp because their KSM
/// completion record was overwritten before any host scan latched it.
pub(crate) static G17P_COMPUTE_RECORDLESS_RETIRES: AtomicU32 = AtomicU32::new(0);
pub(crate) static G17P_MARK_GATE_REBUILDS: AtomicU32 = AtomicU32::new(0);
pub(crate) static G17P_MARK_RETAINED_HANDOFFS: AtomicU32 = AtomicU32::new(0);

/// Compact `i/z/r/h` census appended to every marker line.
pub(crate) fn g17p_mark_counts() -> (u32, u32, u32, u32) {
    (
        G17P_MARK_TAG15_INSTALLS.load(Ordering::Relaxed),
        G17P_MARK_ZERO_DURATION.load(Ordering::Relaxed),
        G17P_MARK_GATE_REBUILDS.load(Ordering::Relaxed),
        G17P_MARK_RETAINED_HANDOFFS.load(Ordering::Relaxed),
    )
}

/// First compute QID of a boot. The captured native CL2 queue uses QID 4, and
/// every proven run of this driver has used it.
pub(crate) const G17P_COMPUTE_QUEUE_ID_BASE: u8 = 4;
/// Last QID the allocator may hand out. `KICK_QID_MASK` is 127 and the
/// firmware's KSM restore set has 128 slots. IDs 0..3 are the two bounded
/// render pairs, 4 is the first compute queue, and additional compute graphs
/// consume 5..127 exactly once. Exhaustion returns ENOSPC rather than
/// reprogramming a queue still installed in the firmware. New compute graphs
/// leave `g17_channel::COMPUTE_RENDER_RESERVE` QIDs to render pairs.
pub(crate) const G17P_COMPUTE_QUEUE_ID_LAST: u8 = 127;
const _: () = assert!(G17P_COMPUTE_QUEUE_ID_BASE > 1);
const _: () = assert!(G17P_COMPUTE_QUEUE_ID_LAST <= 127);

/// The native bootstrap graph owns QIDs 0/1. Every logical engine channel
/// subsequently leases its ID from the same pool, without engine partitions.
fn g17p_reserve_bootstrap_channels(table: &mut crate::g17_channel::ChannelTable) -> bool {
    use crate::g17_channel::{ChannelId, ChannelTable, Engine};
    for qid in 0..2 {
        let engine = if qid & 1 == 0 { Engine::Tiling } else { Engine::Fragment };
        let Some(lease) = table.acquire(ChannelId(u64::from(qid) + 1), engine, Some(qid)) else {
            return false;
        };
        // Retain even an unsubmitted bootstrap reservation until processor stop.
        if !table.publish(lease) {
            return false;
        }
    }
    true
}

fn g17p_initial_channel_table() -> crate::g17_channel::ChannelTable {
    let mut table = crate::g17_channel::ChannelTable::new();
    // The empty table has 128 slots and these two identities are unique.
    assert!(g17p_reserve_bootstrap_channels(&mut table));
    table
}

/// A retained compute queue whose backing graph is prepared during startup.
/// Native processComputeSetup marks the queue dirty; the first direct
/// submitBuffer configures QID 4 after tag 15, once startup is complete.
#[cfg(not(test))]
pub(crate) struct G17PSksmQueue {
    queue_id: u8,
    channel_lease: crate::g17_channel::Lease,
    /// Bootstrap QoS identity, used only before a logical owner is attached.
    hardware_buffer_id: u8,
    scratch: G17SksmScratchGeometry,
    uat_owner: u64,
    configure_pair: g17_submission::G17SksmOrderedWritePair,
    enable_pair: g17_submission::G17SksmOrderedWritePair,
    disable_pair: g17_submission::G17SksmOrderedWritePair,
    hardware_state: G17PSksmQueueHardwareState,
    storage: g17_resources::G17PClRuntimeStorage,
    /// Current logical owner's scheduler page; bootstrap queues initially
    /// reference the dedicated session page. Neither belongs to this QID.
    channel_control_gpu_va: u64,
    logical_channel_control: Option<Arc<g17_resources::G17PClChannelControl>>,
    logical_qos: Option<Arc<g17_resources::G17PLogicalQosLease>>,
    compute: g17_resources::G17PComputeDescriptorStorage,
    producer: g17_submission::G17PClKickProducerState,
    direct_lifecycle: g17_submission::G17PDirectClLifecycleState,
    /// Updated only after the complete ConfigUpdate group is published.
    /// Preparing/reverting a logical-owner handoff never changes this cache.
    published_config: Option<g17_compute::ComputeConfigIdentity>,
    /// Native tracks the first outer publication on the selected AGXChannel.
    /// This state belongs to the logical command queue, not the session-wide
    /// CL_2 transport ring.
    outer_channel_started: bool,
    /// Native configureHardware compares the accelerator's retained QID stamp
    /// against EventMachine::getLastSubmittedStamp before touching MMIO.
    configure_guard_accelerator_stamp: u32,
    configure_guard_event_stamp: u32,
    submission_ordinal: u32,
    independent_context: bool,
    completion_descriptor: u64,
    completion_last_end: u64,
    completion_stamp_armed: bool,
    /// Diagnostic-only copy taken before the command pointer is published.
    /// Default-off so production submission has no extra reads or storage
    /// traffic. The paired FList array covers `+0x08` and `+0x48`.
    publication_resource_snapshot: Option<(
        g17_resources::G17PComputeSubmissionSnapshot,
        [u32; 28],
    )>,
}

/// Immutable identity of one firmware-visible compute publication.
///
/// The retained graph may change logical owners only after every firmware
/// cursor owned by this exact publication has reached its terminal value.
/// Keeping this separate from the queue's mutable producer state prevents a
/// later owner from accidentally satisfying an older completion check.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17PComputeRetirementTicket {
    queue_id: u8,
    descriptor: u64,
    tsq_node: u64,
    /// The kick's 40-bit queue timestamp: its completion-ring identity and
    /// the sequence of the per-kick stamp value its descriptor carries.
    kick_timestamp: u64,
    submitted_target: u32,
    item_producer: u32,
    item_count: u32,
}

/// Whether a finished kick with no latched completion record may retire on
/// firmware counters and its stamp alone.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum G17PComputeRecordless {
    /// No recovery since publication and the caller's grace has passed.
    Allowed,
    /// Keep waiting for the record (or the caller's next pass).
    NotYet,
    /// A recovery happened since publication: only a record can exclude a
    /// zero-duration retire, so a recordless finish fails submission-scoped.
    CrossedRecovery,
}

/// One exact retirement: the UAPI timestamp pair plus the raw firmware
/// observations the opt-in timing buffer records.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct G17PComputeRetired {
    /// Pair-A start and pair-B end: the compute UAPI timestamps.
    pub(crate) timestamps: [u64; 2],
    /// Pair A start/end and pair B start/end of the KSM completion record.
    pub(crate) pairs: [u64; 4],
    /// Host target and firmware completed counters at retirement.
    pub(crate) counters: [u32; 2],
    pub(crate) stamp: u32,
    pub(crate) in_flight_after: u32,
}

impl G17PComputeRetirementTicket {
    pub(crate) fn descriptor(&self) -> u64 { self.descriptor }
    pub(crate) fn kick_timestamp(&self) -> u64 { self.kick_timestamp }
    pub(crate) fn submitted_target(&self) -> u32 { self.submitted_target }
    pub(crate) fn item_producer(&self) -> u32 { self.item_producer }
}

impl G17PSksmQueue {
    /// Native `restartWorkQueue` recovery target for this CL queue's channel:
    /// uncached producer (+0x40) copied to +0x50 while firmware is halted.
    pub(crate) fn publish_recovery_target(&mut self) -> Result<u32> {
        self.compute.publish_recovery_target()
    }

    pub(crate) fn bind_logical_channel_control(
        &mut self, context: &g17_resources::G17PLogicalQueueContext,
    ) -> Result {
        // Apple selects the channel-control page from the command queue's
        // reclaimable resources. Keep that logical ownership even though the
        // queues share the physical CL_2 transport.
        let address = context.channel_control_gpu_va();
        self.compute.retarget_channel_control(address, context.owner_pid, context.tsq_head_gpu_va())?;
        self.channel_control_gpu_va = address;
        // The CL records now name this scheduler page; the queue's close must
        // release it through the firmware before it can be freed.
        context.channel_control.mark_published();
        self.logical_channel_control = Some(context.channel_control.clone());
        self.logical_qos = Some(context.qos.clone());
        self.producer.qos.queue_byte_32 = context.qos.id();
        Ok(())
    }

    pub(crate) fn qos_hardware_buffer_id(&self) -> u8 {
        self.logical_qos.as_ref().map_or(self.hardware_buffer_id, |qos| qos.id())
    }

    fn flist_generation(&self) -> u32 {
        self.logical_channel_control.as_ref().map_or(0x3f, |owner| owner.flist_generation())
    }

    /// The hardware compute QID this graph is published as.
    ///
    /// Needed outside this module so a client handoff can name the QID whose
    /// firmware-side progress record it has to restore.
    pub(crate) fn queue_id(&self) -> u8 {
        self.queue_id
    }

    /// Native stamp of the NEWEST published kick. Kicks of one QID retire in
    /// order through their implicit parent, so waiting on the newest also
    /// waits on every older kick still in flight.
    pub(crate) fn published_compute_dependency(
        &self,
    ) -> Result<g17_submission::G17PClBarrierDependency> {
        if self.producer.submitted == 0 || !self.producer.previous_valid {
            return Err(EIO);
        }
        let timestamp = self.producer.last_add_kicks_timestamp;
        let mut dependency = g17_submission::g17p_render_barrier_from_timestamp(
            timestamp, self.queue_id,
        ).map_err(|_| EIO)?;
        dependency.native_timestamp = Some(timestamp);
        Ok(dependency)
    }
    pub(crate) fn requires_first_install(&self) -> bool {
        self.hardware_state == G17PSksmQueueHardwareState::PreparedMemory
            && self.submission_ordinal == 0
    }

    /// `(submission ordinal, next kick stamp, item-ring producer)`.
    ///
    /// Sampled when the graph is handed from one DRM client to the next, so a
    /// log line proves the retained queue really did carry its stamp sequence
    /// and ring cursor across the handoff instead of restarting them. Every
    /// field is a plain read of an object the driver allocated -- no sgx MMIO,
    /// so it is safe with the GPU cores gated.
    pub(crate) fn handoff_snapshot(&mut self) -> (u32, u64, u32) {
        let ring = self.compute.compute_ring_producer().unwrap_or(u32::MAX);
        (self.submission_ordinal, self.producer.current_timestamp, ring)
    }
}

/// Command-owned fields used to build one compute descriptor and CL entry.
/// The queue supplies descriptor storage, sequencing, preemption storage, and
/// the retained support graph.
#[cfg(not(test))]
pub(crate) struct G17PComputeSubmissionContext<'a> {
    pub(crate) context_id: u32,
    pub(crate) work_state_word: u32,
    pub(crate) work_state: Option<&'a g17_resources::G17PWorkStateLease>,
    pub(crate) dispatch_identity: u64,
    pub(crate) timestamps: g17_uapi::ComputeTimestampAddresses,
    pub(crate) descriptor_flag_4c: bool,
    pub(crate) converted_command_timestamp: u64,
    pub(crate) barriers: &'a [g17_submission::G17PClBarrierDependency],
    pub(crate) mcache: Option<g17_submission::G17PClMcacheAperture>,
    pub(crate) rce_kind: u8,
    pub(crate) auxiliary: u64,
}

/// User-VM aliases and its active TTBAT binding. The live runtime retains this
/// object until the entry completes or both processors stop during recovery.
#[cfg(not(test))]
pub(crate) struct G17PComputeUserBinding {
    mappings: g17_resources::G17PComputeUserMappings,
    _flist: mmu::KernelMapping,
    _sksm_entries: mmu::KernelMapping,
    _vm_bind: Option<mmu::VmBind>,
    _execution_context: Option<Arc<crate::g17_resources::G17PLogicalQueueContext>>,
}

#[cfg(not(test))]
impl G17PComputeUserBinding {
    pub(crate) fn retarget_logical_context(
        &mut self, context: Arc<g17_resources::G17PLogicalQueueContext>,
    ) -> Result {
        if !self.matches(context.vm()) || !context.is_current() { return Err(EFAULT); }
        self._execution_context = Some(context);
        Ok(())
    }

    /// Whether this retained binding already covers `vm`.
    ///
    /// A repeat submission reuses the binding rather than mapping the same
    /// fixed user VAs a second time, which would fail; a different VM has to
    /// go through a fresh bind.
    pub(crate) fn matches(&self, vm: &mmu::Vm) -> bool {
        self._vm_bind.as_ref().is_some_and(|bind| bind.matches(vm))
            || self._execution_context.as_ref().is_some_and(|context| context.matches(vm))
    }

    /// The UAT binding this client's submissions run against.
    ///
    /// Needed so every kick -- not just the one that created this binding --
    /// can re-prove that hardware contexts 2/3 still name this VM's root.
    /// The hardware context this client's compute work actually runs in.
    ///
    /// The kick already declares it at `sgx+0x10428` as
    /// `(context_id << 8) | work_ordinal`; this exposes the same value so the
    /// tag-15 ConfigUpdate can declare it too instead of the constant 2.
    pub(crate) fn declared_context_id(&self) -> Option<u16> {
        self._execution_context
            .as_ref()
            .and_then(|context| u16::try_from(context.context_id()).ok())
    }

    fn context_generation(&self) -> u8 {
        self._execution_context.as_ref().map_or(1, |context| context.generation())
    }

    pub(crate) fn vm_bind(&self) -> &mmu::VmBind {
        self._vm_bind.as_ref().expect("legacy binding required")
    }
}

/// An outgoing client's compute binding, kept alive after its submission was
/// abandoned, with only the UAT slot given back.
///
/// A submission that was published and never seen to retire may still be held
/// by the firmware, and that kick names this client's page tables and the GEM
/// objects mapped into them. Dropping the binding outright would unmap and then
/// free exactly that memory. Every `mmu::KernelMapping` here owns an
/// `Arc<VmInner>`, so retaining them retains the page tables, the gpuvm and its
/// GEM references -- nothing the firmware could still dereference is released.
///
/// The `mmu::VmBind` is deliberately NOT retained. Slot release is refcounted
/// on the `VmBind` alone (`active_users` reaching zero clears `binding.binding`),
/// independently of the `VmInner` lifetime, so dropping it hands UAT slot 1
/// straight back to the next client while the memory stays parked.
///
/// Released only once both firmware processors have stopped, in
/// `release_runtime_objects` -- the same discipline `retired_queues` uses.
#[cfg(not(test))]
pub(crate) struct G17PQuarantinedComputeBinding {
    _mappings: g17_resources::G17PComputeUserMappings,
    _flist: mmu::KernelMapping,
    _sksm_entries: mmu::KernelMapping,
    _execution_context: Option<Arc<crate::g17_resources::G17PLogicalQueueContext>>,
}

#[cfg(not(test))]
impl G17PComputeUserBinding {
    /// Give the UAT slot back and park everything else.
    pub(crate) fn quarantine(self) -> G17PQuarantinedComputeBinding {
        let Self {
            mappings,
            _flist,
            _sksm_entries,
            _vm_bind,
            _execution_context,
        } = self;
        // Explicit and load-bearing: this is what frees UAT slot 1.
        drop(_vm_bind);
        G17PQuarantinedComputeBinding {
            _mappings: mappings,
            _flist,
            _sksm_entries,
            _execution_context,
        }
    }
}

#[cfg(not(test))]
pub(crate) struct G17PComputeTimeoutSnapshot {
    pub(crate) graph: g17_resources::G17PComputeGraphSnapshot,
    pub(crate) channel: g17_resources::G17PComputeChannelSnapshot,
    pub(crate) channel_scan:
        [g17_resources::G17PChannelScanSnapshot; g17_resources::G17P_DATA_MASTER_CHANNEL_COUNT],
    pub(crate) channel_control_prefix: [u8; 0x38],
    pub(crate) control: g17_resources::G17PControlCounters,
}

/// The two complete queue groups exposed by the exact first graphics publish.
#[cfg(not(test))]
pub(crate) struct G17PPartialOpeningRenderSubmission {
    pub(crate) fragment_queue_gpu_va: u64,
    pub(crate) fragment_queue_write_index: u32,
    pub(crate) tiling_queue_gpu_va: u64,
    pub(crate) tiling_queue_write_index: u32,
}

/// Completion cursors retained across observations of one graphics pair.
#[cfg(not(test))]
pub(crate) struct G17PPartialOpeningRenderTracker {
    ordinal: u64,
    hardware_buffer_id: u32,
    command_buffer_state_word: u32,
    work_state: Option<Arc<g17_resources::G17PWorkStateLease>>,
    /// Exact physical TA/fragment publication identities; host retirement may
    /// lag hardware by several commands when a recovery snapshot is taken.
    native_timestamps: Option<[u64; 2]>,
    // Only translated renders publish the paired QoS rows. Keep retirement
    // with this command generation so a later cleanup failure cannot make a
    // retry subtract the same QoS references twice.
    qos_pending: bool,
    pool_a_record_index: u8,
    pool_a_submitted: u32,
    context_update: bool,
    tiling_previous: g17_completion::QueueIndices,
    fragment_previous: g17_completion::QueueIndices,
    tiling_prefix: u32,
    fragment_prefix: u32,
    windows: [g17_completion::RenderQueueWindow; 2],
}

#[cfg(not(test))]

/// DIAGNOSTIC CONTROL, compute path only. When 0, the compute submit publishes
/// everything it normally does -- tag 3/15/14/16, the classic item-ring CL_2
/// command, the doorbell, the producer advance -- but never writes the SKSM
/// kick entry body. It answers one question about the CONTROL, not the failing
/// path: is compute actually scheduled through the SKSM entry ring, or through
/// the classic item ring with its SKSM entry vestigial?
///
/// This matters because "compute completes with `g17p_sksm_mmio=0`" was used to
/// dismiss the suppressed SKSM queue-enable as a render blocker. That argument
/// only carries if compute's completion depends on SKSM scheduling. If compute
/// still passes 64/64 with this at 0, it does not, and the enable becomes a live
/// candidate again -- render has no classic path to fall back on.
pub(crate) fn g17p_compute_sksm_entry_enabled() -> bool {
    crate::g17_config::COMPUTE_SKSM_ENTRY != 0
}


/// Result of one render completion poll.
#[derive(Debug)]
pub(crate) enum G17PRenderPollOutcome {
    /// Both halves retired; the render is done.
    Complete(g17_completion::PairedRenderCompletion),
    /// A completed job list was quiesced; re-poll immediately, do not sleep.
    Retry,
    /// Nothing yet; sleep before polling again.
    Pending,
}

impl G17PManagerConstruction {
    fn next_render_context_epoch(&self, slot: u8) -> Result<u64> {
        self.render_context_epochs
            .get(usize::from(slot))
            .ok_or(EINVAL)?
            .checked_add(1)
            .ok_or(EOVERFLOW)
    }

    /// Select a retained logical render owner without altering any
    /// firmware-visible bytes.  All state reachable by a submitted TA/3D pair
    /// stays with its slot; the global outer rings and HardwareBufferID
    /// allocator remain shared and are still published under the runtime
    /// mutex.


    pub(crate) fn render_slot_allocated(&self, slot: u8) -> bool {
        self.render_slots.allocated(slot).is_ok()
    }

    pub(crate) fn ensure_render_slot(&mut self, slot: u8) -> Result {
        self.render_slots.ensure(slot)
    }

    /// Lazily create two stable logical engine channels. The registry's slot
    /// belongs to one logical owner until processor stop; it is not a victim
    /// chosen for reassignment when another client becomes runnable.
    pub(crate) fn render_queue_pair(&mut self, slot: u8) -> Result<g17_resources::G17PRenderQueuePair> {
        use crate::g17_channel::{ChannelId, Engine};
        if slot == 0 { return g17_resources::G17PRenderQueuePair::for_slot(0).ok_or(EINVAL); }
        let index = usize::from(slot);
        if index >= self.render_channel_leases.len() { return Err(EINVAL); }
        if self.render_channel_leases[index].is_none() {
            let identity = self.next_channel_identity;
            let next = identity.checked_add(2).ok_or(EOVERFLOW)?;
            let ta = self.channel_ids.acquire(ChannelId(identity), Engine::Tiling, None).ok_or(ENOSPC)?;
            self.next_channel_identity = next;
            let fragment = match self.channel_ids.acquire(ChannelId(identity + 1), Engine::Fragment, None) {
                Some(lease) => lease,
                None => {
                    if !self.channel_ids.cancel_unpublished(ta) { return Err(EIO); }
                    return Err(ENOSPC);
                }
            };
            self.render_channel_leases[index] = Some([ta, fragment]);
        }
        self.retained_render_queue_pair(slot)
    }

    fn retained_render_queue_pair(&self, slot: u8) -> Result<g17_resources::G17PRenderQueuePair> {
        if slot == 0 { return g17_resources::G17PRenderQueuePair::for_slot(0).ok_or(EINVAL); }
        let leases = self.render_channel_leases.get(usize::from(slot)).ok_or(EINVAL)?
            .as_ref().ok_or(ENODEV)?;
        if !leases.iter().all(|lease| self.channel_ids.matches(*lease)) { return Err(EIO); }
        g17_resources::G17PRenderQueuePair::with_queue_ids(
            slot, u16::from(leases[0].qid), u16::from(leases[1].qid),
        ).ok_or(EINVAL)
    }

    /// Route recovery by the leased EventMachine ID, never by slot arithmetic.
    pub(crate) fn render_slot_for_qid(&self, qid: u8) -> Option<u8> {
        if qid < 2 { return Some(0); }
        self.render_channel_leases.iter().enumerate().find_map(|(slot, leases)| {
            leases.as_ref().filter(|leases| leases.iter().any(|lease|
                lease.qid == qid && self.channel_ids.matches(*lease)
            )).map(|_| slot as u8)
        })
    }

    fn allocate_active_render_command_buffer_state(
        &mut self, pair_slot: u8, lease: Arc<g17_resources::G17PWorkStateLease>,
    ) -> Result<u32> {
        let slot = pair_slot as usize;
        if self.render_command_buffer_state_pairs[slot].is_some() {
            return Err(EBUSY);
        }
        let word = lease.acquire()?;
        self.render_command_buffer_state_pairs[slot] = Some(lease);
        Ok(word)
    }

    fn release_active_render_command_buffer_states(&mut self, pair_slot: u8) -> Result {
        let slot = pair_slot as usize;
        let lease = self.render_command_buffer_state_pairs[slot]
            .take()
            .ok_or(EIO)?;
        drop(lease);
        Ok(())
    }

    fn take_active_render_command_buffer_state(&mut self, pair_slot: u8) -> Result<Arc<g17_resources::G17PWorkStateLease>> {
        self.render_command_buffer_state_pairs[pair_slot as usize]
            .take()
            .ok_or(EIO)
    }

    fn release_render_command_buffer_state(&mut self, pair_slot: u8, word: u32) -> Result {
        let tracker = self.render_slots.get_mut(pair_slot).tracker.as_mut().ok_or(EIO)?;
        if tracker.work_state.as_ref().map(|lease| lease.word()) != Some(word) { return Err(EIO); }
        drop(tracker.work_state.take());
        Ok(())
    }

    fn parked_render_scheduler_active(&self) -> bool {
        let pair_slot = 0;
        self.render_slots
            .parked(pair_slot)
            .any(|state| state.scheduler_active())
    }

    pub(crate) fn new(
        dev: &AsahiDevice,
        soc: &'static agx3::SocConfig,
        platform: G17PPlatformConfig,
        num_clusters: u32,
        gpc_perf_state_map: u32,
        gpc_perf_state_map_low: u32,
        gpc_perf_state_control: u32,
    ) -> Result<KBox<Self>> {
        let uat = KBox::new(mmu::Uat::new_t8140(dev, true)?, GFP_KERNEL)?;
        Self::new_with_uat(
            dev,
            soc,
            platform,
            num_clusters,
            gpc_perf_state_map,
            gpc_perf_state_map_low,
            gpc_perf_state_control,
            uat,
        )
    }

    /// Kept out of line and heap-returned: the construction locals plus the
    /// assembled manager otherwise land several times in the probe-path stack
    /// frames, which overflowed the 32 KiB task stack on hardware.
    #[inline(never)]
    pub(crate) fn new_with_uat(
        dev: &AsahiDevice,
        soc: &'static agx3::SocConfig,
        platform: G17PPlatformConfig,
        num_clusters: u32,
        gpc_perf_state_map: u32,
        gpc_perf_state_map_low: u32,
        gpc_perf_state_control: u32,
        uat: KBox<mmu::Uat>,
    ) -> Result<KBox<Self>> {
        if num_clusters == 0
            || gpc_perf_state_map > 0xf
            || gpc_perf_state_map_low > 0xf
            || gpc_perf_state_control > 1
        {
            return Err(EINVAL);
        }
        validate_target(target_observation(soc)).map_err(|_| EINVAL)?;
        validate_platform(platform).map_err(|_| EINVAL)?;

        let mut resources = g17_resources::G17ResourceOwner::new_with_uat(dev, uat)?;
        g17_resources::validate_handoff(resources.handoff()).map_err(|_| EINVAL)?;
        validate_resources(resource_observation(resources.handoff(), resources.uat()))
            .map_err(|_| EINVAL)?;
        // Queue close cannot fail, so every possible outstanding teardown has
        // its slot reserved up front.
        let logical_queue_teardowns =
            KVec::with_capacity(G17P_MAX_LOGICAL_QUEUE_TEARDOWNS, GFP_KERNEL)?;
        #[cfg(CONFIG_DEV_COREDUMP)]
        let trace_capture = if module_parameters::g17p_binary_capture.value() != 0 {
            match G17PTraceDump::new(&mut resources) {
                Ok(capture) => Some(capture),
                Err(error) => {
                    // Evidence collection must not turn memory pressure into a
                    // GPU execution failure. This absence is explicit in the log.
                    dev_warn!(dev.as_ref(), "G17P binary capture unavailable: {:?}\n", error);
                    None
                }
            }
        } else {
            None
        };

        // Keep the complete manager value off the probe task's stack.  The
        // two-slot owner contains three render-state images (active plus two
        // parked slots); constructing `Self` by value made rustc reserve
        // 0x3a40 bytes in this function, on top of boot_initial_session's
        // 0x890-byte caller frame.  That exceeds arm64's 16 KiB kernel stack
        // before RTKit/initdata can return from module probe.  An in-place
        // initializer writes every field directly into the final allocation.
        let mut manager = KBox::init(
            try_init!(Self {
                resources,
                #[cfg(CONFIG_DEV_COREDUMP)]
                trace_capture,
                platform,
                num_clusters,
                gpc_perf_state_map,
                gpc_perf_state_map_low,
                gpc_perf_state_control,
                render_context_epochs: [0; 64],
                logical_queue_teardowns,
                render_hardware_buffer_ids: G17PHardwareBufferIdAllocator::new(),
                render_command_buffer_state_pairs: [const { None }; 64],
                render_outer_channels_started: [false; 64],
                compute_channel_control: None,
                logical_qos_ids: None,
                compute_b2_activation: g17_submission::G17PClB2ActivationState::Pending,
                next_render_pm_manager_generation:
                    g17_resources::G17P_PM_INITIAL_HWPB_MANAGER_GENERATION,
                render_parameter_buffer_owner: 0,
                render_slots: G17PRenderSlots::new()?,
                render_flist_owners: g17p_new_render_flist_owners()?,
                render_flist_scan: [0; 4],
                render_command_scratch: Some(KBox::new_uninit(GFP_KERNEL)?),
                render_channel_leases: [None; 64],
                accepted_firmware_recovery_generation: 0,
                pending_primary_recovery: None,
                pending_primary_control: None,
                active_render_grow: None,
                pending_render_grows: KVec::with_capacity(
                    2 * g17_completion::G17P_FIRMWARE_EVENT_CAPACITY as usize, GFP_KERNEL)?,
                pending_flist_release_worker_attempts: 0,
                pending_add_kicks: None,
                last_published_descriptor: 0,
                channel_ids: crate::g17_channel::ChannelTable::new(),
                next_channel_identity: 3,
                last_completion_end: 0,
                compute_completion_batch: g17_resources::G17PComputeCompletionBatch::new()?,
                compute_stamp_armed: false,
                compute_stamp_waits: AtomicU32::new(0),
                compute_stamp_misses: AtomicU32::new(0),
            }),
            GFP_KERNEL,
        )?;
        if !g17p_reserve_bootstrap_channels(&mut manager.channel_ids) {
            return Err(EINVAL);
        }
        Ok(manager)
    }

    /// Large prepared-source observation before FList and outer publication.
    /// Diagnostic modes which expose an outer early receive an explicit
    /// omission instead of a misleading or timing-invasive "before" snapshot.
    pub(crate) fn capture_prepared_render_trace(&mut self, pair_slot: u8, outers_deferred: bool) -> Result {
        #[cfg(CONFIG_DEV_COREDUMP)]
        {
            // Before the first render, the boot archive belongs to the
            // session, not the unused bootstrap slot. Attach it to the first
            // real graph. Once prepared, it must travel only with that graph.
            if self.render_slots.get(pair_slot).trace_capture.is_none() {
                if let Some(capture) = self.trace_capture.take() {
                    self.render_slots.get_mut(pair_slot).trace_capture = Some(capture);
                }
            }
            let slot = self.render_slots.get_mut(pair_slot);
            let Some(capture) = slot.trace_capture.as_mut() else { return Ok(()); };
            if capture.prepared { return Ok(()); }
            capture.prepared = true;
            capture.job_stamp = slot.sksm.as_ref()
                .map_or(0, |queues| queues.fragment.producer.last_add_kicks_timestamp);
            self.capture_render_trace_observation(pair_slot, trace::Phase::PreparedBeforeFList, outers_deferred)?;
        }
        Ok(())
    }

    /// Observe into the archive attached to the selected slot. The unattached
    /// boot archive is never observed: it is attached when a render prepares
    /// it, and both callers require a prepared archive.
    #[cfg(CONFIG_DEV_COREDUMP)]
    fn capture_render_trace_observation(&mut self, pair_slot: u8, phase: trace::Phase, safe: bool) -> Result {
        let slot = self.render_slots.get_mut(pair_slot);
        let Some(capture) = slot.trace_capture.as_mut() else { return Ok(()); };
        capture.phase_sequence += 1;
        let sequence = capture.phase_sequence;
        let stamp = capture.job_stamp;
        let mut archive = TraceArchive::resume(&mut capture.arena).map_err(|_| EINVAL)?;
        let mut boundary = trace::Meta::new(trace::Kind::CaptureBoundary, phase, sequence);
        boundary.job_stamp = stamp;
        if !safe {
            archive.omit(boundary, trace::Omission::NotImplemented).map_err(|_| EINVAL)?;
            return Ok(());
        }
        let mut clock = || Monotonic::ktime_get() as u64;
        if self.resources.capture_owned_trace(&mut archive, phase, sequence, &mut clock).is_err() {
            archive.omit(boundary, trace::Omission::ReadError).map_err(|_| EINVAL)?;
        }
        match slot.storage.as_deref_mut() {
            Some(storage) => {
                if storage.capture_owned_trace(&mut archive, phase, sequence, stamp, &mut clock).is_err() {
                    boundary.instance = 1;
                    archive.omit(boundary, trace::Omission::ReadError).map_err(|_| EINVAL)?;
                }
            }
            None => {
                boundary.instance = 1;
                archive.omit(boundary, trace::Omission::Unavailable).map_err(|_| EINVAL)?;
            }
        }
        Ok(())
    }

    /// Only call after paired completion (before FList release), or after the
    /// state-1 recovery handshake has been validated (before ACK). A timeout
    /// alone is not permission to label a running GPU's memory "halted".
    #[cfg(CONFIG_DEV_COREDUMP)]
    pub(crate) fn finish_render_trace(
        &mut self, pair_slot: u8, phase: trace::Phase,
    ) -> Result<Option<KBox<G17PTraceDump>>> {
        if !matches!(phase, trace::Phase::AfterPairRetirement | trace::Phase::RecoveryBeforeAck) {
            return Err(EINVAL);
        }
        if !self.render_slots.get(pair_slot).trace_capture.as_ref().is_some_and(|capture| capture.prepared) {
            return Ok(None);
        }
        self.capture_render_trace_observation(pair_slot, phase, true)?;
        let mut capture = self.render_slots.get_mut(pair_slot).trace_capture.take().ok_or(EINVAL)?;
        let mut archive = TraceArchive::resume(&mut capture.arena).map_err(|_| EINVAL)?;
        archive.seal();
        let used = archive.bytes().len();
        drop(archive);
        capture.used = used;
        Ok(Some(capture))
    }

    pub(crate) fn config(&self) -> &'static T8140ManagerConfig {
        &T8140_MANAGER_CONFIG
    }

    pub(crate) fn handoff(&self) -> &g17_resources::G17ResourceHandoff {
        self.resources.handoff()
    }

    pub(crate) fn control_counters(&mut self) -> Result<g17_resources::G17PControlCounters> {
        self.resources.control_counters()
    }

    pub(crate) fn control_pointer_snapshot(
        &mut self,
    ) -> Result<g17_resources::G17PControlPointerSnapshot> {
        self.resources.control_pointer_snapshot()
    }

    pub(crate) fn publish_clpc_effort(&mut self, effort: u32) -> Result {
        self.resources.publish_clpc_effort(effort)
    }

    pub(crate) fn snapshot_hw_data(&mut self, out: &[core::sync::atomic::AtomicU64]) -> Result {
        self.resources.snapshot_hw_data(out)
    }

    pub(crate) fn publish_submit_in_progress(&mut self, in_progress: u32, submits: u32) -> Result {
        self.resources.publish_submit_in_progress(in_progress, submits)
    }

    pub(crate) fn publish_native_submit_bracket(&mut self, open: bool, submits: u32) -> Result {
        self.resources.publish_native_submit_bracket(open, submits)
    }

    pub(crate) fn read_clpc_effort(&mut self) -> Result<u32> {
        self.resources.read_clpc_effort()
    }

    pub(crate) fn publish_power_target(&mut self, mw: u32, reason: u32) -> Result {
        self.resources.publish_power_target(mw, reason)
    }

    pub(crate) fn read_power_target(&mut self) -> Result<u32> {
        self.resources.read_power_target()
    }

    pub(crate) fn read_gfx1_stats_u32(&mut self, offset: usize) -> Result<u32> {
        self.resources.read_gfx1_stats_u32(offset)
    }

    pub(crate) fn snapshot_primary_state_prefix(&mut self, out: &[core::sync::atomic::AtomicU64]) -> Result {
        self.resources.snapshot_primary_state_prefix(out)
    }

    pub(crate) fn status_a_snapshot(&mut self) -> Result<g17_resources::G17PStatusASnapshot> {
        self.resources.status_a_snapshot()
    }

    pub(crate) fn firmware_gpu_progress_snapshot(&mut self) -> Result<(u64, u32)> {
        self.resources.firmware_gpu_progress_snapshot()
    }

    pub(crate) fn firmware_recovery_handshake_snapshot(
        &mut self,
    ) -> Result<g17_resources::G17PFirmwareRecoveryHandshakeSnapshot> {
        self.resources.firmware_recovery_handshake_snapshot()
    }

    /// v87 STATE_WORD: the recovery handshake state word alone.
    pub(crate) fn firmware_recovery_state(&mut self) -> Result<u32> {
        self.resources.firmware_recovery_state()
    }

    pub(crate) fn firmware_fault_report_snapshot(
        &mut self,
    ) -> Result<g17_resources::G17PFirmwareFaultReportSnapshot> {
        self.resources.firmware_fault_report_snapshot()
    }

    pub(crate) fn firmware_dm1_slot_snapshot(
        &mut self,
        slot: usize,
    ) -> Result<g17_resources::G17PFirmwareDm1SlotSnapshot> {
        self.resources.firmware_dm1_slot_snapshot(slot)
    }

    pub(crate) fn drain_statistics_ring_native(
        &mut self,
        role: InstanceRole,
    ) -> Result<g17_resources::G17PNativeRingDrainStats> {
        self.resources.drain_statistics_ring_native(role)
    }

    pub(crate) fn drain_stats_ring(&mut self, role: InstanceRole, consume: bool) -> Result<(u32, u32)> {
        self.resources.drain_stats_ring(role, consume)
    }

    pub(crate) fn drain_firmware_event(
        &mut self, role: g17_initdata::InstanceRole,
    ) -> Result<Option<g17_resources::G17PPrimaryFirmwareEventRecord>> {
        self.resources.drain_firmware_event(role)
    }

    pub(crate) fn drain_primary_firmware_event(
        &mut self,
    ) -> Result<Option<g17_resources::G17PPrimaryFirmwareEventRecord>> {
        self.resources.drain_primary_firmware_event()
    }

    pub(crate) fn queue_primary_recovery(
        &mut self,
        generation: u64,
        records: u32,
        blamed_stamp: Option<u8>,
    ) -> Result {
        if records == 0 || self.pending_primary_recovery.is_some() {
            return Err(EBUSY);
        }
        if blamed_stamp.is_some_and(|stamp| stamp >= 128) {
            return Err(EINVAL);
        }
        self.pending_primary_recovery = Some((generation, records, blamed_stamp));
        Ok(())
    }

    /// Queue a firmware restart request, or fold it into the one already
    /// pending.
    ///
    /// The firmware raises ONE restart request, and both RTKit roles can
    /// observe it: `service_primary_firmware_events` drains the gfx ring and
    /// then, in the same pass and unconditionally, the gfx1 ring. A type-4 on
    /// the second ring therefore meets a request the first ring already
    /// queued, and the bare `queue_primary_recovery` answers that with EBUSY.
    /// Every caller propagates it with `?` into a chain whose only error
    /// handler sets `crashed`, so a duplicate view of a single recovery -- the
    /// normal dual-role case, not a fault -- permanently deafened the session.
    ///
    /// Coalescing is what the surrounding code already does with duplicate
    /// type-4 records inside ONE ring (`recovery_records` counts them and the
    /// accepted generation advances once), so doing it across the two rings is
    /// the same rule, not a new one. Returns whether a new request was queued.
    pub(crate) fn queue_or_coalesce_primary_recovery(
        &mut self,
        generation: u64,
        records: u32,
        blamed_stamp: Option<u8>,
    ) -> Result<bool> {
        if let Some((pending_generation, pending_records, pending_stamp)) =
            self.pending_primary_recovery
        {
            // A pending request for a DIFFERENT generation is a real disagreement
            // about how many restarts the firmware asked for, and must not be
            // silently merged.
            if pending_generation != generation {
                dev_err!(
                    self.resources.dev().as_ref(),
                    "G17P recovery: type-4 generation {} arrived while generation {} is still pending\n",
                    generation,
                    pending_generation
                );
                return Err(EBUSY);
            }
            self.pending_primary_recovery = Some((
                generation,
                pending_records.saturating_add(records),
                pending_stamp.or(blamed_stamp),
            ));
            return Ok(false);
        }
        self.queue_primary_recovery(generation, records, blamed_stamp)?;
        Ok(true)
    }

    /// The host generation the next type-4 record must name. Records naming an
    /// older generation are duplicates of a restart already serviced.
    pub(crate) fn accepted_firmware_recovery_generation(&self) -> u64 {
        self.accepted_firmware_recovery_generation
    }

    pub(crate) fn pending_primary_recovery(&self) -> Option<(u64, u32, Option<u8>)> {
        self.pending_primary_recovery
    }

    pub(crate) fn complete_pending_primary_recovery(
        &mut self,
        generation: u64,
        records: u32,
        blamed_stamp: Option<u8>,
    ) -> Result {
        if self.pending_primary_recovery != Some((generation, records, blamed_stamp)) {
            return Err(EINVAL);
        }
        self.pending_primary_recovery = None;
        Ok(())
    }

    pub(crate) fn primary_firmware_recovery_cause_snapshot(
        &mut self,
    ) -> Result<g17_resources::G17PFirmwareRecoveryCauseSnapshot> {
        self.resources.primary_firmware_recovery_cause_snapshot()
    }

    pub(crate) fn arm_firmware_trace_classes(&mut self, mask: u32) -> Result {
        self.resources.arm_firmware_trace_classes(mask)
    }

    pub(crate) fn check_report_ring_pointers(
        &mut self,
        role: InstanceRole,
    ) -> Result<(u64, u64)> {
        self.resources.check_report_ring_pointers(role)
    }

    pub(crate) fn drain_report_ring(
        &mut self,
        role: InstanceRole,
        advance: bool,
        print_budget: u32,
    ) -> Result<g17_resources::G17PReportDrainStats> {
        self.resources.drain_report_ring(role, advance, print_budget)
    }

    pub(crate) fn check_ktrace_ring_pointers(
        &mut self,
        role: InstanceRole,
    ) -> Result<(u64, u64)> {
        self.resources.check_ktrace_ring_pointers(role)
    }

    pub(crate) fn drain_ktrace_ring(
        &mut self,
        role: InstanceRole,
        verbose: bool,
        print_budget: u32,
    ) -> Result<g17_resources::G17PKtraceDrainStats> {
        self.resources
            .drain_ktrace_ring(role, verbose, print_budget)
    }

    /// Publish the held-back tag-14 AddKicks. Call only once the firmware
    /// recovery has closed, otherwise the unhalt's 128-queue restore erases it.
    pub(crate) fn publish_pending_add_kicks(
        &mut self,
        queue: &mut G17PSksmQueue,
        expected_producer: u32,
    ) -> Result<bool> {
        let Some(bytes) = self.pending_add_kicks.take() else {
            return Ok(false);
        };
        let after = queue
            .compute
            .publish_add_kicks_command(expected_producer, &bytes)?;
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P compute: deferred tag-14 AddKicks published at producer {} -> {}\n",
            expected_producer,
            after
        );
        Ok(true)
    }

    pub(crate) fn has_pending_add_kicks(&self) -> bool {
        self.pending_add_kicks.is_some()
    }

    pub(crate) fn dump_firmware_log(&mut self, max_records: usize) -> Result<usize> {
        self.resources.dump_firmware_log(max_records)
    }

    pub(crate) fn dump_completion_stamps(&mut self, label: &'static str) -> Result {
        self.resources.dump_completion_stamps(label)
    }

    pub(crate) fn capture_completion_ring_snapshot(&mut self) -> Result {
        let read_bias = || -> Result<u64> {
            const LINK: u64 = 0xffff_fc00_0017_9318;
            let start = g17p_binding_row_address(LINK, 0, 8).ok_or(EINVAL)?;
            pgtable::LiveFirmwareU64Probe::new(g17p_gfx_link_to_physical(start))
                .and_then(|probe| probe.observe_mask(u64::MAX, 1))
                .map(|sample| sample.first)
        };
        crate::g17_timing_buffer::completion_snapshot_meta(1,
            crate::g17_timing_buffer::arch_counter());
        crate::g17_timing_buffer::completion_snapshot_meta(3,
            crate::g17_timing_buffer::arch_frequency());
        crate::g17_timing_buffer::completion_snapshot_meta(8,
            u64::from(self.accepted_firmware_recovery_generation));
        let before = read_bias()?;
        crate::g17_timing_buffer::completion_snapshot_meta(4, before);
        self.resources.capture_completion_ring_snapshot()?;
        let after = read_bias()?;
        crate::g17_timing_buffer::completion_snapshot_meta(5, after);
        if before != after { return Err(EAGAIN); }
        Ok(())
    }

    pub(crate) fn dump_completion_records(&mut self, label: &'static str) -> Result {
        self.dump_completion_records_targets(label, [None; 2])
    }

    fn dump_completion_records_targets(&mut self, label: &'static str, targets: [Option<(u8, u64)>; 2]) -> Result {
        // Native B1 ab64 adds this DATA value to completion pair-B timestamps
        // before writing descriptor timestamps. Capture it around the bounded
        // ring snapshot: raw and descriptor clocks must not be silently mixed.
        let read_bias = || -> Result<u64> {
            const LINK: u64 = 0xffff_fc00_0017_9318;
            if LINK < G17P_GFX_LINK_DATA_BASE || LINK + 8 > G17P_GFX_LINK_DATA_END {
                return Err(EINVAL);
            }
            pgtable::LiveFirmwareU64Probe::new(g17p_gfx_link_to_physical(LINK))
                .and_then(|probe| probe.observe_mask(u64::MAX, 1))
                .map(|sample| sample.first)
        };
        let before = read_bias();
        let result = (|| {
            self.resources.dump_completion_records(label)?;
            for target in targets.into_iter().flatten() {
                self.resources.dump_completion_target(label, target)?;
            }
            Ok(())
        })();
        let after = read_bias();
        dev_info!(self.resources.dev().as_ref(),
            "G17P completion timestamp bias[{}] before/after={:x?}/{:x?} (bounded non-atomic DATA snapshot)\n",
            label, before, after);
        result
    }

    /// Publish one item-ring record, reporting the ring state if it is refused.
    ///
    /// The four publishes used to `?` straight out, so an exhausted or
    /// unreclaimed ring surfaced as a bare `EBUSY` from four frames down with
    /// nothing to say which record failed or what the cursors were. At the ring
    /// wrap that is precisely the information needed.
    fn publish_repeat_record(
        &mut self,
        queue: &mut G17PSksmQueue,
        which: &'static str,
        producer: u32,
        record: G17PRepeatRecord,
    ) -> Result<u32> {
        let result = match record {
            G17PRepeatRecord::Pointer(gpu_va) => {
                queue.compute.publish_command_pointer_at(producer, gpu_va)
            }
        };
        match result {
            Ok(next) => Ok(next),
            Err(error) => {
                dev_err!(
                    self.resources.dev().as_ref(),
                    "G17P compute: publishing {} at ring producer {} failed ({:?}); the target slot was not free\n",
                    which,
                    producer,
                    error
                );
                self.log_compute_ring_state(queue, "publish-failed");
                Err(error)
            }
        }
    }

    /// Per-submission retire trace, on the SUCCESS path.
    ///
    /// The failure mode has moved: a repeat now retires cleanly, with no ioctl
    /// error and no timeout, while the GPU writes nothing. A timeout-only hook
    /// can never observe that, so this samples the same evidence on every
    /// successful retire.
    ///
    /// The decisive field is the completion's own duration, `end - start`.
    /// Those are GPU timestamps written by the firmware, so:
    ///
    ///  * a plausible, varying duration means the GPU really executed and the
    ///    result is invisible to the reader -- a coherency or cache-maintenance
    ///    problem, not a scheduling one; whereas
    ///  * a zero or near-constant duration means the firmware retired a record
    ///    for work the GPU never ran, and the bug is upstream of completion.
    ///
    /// The support words are the cross-check: they are the GPU's own progress
    /// words for the submission, so if they stop advancing while completions
    /// keep arriving, the second reading is confirmed independently.
    ///
    /// DRAM only.
    pub(crate) fn log_compute_retire(
        &mut self,
        queue: &mut G17PSksmQueue,
        completion: [u64; 2],
        ordinal: u32,
    ) {
        let support = queue.compute.read_compute_support_words();
        let duration = completion[1].wrapping_sub(completion[0]);
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P retire[{}]: descriptor {:#x} start {:#x} end {:#x} duration {:#x} support(dispatch-a,dispatch-b,status-a,status-b)={:#x?}\n",
            ordinal,
            self.last_published_descriptor,
            completion[0],
            completion[1],
            duration,
            support
        );
        if module_parameters::g17p_completion_trace.value() > 1 {
            let _ = self.dump_compute_completion_records("retire");
        }
    }

    /// Decode the completion ring against the descriptor the in-flight
    /// submission is waiting for.
    pub(crate) fn dump_compute_completion_records(&mut self, label: &'static str) -> Result {
        let expected = self.last_published_descriptor;
        self.resources
            .dump_compute_completion_records(label, expected, None)
    }

    /// Timeout dump for one independent queue: its own awaited descriptor
    /// (not the device's last publication) and only its own records, with
    /// each record's decoded kick timestamp next to the kicks still in
    /// flight and the stamp sequence they are waiting for.
    pub(crate) fn dump_queue_compute_completion_records(
        &mut self,
        label: &'static str,
        queue: &mut G17PSksmQueue,
    ) -> Result {
        let stamp = queue.compute.read_compute_stamp().unwrap_or(0);
        let counters = queue.compute.read_compute_submitted_counters().unwrap_or([0, 0]);
        let newest = queue.producer.previous_timestamp;
        let oldest = newest.wrapping_sub(u64::from(queue.producer.submitted.saturating_sub(1)));
        // Per-kick identity is the firmware completed counter vs each kick's
        // target; the stamp word is a constant queue-level flush barrier
        // (non-zero == flushed). Print both so a timeout shows which is missing.
        dev_err!(self.resources.dev().as_ref(),
            "G17P completion[{}]: qid={} in-flight={} kick-ts={:#x}..={:#x} target={:#x} completed={:#x} stamp-word={:#x} flushed={}\n",
            label, queue.queue_id, queue.producer.submitted, oldest, newest,
            counters[0], counters[1], stamp,
            g17_compute::completion_stamp_covers(stamp, newest));
        let expected = queue.completion_descriptor;
        self.resources
            .dump_compute_completion_records(label, expected, Some(queue.queue_id))
    }

    pub(crate) fn read_compute_completion(&mut self) -> Result<Option<[u64; 2]>> {
        let expected = self.last_published_descriptor;
        let last_end = self.last_completion_end;
        self.resources.read_compute_completion(expected, last_end)
    }

    /// Mark a completion as consumed so the next wait cannot be satisfied by
    /// the record it left behind.
    pub(crate) fn commit_compute_completion(&mut self, completion: [u64; 2]) {
        self.last_completion_end = completion[1];
    }

    pub(crate) fn dump_primary_recovery_cause_block(&mut self) -> Result {
        self.resources.dump_primary_recovery_cause_block()
    }

    pub(crate) fn primary_firmware_recovery_info_snapshot(
        &mut self,
    ) -> Result<g17_resources::G17PFirmwareRecoveryInfoSnapshot> {
        self.resources.primary_firmware_recovery_info_snapshot()
    }

    pub(crate) fn primary_recovery_info_entries(&mut self, out: &mut [(u32, u32)]) -> Result<usize> {
        self.resources.primary_recovery_info_entries(out)
    }

    /// Accept one firmware-initiated recovery before Linux has constructed a
    /// queue, fence, or user submission. Native macOS compares the packet to
    /// its host-side accepted generation and increments that generation before
    /// it publishes recovery state 2.
    pub(crate) fn validate_empty_primary_firmware_recovery(
        &self,
        packet_generation: u64,
    ) -> Result {
        if packet_generation != self.accepted_firmware_recovery_generation {
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P recovery: packet generation {} does not match host accepted generation {}\n",
                packet_generation,
                self.accepted_firmware_recovery_generation
            );
            return Err(EIO);
        }
        Ok(())
    }

    /// Is there live render scheduler state?
    ///
    /// This is the exact predicate
    /// `validate_empty_primary_firmware_recovery_scheduler` refuses on. It is
    /// exposed separately because that refusal is unsatisfiable for a
    /// render-blamed recovery: the firmware raises the restart request DURING
    /// the render wait, so `render_storage` and `render_tracker` are
    /// necessarily populated at exactly the moment we need to answer.
    pub(crate) fn render_scheduler_active(&self) -> bool {
        let pair_slot = 0;
        self.render_slots.get(pair_slot).storage.is_some()
            || self.render_slots.get(pair_slot).tracker.is_some()
            || self.render_slots.get(pair_slot).queued_tracker.is_some()
            || self.render_slots.get(pair_slot).bind.is_some()
            || self.render_slots.get(pair_slot).context_retirement.is_some()
            || !self.logical_queue_teardowns.is_empty()
            || self.render_slots.get(pair_slot).submission_ordinal != 0
            || self.parked_render_scheduler_active()
    }

    pub(crate) fn validate_empty_primary_firmware_recovery_scheduler(&self) -> Result {
        let pair_slot = 0;
        if self.render_slots.get(pair_slot).storage.is_some()
            || self.render_slots.get(pair_slot).tracker.is_some()
            || self.render_slots.get(pair_slot).queued_tracker.is_some()
            || self.render_slots.get(pair_slot).bind.is_some()
            || self.render_slots.get(pair_slot).context_retirement.is_some()
            || !self.logical_queue_teardowns.is_empty()
            || self.render_slots.get(pair_slot).submission_ordinal != 0
            || self.parked_render_scheduler_active()
        {
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P recovery: empty-scheduler validation failed storage={} tracker={} bind={} ordinal={}\n",
                self.render_slots.get(pair_slot).storage.is_some(),
                self.render_slots.get(pair_slot).tracker.is_some(),
                self.render_slots.get(pair_slot).bind.is_some(),
                self.render_slots.get(pair_slot).submission_ordinal
            );
            return Err(EBUSY);
        }
        Ok(())
    }

    pub(crate) fn accept_empty_primary_firmware_recovery(
        &mut self,
        packet_generation: u64,
    ) -> Result<u64> {
        self.validate_empty_primary_firmware_recovery(packet_generation)?;
        self.accepted_firmware_recovery_generation =
            self.accepted_firmware_recovery_generation.wrapping_add(1);
        Ok(self.accepted_firmware_recovery_generation)
    }

    pub(crate) fn acknowledge_primary_firmware_recovery(
        &mut self,
        expected: u32,
        next: u32,
    ) -> Result {
        self.resources
            .acknowledge_primary_firmware_recovery(expected, next)
    }

    pub(crate) fn reset_empty_sksm_last_submitted_hw_timestamps(&mut self) -> Result {
        self.resources
            .reset_empty_sksm_last_submitted_hw_timestamps()
    }

    /// Hexdump one kernel-built record, for the add3-vs-Honeykrisp diff.
    ///
    /// Everything audited so far is an object the GPU READS. These are the
    /// records the DRIVER WRITES from the UAPI submit arguments, and add3 and
    /// Honeykrisp reach them through different callers with different
    /// arguments -- the last surface where the two paths can differ.
    fn log_record(&self, label: &str, bytes: &[u8]) {
        if module_parameters::g17p_fault_report.value() & 1 == 0 {
            return;
        }
        for (index, chunk) in bytes.chunks(16).enumerate() {
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P rec {} +{:#05x} {:02x?}\n",
                label,
                index * 16,
                chunk,
            );
        }
    }

    pub(crate) fn g17p_last_submitted_hw_timestamp(&mut self, qid: usize) -> Result<(u32, u64)> {
        self.resources.g17p_last_submitted_hw_timestamp(qid)
    }

    /// Verify the untouched native pre-user root phase. The first real user
    /// bind owns the transition from the retained context-0 roots to context
    /// 1; no driver-owned startup render VM is needed to manufacture it.
    pub(crate) fn validate_native_pre_user_roots(&self) -> Result {
        if !self
            .resources
            .uat()
            .t8140_partial_opening_kernel_contexts_ready()
        {
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P roots: native pre-user context 0/1 table is invalid\n"
            );
            return Err(EIO);
        }
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P roots: native pre-user context 0/1 table retained; user bind owns first split\n"
        );
        Ok(())
    }

    pub(crate) fn activate_render_global_context(&self) -> Result {
        self.resources
            .uat()
            .activate_t8140_render_global_context()
    }

    pub(crate) fn control_opening_effect(
        &mut self,
    ) -> Result<g17_resources::G17PControlOpeningEffect> {
        self.resources.control_opening_effect()
    }

    /// Stage the same queue-pair-zero TA/3D bootstrap used by the
    /// output-positive source path. The VDM stream is a driver-owned terminate
    /// command, so this establishes scheduler lifecycle without user BOs.
    pub(crate) fn stage_compute_bootstrap_render(
        &mut self,
        dev: &AsahiDevice,
    ) -> Result<mmu::Vm> {
        let pair_slot = 0;
        let kernel_range = self.resources.uat().kernel_va_range().inspect_err(|error| {
            dev_err!(
                dev.as_ref(),
                "G17P bootstrap: kernel VA range failed ({:?})\n",
                error
            );
        })?;
        let vm = self
            .new_user_vm_inner(0x8140_b007, kernel_range, true)
            .inspect_err(|error| {
                dev_err!(
                    dev.as_ref(),
                    "G17P bootstrap: render VM setup failed ({:?})\n",
                    error
                );
            })?;
        let mut parameters = g17_render::G17pRenderParameters::source_rsrc8();
        parameters.gpc_perf_state_map = self.gpc_perf_state_map;
        parameters.gpc_perf_state_map_low = self.gpc_perf_state_map_low;
        parameters.gpc_perf_state_control = self.gpc_perf_state_control;
        parameters.encoder = g17_resources::G17P_BOOTSTRAP_VDM_VA;
        parameters.process_empty_tiles = false;
        parameters.tile_config &= !0x1_0000;
        let timestamp = g17_uapi::UapiTimestamp {
            handle: 0,
            offset: 0,
        };
        let command = g17_uapi::TranslatedRenderCommand {
            render_order: g17_submission::G17PRenderOrder::default(),
            render_barrier: g17_uapi::UAPI_BARRIER_NONE,
            compute_barrier: g17_uapi::UAPI_BARRIER_NONE,
            compute_wait_through: None,
            flags: 0,
            vdm_base: parameters.encoder,
            parameters,
            vertex_attachments: g17_uapi::UapiAttachmentList::EMPTY,
            fragment_attachments: g17_uapi::UapiAttachmentList::EMPTY,
            vertex_timestamps: g17_uapi::UapiTimestamps {
                start: timestamp,
                end: timestamp,
            },
            fragment_timestamps: g17_uapi::UapiTimestamps {
                start: timestamp,
                end: timestamp,
            },
        };
        if self.render_slots.get(pair_slot).tracker.is_some()
            || self.render_slots.get(pair_slot).queued_tracker.is_some()
            || self.render_slots.get(pair_slot).storage.is_some()
            || self.render_slots.get(pair_slot).bind.is_some()
            || self.render_slots.get(pair_slot).submission_ordinal != 0
        {
            return Err(EBUSY);
        }
        let storage_box = KBox::<g17_resources::G17PUserRenderStorage>::new_uninit(GFP_KERNEL)?;
        let mut no_retained_sksm_backing = None;
        let pm_generation = self.prepare_render_pm_manager_generation()?;
        self.render_slots.get_mut(pair_slot).storage = Some(
            self.resources
                .new_user_render_storage(
                    storage_box,
                    dev,
                    &vm,
                    g17_resources::G17PRenderQueuePair::for_slot(0).ok_or(EINVAL)?,
                    None,
                    1,
                    None,
                    &command,
                    self.num_clusters,
                    // The driver-owned opening command has its own Shared
                    // state owner. Its first TA/3D pair uses native tags 0/1;
                    // user command buffers allocate from the session owner
                    // below and are not advanced by this bootstrap object.
                    [0, 1],
                    0,
                    pm_generation,
                    &mut no_retained_sksm_backing,
                )
                .inspect_err(|error| {
                    dev_err!(
                        dev.as_ref(),
                        "G17P bootstrap: render graph setup failed ({:?})\n",
                        error
                    );
                })?,
        );
        let storage = self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?;
        if let Some(failed) = storage.first_unreachable(self.resources.uat(), &vm) {
            storage.log_reachability(dev, self.resources.uat(), &vm);
            dev_err!(
                dev.as_ref(),
                "G17P bootstrap render: storage unreachable, first failing predicate is {}\n",
                failed
            );
            return Err(EFAULT);
        }
        let binding = self.resources.uat().bind(&vm)?;
        if binding.slot() != 1 {
            return Err(EFAULT);
        }
        self.resources
            .uat()
            .install_t8140_partial_opening_shared_render_roots(&binding)?;
        self.render_slots.get_mut(pair_slot).bind = Some(G17PRenderRootOwner::Bootstrap(binding));
        self.commit_render_pm_manager_generation(pair_slot, pm_generation)?;
        Ok(vm)
    }

    /// Apply the measured post-opening context split after primary
    /// control-done and before either first-work producer becomes visible.
    pub(crate) fn split_compute_bootstrap_render_contexts(&self) -> Result {
        let pair_slot = 0;
        let binding = self.render_slots.get(pair_slot).bind.as_ref().and_then(G17PRenderRootOwner::bootstrap).ok_or(ENODEV)?;
        self.resources
            .uat()
            .split_t8140_partial_opening_render_roots(binding)?;
        if !self
            .resources
            .uat()
            .t8140_first_work_render_context_ready(binding)
        {
            return Err(EFAULT);
        }
        Ok(())
    }

    /// Publish the staged bootstrap pair after primary control-done.
    pub(crate) fn publish_staged_compute_bootstrap_render(
        &mut self,
    ) -> Result<g17_submission::PreparedG17PPartialOpeningWorkPair> {
        let pair_slot = 0;
        if self.render_slots.get(pair_slot).tracker.is_some()
            || self.render_slots.get(pair_slot).queued_tracker.is_some()
            || self.render_slots.get(pair_slot).bind.is_none()
            || self.render_slots.get(pair_slot).submission_ordinal != 0
        {
            return Err(EBUSY);
        }
        if !self.resources.uat().t8140_first_work_render_context_ready(
            self.render_slots.get(pair_slot).bind.as_ref().and_then(G17PRenderRootOwner::bootstrap).ok_or(ENODEV)?,
        ) {
            return Err(EFAULT);
        }
        let (fragment_queue, tiling_queue, previous, late_tiling) = {
            let slot = self.render_slots.get_mut(pair_slot);
            let storage = slot.storage.as_deref_mut().ok_or(ENODEV)?;
            // The successful first-partial path keeps both inner groups blank
            // until primary control-done. Restore 3D now; TA remains held by
            // the late publication callback below.
            storage.stage_fragment(0)?;
            let previous = storage.snapshot(slot.submission_ordinal)?;
            let late_tiling = storage.prepare_late_tiling_publication(0)?;
            let (fragment_queue, tiling_queue) = storage.queue_gpu_vas();
            (fragment_queue, tiling_queue, previous, late_tiling)
        };
        let prepared = self.resources.publish_partial_opening_pair(
            true,
            g17_resources::G17PRenderQueuePair::for_slot(0).ok_or(EINVAL)?,
            g17_resources::G17PDeferredOuterPublication::None,
            fragment_queue,
            3,
            tiling_queue,
            3,
            true,
            || Ok(()),
            move || late_tiling.publish(),
        )?;
        self.render_slots.get_mut(pair_slot).tracker = Some(G17PPartialOpeningRenderTracker {
            ordinal: 0,
            hardware_buffer_id: 0,
            command_buffer_state_word: 0,
            work_state: None,
            native_timestamps: None,
            qos_pending: false,
            pool_a_record_index: self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?
                .current_pool_a_ticket().0,
            pool_a_submitted: self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?
                .current_pool_a_ticket().1,
            context_update: true,
            tiling_previous: previous.tiling,
            fragment_previous: previous.fragment,
            tiling_prefix: 3,
            fragment_prefix: 3,
            windows: [g17_completion::RenderQueueWindow::new(0, 3, 0x500)
                .map_err(|_| EINVAL)?; 2],
        });
        Ok(prepared)
    }

    /// Match the source bootstrap fence: both three-item inner queues and
    /// both outer channel consumers must retire before the graph can drop.
    pub(crate) fn wait_compute_bootstrap_render_retirement(&mut self) -> Result {
        let pair_slot = 0;
        const POLLS: usize = 500;
        let mut last = None;
        for _ in 0..POLLS {
            fence(Ordering::Acquire);
            let slot = self.render_slots.get_mut(pair_slot);
            let state = slot.storage.as_deref_mut().ok_or(ENODEV)?
                .snapshot(slot.submission_ordinal)?;
            let outer = self.resources.partial_opening_outer_counters()?;
            // m1n1's fence owns the prefix once done/read cover the three
            // items and the outer consumers reach producer one. The queue may
            // already advertise a later write prefix (the positive run is
            // 3/3/6), so equality with write=3 is not an ownership rule.
            let inner_retired = state.tiling.done >= 3
                && state.tiling.read >= 3
                && state.tiling.write >= 3
                && state.fragment.done >= 3
                && state.fragment.read >= 3
                && state.fragment.write >= 3;
            let outer_retired = outer
                .into_iter()
                .all(|counters| counters[2] == 1 && counters[..2] == [counters[2]; 2]);
            if inner_retired && outer_retired {
                self.render_slots.get_mut(pair_slot).tracker = None;
                self.render_slots.get_mut(pair_slot).bind = None;
                self.unreserve_render_storage_hardware_buffer_id(pair_slot);
                self.render_slots.get_mut(pair_slot).storage = None;
                self.render_slots.get_mut(pair_slot).sksm = None;
                self.render_slots.get_mut(pair_slot).submission_ordinal = 0;
                self.resources
                    .uat()
                    .restore_t8140_kernel_context_after_bootstrap_render()?;
                return Ok(());
            }
            last = Some((state, outer));
            fsleep(Delta::from_millis(1));
        }
        if let Some((state, outer)) = last {
            pr_err!(
                "G17P bootstrap timeout: TA {:?}, 3D {:?}, outer 3D/TA {:?}\n",
                state.tiling,
                state.fragment,
                outer
            );
        }
        Err(ETIMEDOUT)
    }

    pub(crate) fn prepare_compute_runtime_class1(&mut self) -> Result {
        self.resources.prepare_compute_runtime_class1()
    }

    pub(crate) fn publish_compute_runtime_control_record(
        &mut self,
        index: usize,
    ) -> Result<u32> {
        if self.primary_device_control_backpressured() {
            return Err(EAGAIN);
        }
        self.resources.publish_compute_runtime_control_record(index)
    }

    /// Append one live device-control record to the primary channel-12 ring and
    /// advance its host-owned producer. The caller owns the EP-0x21 mailbox and
    /// must send `encode_primary_device_control()` afterwards.
    pub(crate) fn publish_primary_device_control_record(
        &mut self,
        opcode: u32,
        arg: u32,
    ) -> Result<g17_resources::G17PDeviceControlPublication> {
        if self.primary_device_control_backpressured() {
            return Err(EAGAIN);
        }
        self.resources
            .publish_primary_device_control_record(opcode, arg)
    }

    pub(crate) fn primary_device_control_backpressured(&self) -> bool {
        self.pending_primary_control.is_some()
            || self.pending_render_usc_freelist_release().is_some()
    }

    pub(crate) fn primary_tvb_reply_pending(&self) -> bool {
        self.pending_primary_control.is_some()
    }

    pub(crate) fn flush_pending_primary_device_control(
        &mut self,
    ) -> Result<G17PPendingPrimaryControlFlush> {
        let Some(pending) = self.pending_primary_control.as_ref() else {
            return Ok(G17PPendingPrimaryControlFlush::Empty);
        };
        if pending.recovery_generation != self.accepted_firmware_recovery_generation {
            return Err(EIO);
        }
        let record = pending.record;
        match self.resources.publish_primary_device_control_raw(&record) {
            Ok(publication) => {
                self.pending_primary_control = None;
                if u32::from_le_bytes([record[0], record[1], record[2], record[3]]) == 8 {
                    // A deferred opcode-8 TVB grow reply (handshake step 5).
                    crate::g17_recovery_path::watch_grow_reply(publication.producer_after,
                        u32::from_le_bytes([record[8], record[9], record[10], record[11]]));
                    dev_info!(self.resources.dev().as_ref(),
                        "G17P TVB grow: deferred ack=8 published producer-after={}\n",
                        publication.producer_after);
                }
                self.rekick_parked_render_grows(None);
                Ok(G17PPendingPrimaryControlFlush::Published(publication))
            }
            Err(error) if error == EAGAIN => Ok(G17PPendingPrimaryControlFlush::Blocked),
            Err(error) => Err(error),
        }
    }

    /// Count one event-worker pass, which is followed by a one-millisecond
    /// sleep outside the runtime mutex. Inline submit-side probes do not call
    /// this and therefore cannot consume the retry budget in a tight loop.
    pub(crate) fn note_primary_device_control_backpressure(&mut self) -> Result {
        let pending = self.pending_primary_control.as_mut().ok_or(EINVAL)?;
        pending.worker_attempts = pending.worker_attempts.checked_add(1).ok_or(ETIMEDOUT)?;
        if pending.worker_attempts > G17P_PRIMARY_CONTROL_RETRY_LIMIT {
            return Err(ETIMEDOUT);
        }
        Ok(())
    }

    pub(crate) fn pending_render_usc_freelist_release(&self) -> Option<G17PBootstrapUmaRelease> {
        for bank in 0..4 {
            let mut bits = self.render_flist_scan[bank];
            while bits != 0 {
                let index = bank * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if let Some(release) = self.render_flist_owners[index].as_ref()
                    .and_then(G17PBootstrapUmaLifecycle::pending_release) { return Some(release); }
            }
        }
        None
    }

    pub(crate) fn note_flist_release_backpressure(&mut self) -> Result {
        if self.pending_render_usc_freelist_release().is_none() {
            return Err(EINVAL);
        }
        self.pending_flist_release_worker_attempts = self
            .pending_flist_release_worker_attempts
            .checked_add(1)
            .ok_or(ETIMEDOUT)?;
        if self.pending_flist_release_worker_attempts > G17P_PRIMARY_CONTROL_RETRY_LIMIT {
            return Err(ETIMEDOUT);
        }
        Ok(())
    }

    /// Grow the retained owner named by firmware, then publish its opcode-8
    /// reply. Both parked and selected pairs can be executing. Keep selection
    /// unchanged so event service cannot redirect the caller's render state.
    fn render_grow_owner_preparing(&self, request: g17_completion::G17PFirmwareTvbGrowEvent) -> bool {
        self.render_slots.slots.iter().flatten().any(|slot| slot.preparing && (
            slot.tracker.as_ref().is_some_and(|t| t.hardware_buffer_id == request.buffer_slot)
            || slot.queued_tracker.iter().any(|t| t.hardware_buffer_id == request.buffer_slot)
            || (slot.hardware_buffer_published && slot.hardware_buffer_id == Some(request.buffer_slot))))
    }

    /// Ingress only queues the complete tuple. Allocation runs on the
    /// separate non-hardware worker, keeping event drain and publication live.
    pub(crate) fn queue_render_tvb_grow(
        &mut self, request: g17_completion::G17PFirmwareTvbGrowEvent,
    ) -> Result<G17PTvbGrowDisposition> {
        let ingress = crate::g17_recovery_path::GROW_INGRESS
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
        if crate::g17_recovery_path::log_budget(ingress, 64, 256) {
            dev_info!(self.resources.dev().as_ref(),
                "G17P TVB grow request #{}: slot={} context={} counter={} subpipe={} generation={}/{} owner-preparing={} pending={} active={} backpressured={}\n",
                ingress, request.buffer_slot, request.vm_slot, request.counter, request.subpipe,
                request.halt_count, self.accepted_firmware_recovery_generation,
                self.render_grow_owner_preparing(request), self.pending_render_grows.len(),
                self.active_render_grow.is_some(), self.primary_device_control_backpressured());
        }
        if request.halt_count != self.accepted_firmware_recovery_generation {
            return Ok(G17PTvbGrowDisposition::IgnoredStale);
        }
        if self.active_render_grow == Some(request) {
            return Ok(G17PTvbGrowDisposition::PreparationDeferred);
        }
        if !self.pending_render_grows.contains(&request) {
            if self.pending_render_grows.len() == self.pending_render_grows.capacity() {
                return Err(EOVERFLOW);
            }
            self.pending_render_grows.push(request, GFP_KERNEL)?;
        }
        crate::driver::queue_g17p_tvb_grow_worker(self.resources.owned_dev());
        Ok(G17PTvbGrowDisposition::PreparationDeferred)
    }

    pub(crate) fn take_render_grow(&mut self) -> Result<Option<G17PRenderGrow>> {
        if self.primary_device_control_backpressured() || self.active_render_grow.is_some() { return Ok(None); }
        loop {
            let Some(index) = self.pending_render_grows.iter().position(|request|
                request.halt_count != self.accepted_firmware_recovery_generation
                    || !self.render_grow_owner_preparing(*request)) else {
                if !self.pending_render_grows.is_empty() {
                    // Every parked request's pair is borrowed right now. The
                    // worker exits; see GROW_REKICK for who retries it.
                    let deferred = crate::g17_recovery_path::GROW_DEFERRED
                        .fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
                    if crate::g17_recovery_path::log_budget(deferred, 32, 1024) {
                        let first = self.pending_render_grows[0];
                        dev_info!(self.resources.dev().as_ref(),
                            "G17P TVB grow deferred #{}: {} request(s) parked, owner pair borrowed (first slot={} counter={} subpipe={}); rekick={}\n",
                            deferred, self.pending_render_grows.len(), first.buffer_slot, first.counter,
                            first.subpipe,
                            crate::g17_recovery_path::enabled(crate::g17_recovery_path::GROW_REKICK));
                    }
                }
                return Ok(None);
            };
            let request = self.pending_render_grows.remove(index).map_err(|_| EIO)?;
            if request.halt_count != self.accepted_firmware_recovery_generation { continue; }
            let owner = self.render_slots.slots.iter().enumerate().find_map(|(index, slot)| {
                let slot = slot.as_deref()?;
                let published = slot.tracker.as_ref().is_some_and(|t| t.hardware_buffer_id == request.buffer_slot)
                    || slot.queued_tracker.iter().any(|t| t.hardware_buffer_id == request.buffer_slot)
                    || (slot.hardware_buffer_published && slot.hardware_buffer_id == Some(request.buffer_slot));
                let storage = slot.storage.as_deref()?;
                let bind = slot.bind.as_ref()?;
                if !published || slot.preparing || !request.matches_owner(
                    self.accepted_firmware_recovery_generation,
                    storage.retained_hardware_buffer_id()?, u32::from(bind.root_slot().ok()?)) { return None; }
                Some(index as u8)
            });
            let (storage, vm) = if let Some(index) = owner {
                let slot = self.render_slots.get_mut(index);
                let vm = slot.bind.as_ref().ok_or(EIO)?.vm().clone();
                slot.preparing = true;
                (slot.storage.take(), Some(vm))
            } else { (None, None) };
            self.active_render_grow = Some(request);
            let taken = G17P_GROW_TAKEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
            if crate::g17_recovery_path::log_budget(taken, 64, 256) {
                dev_info!(self.resources.dev().as_ref(),
                    "G17P TVB grow take #{}: slot={} counter={} subpipe={} owner-pair={:?} still-parked={}\n",
                    taken, request.buffer_slot, request.counter, request.subpipe, owner,
                    self.pending_render_grows.len());
            }
            return Ok(Some(G17PRenderGrow { request, pair_slot: owner, storage, vm }));
        }
    }

    pub(crate) fn return_render_grow(&mut self, mut grow: G17PRenderGrow,
        result: Result<bool>, commit: bool) -> Result<G17PTvbGrowDisposition> {
        if let Some(index) = grow.pair_slot {
            let slot = self.render_slots.get_mut(index);
            assert!(slot.preparing && slot.storage.is_none());
            let storage = grow.storage.take().expect("grow owns borrowed graph");
            if self.render_flist_owners[slot.flist_index].as_ref().is_some_and(|owner|
                owner.request.is_some() && matches!(owner.state, G17PBootstrapUmaRequestState::Ready)
                    && owner.matches_owner(storage.render_flist_id(), storage.render_flist_pointer(),
                        storage.render_flist_operand_table(), storage.render_flist_cookie())
                    && owner.same_pool_object(storage.render_flist_serial())) {
                storage.mark_render_flist_grown();
            }
            slot.storage = Some(storage);
            slot.preparing = false;
        }
        assert_eq!(self.active_render_grow.take(), Some(grow.request));
        if !commit { return Ok(G17PTvbGrowDisposition::IgnoredStale); }
        let grew = match result {
            Ok(grew) => grew,
            Err(error) => {
                dev_warn!(self.resources.dev().as_ref(),
                    "G17P TVB grow: off-runtime allocation failed slot={} context={} ({:?})\n",
                    grow.request.buffer_slot, grow.request.vm_slot, error);
                false
            }
        };
        self.publish_render_tvb_grow_reply(grow.request, grew)
    }

    /// Replay ready owners without waiting for another firmware edge. Keep
    /// blocked owners' complete request (counter/subpipe/generation included).
    pub(crate) fn replay_prepared_render_grows(&mut self,
        announce: &mut impl FnMut() -> Result) -> Result {
        while !self.primary_device_control_backpressured() {
            let Some(index) = self.pending_render_grows.iter().position(|request|
                request.halt_count != self.accepted_firmware_recovery_generation
                    || !self.render_grow_owner_preparing(*request)) else { break };
            let request = self.pending_render_grows.remove(index).map_err(|_| EIO)?;
            match self.service_render_tvb_grow(request)? {
                G17PTvbGrowDisposition::Published => announce()?,
                G17PTvbGrowDisposition::Deferred => break,
                G17PTvbGrowDisposition::IgnoredStale => {},
                G17PTvbGrowDisposition::PreparationDeferred => return Err(EIO),
            }
        }
        Ok(())
    }

    /// The caller owns the primary DeviceControl doorbell.
    pub(crate) fn service_render_tvb_grow(
        &mut self,
        request: g17_completion::G17PFirmwareTvbGrowEvent,
    ) -> Result<G17PTvbGrowDisposition> {
        let pair_slot = 0;
        if self.primary_device_control_backpressured() {
            return Err(EBUSY);
        }
        let generation = self.accepted_firmware_recovery_generation;
        if request.halt_count != generation {
            dev_warn!(self.resources.dev().as_ref(),
                "G17P TVB grow: ignoring stale request slot={} generation={} current={}\n",
                request.buffer_slot, request.halt_count, generation);
            return Ok(G17PTvbGrowDisposition::IgnoredStale);
        }
        if self.render_grow_owner_preparing(request) {
            if !self.pending_render_grows.contains(&request) {
                // Allocation was reserved before firmware ingress started.
                // Exceeding both rings' capacity indicates a protocol fault;
                // preserve existing requests for processor-stop recovery.
                if self.pending_render_grows.len() == self.pending_render_grows.capacity() {
                    return Err(EOVERFLOW);
                }
                self.pending_render_grows.push(request, GFP_KERNEL)?;
            }
            return Ok(G17PTvbGrowDisposition::PreparationDeferred);
        }
        let grow_owner = |resources: &mut g17_resources::G17ResourceOwner,
                          storage: Option<&mut g17_resources::G17PUserRenderStorage>,
                          bind: Option<&G17PRenderRootOwner>,
                          published: bool| -> Result<bool> {
            let storage = storage.ok_or(ENOENT)?;
            let bind = bind.ok_or(ENOENT)?;
            let id = storage.retained_hardware_buffer_id().ok_or(ENOENT)?;
            if !published || !request.matches_owner(generation, id, bind.root_slot()? as u32) {
                return Err(EINVAL);
            }
            resources.grow_render_tvb_synchronously(storage, bind.vm())
        };
        let selected_published = self.render_slots.get(pair_slot).tracker.as_ref()
            .is_some_and(|tracker| tracker.hardware_buffer_id == request.buffer_slot)
            || self.render_slots.get(pair_slot).queued_tracker.iter()
                .any(|tracker| tracker.hardware_buffer_id == request.buffer_slot)
            || (self.render_slots.get(pair_slot).hardware_buffer_published
                && self.render_slots.get(pair_slot).hardware_buffer_id == Some(request.buffer_slot));
        let result = if selected_published {
            let slot = self.render_slots.get_mut(pair_slot);
            grow_owner(&mut self.resources, slot.storage.as_deref_mut(),
                slot.bind.as_ref(), true)
        } else if let Some(owner) = self.render_slots.parked_mut(pair_slot)
            .find(|owner| {
                owner.tracker.as_ref()
                    .is_some_and(|tracker| tracker.hardware_buffer_id == request.buffer_slot)
                    || owner.queued_tracker.iter()
                        .any(|tracker| tracker.hardware_buffer_id == request.buffer_slot)
                    || (owner.hardware_buffer_published
                        && owner.hardware_buffer_id == Some(request.buffer_slot))
            })
        {
            grow_owner(&mut self.resources, owner.storage.as_deref_mut(), owner.bind.as_ref(),
                true)
        } else {
            Err(ENOENT)
        };
        // Failure is still a reply: claiming success or ignoring a current
        // request would leave firmware with a fictitious inventory or no wake.
        let grew = match result {
            Ok(grew) => grew,
            Err(error) => {
                dev_warn!(self.resources.dev().as_ref(),
                    "G17P TVB grow: slot={} context={} allocation failed ({:?})\n",
                    request.buffer_slot, request.vm_slot, error);
                false
            }
        };
        self.publish_render_tvb_grow_reply(request, grew)
    }

    fn publish_render_tvb_grow_reply(&mut self,
        request: g17_completion::G17PFirmwareTvbGrowEvent, grew: bool,
    ) -> Result<G17PTvbGrowDisposition> {
        let generation = self.accepted_firmware_recovery_generation;
        // Reset may complete while the worker allocates; never wake firmware
        // with an old request's counter or a previous pool generation.
        if request.halt_count != generation { return Ok(G17PTvbGrowDisposition::IgnoredStale); }
        let record = request.reply(grew);
        match self.resources.publish_primary_device_control_raw(&record) {
            Ok(publication) => {
                crate::g17_recovery_path::watch_grow_reply(publication.producer_after, request.buffer_slot);
                crate::g17_recovery_path::GROW_REPLIED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                dev_info!(self.resources.dev().as_ref(),
                    "G17P TVB grow: slot={} context={} counter={} subpipe={} generation={} grew={} ack=8\n",
                    request.buffer_slot, request.vm_slot, request.counter, request.subpipe,
                    request.halt_count, grew);
                Ok(G17PTvbGrowDisposition::Published)
            }
            Err(error) if error == EAGAIN => {
                if self.pending_primary_control.is_some() {
                    return Err(EBUSY);
                }
                self.pending_primary_control = Some(G17PPendingPrimaryControl {
                    record,
                    recovery_generation: generation,
                    worker_attempts: 0,
                });
                Ok(G17PTvbGrowDisposition::Deferred)
            }
            Err(error) => Err(error),
        }
    }

    /// Activate this prepared pair's FList lease. Only the cold lease has a
    /// pending async grow to publish; a warm pair becomes Ready without 0x20.
    pub(crate) fn publish_render_usc_freelist_request(
        &mut self, pair_slot: u8,
    ) -> Result<Option<g17_resources::G17PDeviceControlPublication>> {
        if self.render_slots.get(pair_slot).storage.is_none() {
            return Err(EINVAL);
        }
        if !self.render_slots.get(pair_slot).flist_leases.has_room() { return Err(EBUSY); }
        let mut lifecycle = self.render_slots.get(pair_slot).prepared_render_flist.ok_or(EIO)?;
        lifecycle.retire_prepare_reference().map_err(|_| EIO)?;
        if lifecycle.request.is_none() || lifecycle.request_published {
            let ordinal = self.render_slots.get(pair_slot).submission_ordinal;
            self.render_slots.get_mut(pair_slot).flist_leases
                .push((ordinal, lifecycle.sequence, lifecycle.hardware_buffer_id)).map_err(|_| EIO)?;
            self.render_flist_owners[self.render_slots.get(pair_slot).flist_index] = Some(lifecycle);
            let index = lifecycle.hardware_buffer_id as usize;
            self.render_flist_scan[index / 64] |= 1u64 << (index % 64);
            self.render_slots.get_mut(pair_slot).prepared_render_flist = None;
            return Ok(None);
        }
        if self.primary_device_control_backpressured() {
            return Err(EAGAIN);
        }
        let mut record = [0u8; g17_initdata::CONTROL_RECORD_SIZE];
        g17_submission::encode_g17p_usc_freelist_control(
            lifecycle.shared_control,
            lifecycle.operand_table,
            lifecycle.cookie,
            g17_submission::G17PUscFreelistSequence {
                word_04: G17P_BOOTSTRAP_UMA_STAMP as u32,
                word_0c: u32::try_from(lifecycle.sequence).map_err(|_| EOVERFLOW)?,
                word_30: lifecycle.hardware_buffer_id,
            },
            &mut record,
        )
        .map_err(|_| EINVAL)?;

        let publication = self
            .resources
            .publish_primary_device_control_raw(&record)?;
        lifecycle.request_published = true;
        let ordinal = self.render_slots.get(pair_slot).submission_ordinal;
        self.render_slots.get_mut(pair_slot).flist_leases
            .push((ordinal, lifecycle.sequence, lifecycle.hardware_buffer_id)).map_err(|_| EIO)?;
        self.render_flist_owners[self.render_slots.get(pair_slot).flist_index] = Some(lifecycle);
        let index = lifecycle.hardware_buffer_id as usize;
        self.render_flist_scan[index / 64] |= 1u64 << (index % 64);
        self.render_slots.get_mut(pair_slot).prepared_render_flist = None;
        Ok(Some(publication))
    }

    /// Match a firmware type-13 completion to the retained FList request.
    /// This retires the async-grow reference only; TA and 3D still own it.
    pub(crate) fn match_render_usc_freelist_completion(
        &mut self, completion: G17PBootstrapUmaCompletion,
    ) -> Result {
        let lifecycle = self.render_flist_owners.get_mut(completion.hardware_buffer_id as usize)
            .and_then(Option::as_mut).ok_or(EIO)?;
        lifecycle.match_completion(completion).map_err(|_| EIO)?;
        for state in self.render_slots.slots.iter().flatten() {
            if let Some(storage) = state.storage.as_deref() {
                if lifecycle.matches_owner(storage.render_flist_id(), storage.render_flist_pointer(),
                    storage.render_flist_operand_table(), storage.render_flist_cookie())
                    && lifecycle.same_pool_object(storage.render_flist_serial()) {
                    storage.mark_render_flist_grown();
                }
            }
        }
        // Borrowed graphs apply the same matched tuple when they return.
        Ok(())
    }

    /// Whether the asynchronous opcode-0x20 grow has retired its own
    /// reference. TA/3D references intentionally remain live until paired
    /// render completion.
    pub(crate) fn render_usc_freelist_grow_matched(&self, pair_slot: u8) -> bool {
        self.render_flist_owners[self.render_slots.get(pair_slot).flist_index]
            .as_ref()
            .is_some_and(|lifecycle| lifecycle.state == G17PBootstrapUmaRequestState::Ready)
    }

    /// Retire the TA and 3D FList references after paired render completion.
    /// Only this final native reference drop authorizes opcode 0x2e.
    pub(crate) fn retire_render_usc_freelist_at_completion(
        &mut self, pair_slot: u8,
    ) -> Result<Option<G17PBootstrapUmaRelease>> {
        if crate::g17_config::RENDER_USC_FREELIST == 0 { return Ok(None); }
        let state = self.render_slots.get(pair_slot);
        let ordinal = state.flist_retired_ordinal.ok_or(EIO)?;
        let (expected, sequence, id) = *state.flist_leases.front().ok_or(EIO)?;
        if ordinal != expected || id as usize != state.flist_index { return Err(EIO); }
        let mut owner = self.render_flist_owners[id as usize].ok_or(EIO)?;
        if owner.sequence != sequence { return Err(EIO); }
        let release = owner.retire_render_references().map_err(|_| EIO)?;
        self.render_flist_owners[id as usize] = Some(owner);
        let state = self.render_slots.get_mut(pair_slot);
        state.flist_leases.take();
        state.flist_retired_ordinal = None;
        Ok(release)
    }

    /// Publish the release generated by this pair's final reference drop.
    /// The caller still owns the EP-0x21 announcement.
    pub(crate) fn publish_render_usc_freelist_release(
        &mut self,
        release: G17PBootstrapUmaRelease,
    ) -> Result<g17_resources::G17PDeviceControlPublication> {
        let index = release.hardware_buffer_id as usize;
        if index >= self.render_flist_owners.len() { return Err(EINVAL); }
        if self.pending_primary_control.is_some() {
            return Err(EAGAIN);
        }
        let pending = self.render_flist_owners[index]
            .as_ref()
            .and_then(G17PBootstrapUmaLifecycle::pending_release);
        if pending != Some(release) {
            dev_warn!(
                self.resources.dev().as_ref(),
                "G17P render: FList release mismatch pending={:?} wanted={:?}\n",
                pending,
                release,
            );
            return Err(EIO);
        }
        let mut record = [0u8; g17_initdata::CONTROL_RECORD_SIZE];
        g17_initdata::encode_bootstrap_uma_release_record(
            release.sequence,
            release.hardware_buffer_id,
            &mut record,
        )
        .map_err(|_| EINVAL)?;
        let publication = self
            .resources
            .publish_primary_device_control_raw(&record)?;
        self.render_flist_owners[index]
            .as_mut()
            .ok_or(EIO)?
            .mark_published(release)
            .map_err(|_| EIO)?;
        self.pending_flist_release_worker_attempts = 0;
        self.rekick_parked_render_grows(None);
        Ok(publication)
    }

    pub(crate) fn mark_render_usc_freelist_release_sent(
        &mut self, release: G17PBootstrapUmaRelease, producer_after: u32,
    ) -> Result {
        let owner = self.render_flist_owners.get_mut(release.hardware_buffer_id as usize)
            .and_then(Option::as_mut).ok_or(EIO)?;
        if owner.sequence != release.sequence { return Err(EIO); }
        owner.mark_sent(producer_after).map_err(|_| EIO)
    }

    /// v92c FList takeover guard. True when `pair_slot` still carries an
    /// unobserved 0x2e release witness (its own sent release, or one carried
    /// from a predecessor) and its root is not already owned by `context`.
    /// Taking such a pair over stages a new pool's image for the same FList
    /// ID while firmware may still process the predecessor's release for that
    /// ID. The fault seen on v92 (boot fc063e77) is a 3D GMMU fault inside the
    /// predecessor client's pool page list, in the successor's VM. A pair the
    /// same context already owns keeps its own image; an unallocated pair has
    /// no witness.
    pub(crate) fn render_flist_takeover_blocked(
        &self, _pair_slot: u8,
        _context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
    ) -> bool {
        // An incoming pool receives an independent ID; physical pair reuse
        // cannot replace the old pool's UMA row. Its context/teardown still
        // retains that pool until its own release is consumed.
        false
    }

    /// v92c: primary device-control ring (consumer, producer) snapshot.
    pub(crate) fn primary_control_cursors(&mut self) -> Result<(u32, u32)> {
        let counters = self.resources.control_counters()?;
        Ok((
            counters.primary[g17_initdata::CHANNEL_STATE_CONSUMER],
            counters.primary[g17_initdata::CHANNEL_STATE_PRODUCER],
        ))
    }

    /// Settle every sent owner against one coherent control-ring snapshot.
    /// This is allocation-free, sleep-free and independent of slot selection.
    pub(crate) fn settle_consumed_render_usc_freelist_releases(&mut self) -> Result<u32> {
        let counters = self.resources.control_counters()?;
        let consumer = counters.primary[g17_initdata::CHANNEL_STATE_CONSUMER];
        let producer = counters.primary[g17_initdata::CHANNEL_STATE_PRODUCER];
        let mut settled = 0u32;
        for bank in 0..4 {
            let mut bits = self.render_flist_scan[bank];
            while bits != 0 {
                let bit = bits.trailing_zeros();
                bits &= bits - 1;
                let index = bank * 64 + bit as usize;
                let owner = self.render_flist_owners[index].as_mut().ok_or(EIO)?;
                if owner.has_release_witness() {
                    settled = settled.checked_add(owner.settle_release_witnesses(consumer, producer)
                        .map_err(|_| EIO)?).ok_or(EOVERFLOW)?;
                }
                if owner.state == G17PBootstrapUmaRequestState::Completed
                    && !owner.hardware_buffer_owned && !owner.has_release_witness() {
                    self.render_flist_scan[bank] &= !(1u64 << bit);
                }
            }
        }
        Ok(settled)
    }

    pub(crate) fn prepare_compute_runtime_class2(&mut self) -> Result {
        self.resources.prepare_compute_runtime_class2()
    }

    pub(crate) fn render_flist_owned(&self, pair_slot: u8) -> bool {
        self.render_slots.get(pair_slot).storage.is_some()
    }

    pub(crate) fn stage_compute_flist_state(&mut self) -> Result {
        self.resources.stage_compute_flist_state()
    }

    pub(crate) fn rewrite_compute_dispatch_record(&mut self) -> Result {
        self.resources.rewrite_compute_dispatch_record()
    }

    pub(crate) fn uat(&self) -> &mmu::Uat {
        self.resources.uat()
    }

    pub(crate) fn mark_firmware_cache_flush_ready(&self) {
        self.resources.mark_firmware_cache_flush_ready();
    }

    fn validate_partial_opening_resources(&self, pair_slot: u8) -> Result {
        let binding = self.resources.partial_opening_binding();
        let context = g17_submission::G17P_PARTIAL_OPENING_CONTEXT;
        let expected_fwctl = binding
            .kernel_va_base
            .checked_add(g17_submission::G17P_PARTIAL_OPENING_FWCTL_OFFSET)
            .ok_or(EINVAL)?;
        let addresses_ready = binding.scheduler_page
            == g17_submission::G17P_PARTIAL_OPENING_SCHEDULER_GPU_VA
            && binding.primary_index_page
                == g17_submission::G17P_PARTIAL_OPENING_PRIMARY_INDEX_GPU_VA
            && binding.fwctl == expected_fwctl;
        let context_ready = context.context0_root_slot == 0
            && context.context0_context_id == 0
            && context.render_root_slot == 1
            && context.render_context_id == 1
            && context.context0_empty_high_root_slot == 0
            && context.render_empty_high_root_slot == 1
            && context.transport_pair == 0
            && context.descriptor_pair == 0
            && context.shared_queue_pair_namespace == 0
            && context.optional_identity_context_id == 1
            && context.descriptor_context_id == 1
            && context.scheduler_class == 2
            && context.work_doorbell_channel == 0
            && context.context0_low_root_present_at_first_work
            && context.render_low_root_present_at_first_work
            && !context.source_firmware_high_root_table_resident;
        let roots_ready = match self.render_slots.get(pair_slot).bind.as_ref() {
            Some(binding) => binding.bootstrap().is_some_and(|binding| self
                .resources
                .uat()
                .t8140_partial_opening_shared_render_context_ready(binding)),
            None => self
                .resources
                .uat()
                .t8140_partial_opening_kernel_contexts_ready(),
        };
        if !addresses_ready || !context_ready || !roots_ready {
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P render: opening contract addresses={} context={} roots={} (scheduler {:#x}, index {:#x}, fwctl {:#x}/{:#x})\n",
                addresses_ready,
                context_ready,
                roots_ready,
                binding.scheduler_page,
                binding.primary_index_page,
                binding.fwctl,
                expected_fwctl
            );
            return Err(EINVAL);
        }
        Ok(())
    }

    /// Apply first-work status immediately before the caller sends primary
    /// control-done 0x84.
    pub(crate) fn prepare_partial_opening_control_done(&mut self, pair_slot: u8) -> Result {
        self.validate_partial_opening_resources(pair_slot).inspect_err(|error| {
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P render: opening validation failed ({:?})\n",
                error
            );
        })?;
        self.resources
            .apply_partial_opening_pre_control_status()
            .inspect_err(|error| {
                dev_err!(
                    self.resources.dev().as_ref(),
                    "G17P render: status-B update failed ({:?})\n",
                    error
                );
            })
    }

    /// Publish the first-partial operand table in the control-done to
    /// first-work interval.
    pub(crate) fn publish_partial_opening_operand_table(&mut self) -> Result {
        self.resources.publish_partial_opening_operand_table()
    }

    /// Publish the exact first 3D/TA outer-ring pair. The callback installs
    /// the late TA descriptor, item-ring, and queue-head state between the 3D
    /// producer and the TA outer slot. The retained live runtime sends the
    /// returned channel-0 doorbell exactly once.
    pub(crate) fn submit_partial_opening_render<F>(
        &mut self,
        submission: G17PPartialOpeningRenderSubmission,
        publish_late_tiling_state: F,
    ) -> Result<(
        G17PPartialOpeningRenderTracker,
        g17_submission::PreparedG17PPartialOpeningWorkPair,
    )>
    where
        F: FnOnce(),
    {
        self.validate_partial_opening_resources(0)?;
        if submission.fragment_queue_write_index != 3 || submission.tiling_queue_write_index != 3 {
            return Err(EINVAL);
        }
        let prepared = self.resources.publish_partial_opening_pair(
            false,
            g17_resources::G17PRenderQueuePair::for_slot(0).ok_or(EINVAL)?,
            g17_resources::G17PDeferredOuterPublication::None,
            submission.fragment_queue_gpu_va,
            submission.fragment_queue_write_index,
            submission.tiling_queue_gpu_va,
            submission.tiling_queue_write_index,
            true,
            || Ok(()),
            publish_late_tiling_state,
        )?;
        if prepared.paired_doorbell != g17_submission::G17P_PARTIAL_OPENING_PAIRED_DOORBELL
            || prepared.paired_doorbell != 0x0083_0000_0000_0000
        {
            return Err(EINVAL);
        }
        let tracker = G17PPartialOpeningRenderTracker {
            ordinal: 0,
            hardware_buffer_id: 0,
            command_buffer_state_word: 0,
            work_state: None,
            native_timestamps: None,
            qos_pending: false,
            pool_a_record_index: 0,
            pool_a_submitted: 0,
            context_update: false,
            tiling_previous: g17_completion::QueueIndices {
                done: 0,
                read: 0,
                write: submission.tiling_queue_write_index,
            },
            fragment_previous: g17_completion::QueueIndices {
                done: 0,
                read: 0,
                write: submission.fragment_queue_write_index,
            },
            tiling_prefix: submission.tiling_queue_write_index,
            fragment_prefix: submission.fragment_queue_write_index,
            windows: [g17_completion::RenderQueueWindow::new(0, 3, 0x500)
                .map_err(|_| EINVAL)?; 2],
        };
        Ok((tracker, prepared))
    }

    /// Classify paired render progress from queue indices plus the scheduler
    /// job list and both descriptor timestamp quartets.
    pub(crate) fn observe_partial_opening_render_completion(
        &self,
        tracker: &mut G17PPartialOpeningRenderTracker,
        tiling_current: g17_completion::QueueIndices,
        fragment_current: g17_completion::QueueIndices,
        execution: g17_completion::RenderExecutionState,
    ) -> Result<g17_completion::PairedRenderCompletion> {
        let completion = g17_completion::observe_paired_render_completion(
            tracker.tiling_previous,
            tiling_current,
            tracker.tiling_prefix,
            tracker.fragment_previous,
            fragment_current,
            tracker.fragment_prefix,
            execution,
        )
        .map_err(|_| EINVAL)?;
        tracker.tiling_previous = tiling_current;
        tracker.fragment_previous = fragment_current;
        Ok(completion)
    }

    pub(crate) fn new_user_vm(
        &mut self,
        id: u64,
        kernel_range: core::ops::Range<u64>,
    ) -> Result<mmu::Vm> {
        self.new_user_vm_inner(id, kernel_range, false)
    }

    fn new_user_vm_inner(
        &mut self,
        id: u64,
        kernel_range: core::ops::Range<u64>,
        bootstrap_vdm: bool,
    ) -> Result<mmu::Vm> {
        let vm = self
            .resources
            .uat()
            .new_vm(id, kernel_range)
            .inspect_err(|error| {
                dev_err!(
                    self.resources.dev().as_ref(),
                    "G17P VM: context allocation failed ({:?})\n",
                    error
                );
            })?;
        self.resources
            .install_user_vm_aliases(&vm, bootstrap_vdm)
            .inspect_err(|error| {
                dev_err!(
                    self.resources.dev().as_ref(),
                    "G17P VM: fixed aliases failed ({:?})\n",
                    error
                );
            })?;
        Ok(vm)
    }

    pub(crate) fn map_timestamp_buffer(
        &self,
        mut bo: crate::gem::ObjectRef,
        range: core::ops::Range<usize>,
    ) -> Result<mmu::KernelMapping> {
        bo.map_range_into_range(
            self.resources.uat().kernel_vm(),
            range,
            g17_resources::g17p_dynamic_kernel_va_range(
                self.resources.uat().kernel_va_range()?).ok_or(ERANGE)?,
            mmu::UAT_PGSZ as u64,
            mmu::PROT_FW_SHARED_RW,
            false,
        )
    }

    /// Release one stopped firmware session without replacing its UAT owner.
    pub(crate) fn release_runtime_objects_after_processor_stop(&mut self) {
        let pair_slot = 0;
        for mut state in self.render_slots.take_parked(pair_slot) {
            if let Some(id) = state.hardware_buffer_id.take() {
                if let Err(error) = self.render_hardware_buffer_ids.release(id) {
                    dev_warn!(
                        self.resources.dev().as_ref(),
                        "G17P render: failed to release parked TA HardwareBufferID {} after processor stop ({:?})\n",
                        id,
                        error,
                    );
                }
            }
            if let Some(id) = state
                .storage
                .as_deref()
                .and_then(g17_resources::G17PUserRenderStorage::retained_hardware_buffer_id)
            {
                self.render_hardware_buffer_ids.unreserve(id);
            }
            // Both firmware CPUs are stopped; dropping the complete state now
            // retracts its command root before any reachable mapping backing.
            drop(state);
        }
        if let Some(id) = self.render_slots.get_mut(pair_slot).hardware_buffer_id.take() {
            match self.render_hardware_buffer_ids.release(id) {
                Ok(()) => dev_info!(
                    self.resources.dev().as_ref(),
                    "G17P render: released TA HardwareBufferID {} after processor stop\n",
                    id,
                ),
                Err(error) => dev_warn!(
                    self.resources.dev().as_ref(),
                    "G17P render: failed to release TA HardwareBufferID {} after processor stop ({:?})\n",
                    id,
                    error,
                ),
            }
        }
        self.render_slots.get_mut(pair_slot).hardware_buffer_published = false;
        self.render_outer_channels_started = [false; 64];
        self.compute_channel_control = None;
        self.render_slots.get_mut(pair_slot).channel_control = None;
        self.logical_qos_ids = None;
        self.compute_b2_activation = g17_submission::G17PClB2ActivationState::Pending;
        self.render_slots.get_mut(pair_slot).context_retirement = None;
        self.render_context_epochs = [0; 64];
        self.render_slots.get_mut(pair_slot).bind = None;
        self.render_slots.get_mut(pair_slot).tracker = None;
        self.render_slots.get_mut(pair_slot).queued_tracker.clear();
        self.unreserve_render_storage_hardware_buffer_id(pair_slot);
        self.render_slots.get_mut(pair_slot).storage = None;
        // Both CPUs are stopped at this boundary. The alias owner's Drop
        // removes the context-0 leaves and invalidates ASIDs 0/1.
        self.render_slots.get_mut(pair_slot).context_aliases = None;
        // Both processors are stopped: every outstanding scheduler-state
        // release is moot, and every graph a teardown still holds is freed
        // with it, root first.
        self.logical_queue_teardowns.clear();
        self.render_slots.get_mut(pair_slot).sksm = None;
        self.render_flist_owners.fill(None);
        self.render_flist_scan = [0; 4];
        self.render_slots.get_mut(pair_slot).flist_index = 0;
        self.render_slots.get_mut(pair_slot).flist_leases.clear();
        self.render_slots.get_mut(pair_slot).flist_retired_ordinal = None;
        self.render_slots.get_mut(pair_slot).prepared_render_flist = None;
        self.render_slots.get_mut(pair_slot).submission_ordinal = 0;
        self.render_slots.get_mut(pair_slot).auxiliary_timestamp = 0;
        self.next_render_pm_manager_generation =
            g17_resources::G17P_PM_INITIAL_HWPB_MANAGER_GENERATION;
        self.render_parameter_buffer_owner = 0;
        self.render_command_buffer_state_pairs = [const { None }; 64];
        // Both processors are stopped and every retained compute/render graph
        // is gone, so queue-lifetime HardwareBufferIDs can be allocated from
        // the native cold-session order again (render/bootstrap 0, compute 1+).
        self.render_hardware_buffer_ids = G17PHardwareBufferIdAllocator::new();
        // Every compute graph and the boot graph have been dropped after both
        // processors stopped. Their QIDs are no longer installed in firmware,
        // so the replacement session owns a fresh namespace. Leaving this at
        // its previous high-water mark exhausted QID 127 after enough bounded
        // recoveries even though no hardware graph survived any of them.
        self.channel_ids = g17p_initial_channel_table();
        self.render_channel_leases = [None; 64];
        // Keep host identities monotonic across restarts. An old packet's
        // cached lease must not validate against a replacement QID owner.
        // These values name the stopped session's descriptor and deferred
        // AddKicks record. Replaying either against the replacement boot graph
        // can acknowledge or kick the wrong QID even when that QID number was
        // legitimately reused above.
        self.pending_add_kicks = None;
        self.last_published_descriptor = 0;
        self.last_completion_end = 0;
        self.compute_stamp_armed = false;
    }

    /// Restore cold initdata in the existing mappings after both CPUs stop.
    pub(crate) fn reset_session_after_processor_stop(&mut self) -> Result {
        let pair_slot = 0;
        if self.render_slots.get(pair_slot).bind.is_some()
            || self.render_slots.get(pair_slot).tracker.is_some()
            || self.render_slots.get(pair_slot).queued_tracker.is_some()
            || self.render_slots.get(pair_slot).storage.is_some()
            || self.render_slots.get(pair_slot).context_aliases.is_some()
            || self.render_slots.get(pair_slot).context_retirement.is_some()
            || !self.logical_queue_teardowns.is_empty()
            || self.parked_render_scheduler_active()
        {
            return Err(EBUSY);
        }
        // Both firmware CPUs are stopped, so no old-session control record can
        // still require delivery. Clear deferred host work before the fallible
        // resource rewrite; a failed rewrite must not leave it eligible for a
        // later retry against partially reset mappings.
        self.pending_primary_control = None;
        self.pending_render_grows.clear();
        self.pending_flist_release_worker_attempts = 0;
        self.resources.reset_session_after_processor_stop()?;
        self.accepted_firmware_recovery_generation = 0;
        self.pending_primary_recovery = None;
        Ok(())
    }

    /// The retained graph and global aliases must agree on the incoming VM and
    /// descriptor context. A VM can own several logical queues, but each queue
    /// receives an independent application-GART context. Reusing a sibling
    /// queue's VM-owned graph requires retargeting that encoded context first.
    pub(crate) fn retained_render_has_other_owner(
        &self, pair_slot: u8,
        vm: &mmu::Vm,
        descriptor_context_id: u16,
    ) -> bool {
        let graph_retained = self.render_slots.get(pair_slot).storage.is_some();
        let aliases_retained = self.render_slots.get(pair_slot).context_aliases.is_some();
        let retained = graph_retained || aliases_retained;
        let graph_same_owner = !graph_retained
            || self.render_slots.get(pair_slot).bind.as_ref().is_some_and(|binding| {
                binding.matches(vm)
                    && self.render_slots.get(pair_slot).storage.as_deref().is_some_and(|storage| {
                        storage.descriptor_context_id() == descriptor_context_id
                            && vm.render_pool_mappings_match(
                                storage.queue_pair().slot(),
                                storage.render_pool_id(),
                            )
                    })
            });
        let aliases_same_vm = self.render_slots.get(pair_slot)
            .context_aliases
            .as_ref()
            .map_or(true, |aliases| aliases.matches(vm));
        let same_owner = graph_same_owner && aliases_same_vm;
        retained && !same_owner
    }

    /// Failure-only DRAM census. A firmware-branch failure supplies the exact
    /// snapshot that rejected handoff; a host-branch failure takes one diagnostic
    /// snapshot. Never poll, select a queue, access registers, or change owners.
    fn report_render_handoff_busy(
        &mut self, pair_slot: u8,
        vm: &mmu::Vm,
        descriptor_context_id: u16,
        reason: &'static str,
        snapshot: Option<Result<g17_resources::G17PUserRenderQueueState>>,
    ) {
        let retained_bind = self.render_slots.get(pair_slot).bind.as_ref()
            .map(|binding| (binding.vm_id(), binding.root()));
        let retained_storage = self.render_slots.get(pair_slot).storage.as_deref().map(|storage| (
            storage.queue_pair().slot(),
            storage.descriptor_context_id(),
            storage.render_pool_id(),
            storage.context_update_pending(),
        ));
        let matches = (
            self.render_slots.get(pair_slot).bind.as_ref().is_some_and(|binding| binding.matches(vm)),
            self.render_slots.get(pair_slot).storage.as_deref().is_some_and(|storage| {
                storage.descriptor_context_id() == descriptor_context_id
            }),
            self.render_slots.get(pair_slot).storage.as_deref().is_some_and(|storage| {
                vm.render_pool_mappings_match(storage.queue_pair().slot(), storage.render_pool_id())
            }),
            self.render_slots.get(pair_slot).context_aliases.as_ref().map_or(true, |aliases| aliases.matches(vm)),
        );
        let host_mask = u32::from(self.render_slots.get(pair_slot).tracker.is_some())
            | (u32::from(self.render_slots.get(pair_slot).queued_tracker.is_some()) << 1)
            | (u32::from(self.render_slots.get(pair_slot).hardware_buffer_id.is_some()) << 2)
            | (u32::from(self.render_slots.get(pair_slot).hardware_buffer_published) << 3)
            | (u32::from(self.render_slots.get(pair_slot).prepared_render_flist.is_some()) << 4)
            | (u32::from(self.render_slots.get(pair_slot).submission_ordinal == 0) << 5);
        let host = (
            self.render_slots.get(pair_slot).submission_ordinal,
            self.render_slots.get(pair_slot).tracker.as_ref().map(|tracker| tracker.ordinal),
            self.render_slots.get(pair_slot).queued_tracker.front().map(|tracker| tracker.ordinal),
            self.render_slots.get(pair_slot).hardware_buffer_id,
            self.render_slots.get(pair_slot).hardware_buffer_published,
            self.render_slots.get(pair_slot).prepared_render_flist.is_some(),
        );
        let snapshot = match snapshot {
            Some(snapshot) => snapshot,
            None => {
                let slot = self.render_slots.get_mut(pair_slot);
                slot.storage.as_deref_mut().ok_or(ENODEV)
                    .and_then(|storage| storage.snapshot(slot.submission_ordinal))
            }
        };
        dev_warn!(
            self.resources.dev().as_ref(),
            "G17P render handoff busy: reason={} selected-slot={} incoming(vm,context)=({},{}) retained(vm,root)={:x?} storage(slot,context,pool,context-pending)={:x?} matches(vm,context,pool,aliases)={:?} host-mask={:#x} host(ordinal,tracker,queued,hbid,published,prepared-flist)={:?} dram={:x?}\n",
            reason, pair_slot, vm.id(), descriptor_context_id,
            retained_bind, retained_storage, matches, host_mask, host, snapshot,
        );
    }

    /// Actual paired completion and FList release authorize root ownership
    /// changes. T8140's handoff page does not publish a live idle witness.
    fn require_retired_render_owner(&mut self, pair_slot: u8) -> Result {
        self.require_retired_render_owner_inner(pair_slot, None)
    }

    fn require_retired_render_owner_inner(
        &mut self, pair_slot: u8,
        handoff: Option<(&mmu::Vm, u16)>,
    ) -> Result {
        let state = self.render_slots.get(pair_slot);
        if state.preparing || state.flist_leases.is_some()
            || state.flist_retired_ordinal.is_some() { return Err(EBUSY); }
        // A graph the warm second pair built for an owner that closed before
        // kicking on it: nothing was ever published on this pair, so there is
        // no paired retirement to witness and nothing firmware could hold.
        // It is retired by construction (same predicate as the teardown gate).
        if self.render_slots.get(pair_slot).storage.is_some()
            && self.render_slots.get(pair_slot).submission_ordinal == 0
            && self.render_slots.get(pair_slot).tracker.is_none()
            && self.render_slots.get(pair_slot).queued_tracker.is_none()
            && self.render_slots.get(pair_slot).hardware_buffer_id.is_none()
            && !self.render_slots.get(pair_slot).hardware_buffer_published
            && self.render_slots.get(pair_slot).prepared_render_flist.is_none()
        {
            return Ok(());
        }
        // Handoff.current_slot is a T8140 stand-in, not a firmware idle
        // witness. Use the actual paired retirement and FList acknowledgements.
        if self.render_slots.get(pair_slot).tracker.is_some() || self.render_slots.get(pair_slot).queued_tracker.is_some()
            || self.render_slots.get(pair_slot).hardware_buffer_id.is_some()
            || self.render_slots.get(pair_slot).hardware_buffer_published || self.render_slots.get(pair_slot).prepared_render_flist.is_some()
            || self.render_slots.get(pair_slot).submission_ordinal == 0
        {
            if let Some((vm, context)) = handoff {
                self.report_render_handoff_busy(pair_slot, vm, context, "host-state", None);
            }
            return Err(EBUSY);
        }
        // This slot's USC FList is retained with this graph. Other slots may
        // retain their independent pools after this slot's TA/3D pair is
        // complete; those references do not keep this job's graph or VM alive.
        let slot = self.render_slots.get_mut(pair_slot);
        let snapshot = match slot.storage.as_deref_mut().ok_or(ENODEV)?
            .snapshot(slot.submission_ordinal)
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if error == EBUSY {
                    if let Some((vm, context)) = handoff {
                        self.report_render_handoff_busy(pair_slot, vm, context, "snapshot-read", Some(Err(error)));
                    }
                }
                return Err(error);
            }
        };
        // The shared list can contain other pairs/CL's live nodes. This
        // physical pair's exact retirement and idle cursors authorize rebind;
        // its old logical owner remains retained by the other live channels.
        let shared_tsq = slot.storage.as_deref().ok_or(ENODEV)?.has_shared_tsq();
        if (!shared_tsq && !snapshot.job_list_empty) || !snapshot.tiling.idle() || !snapshot.fragment.idle() {
            if let Some((vm, context)) = handoff {
                self.report_render_handoff_busy(pair_slot, vm, context, "firmware-state", Some(Ok(snapshot)));
            }
            return Err(EBUSY);
        }
        // Do not require the shared outer channels to be empty here.
        // This selected slot's paired completion already proved that its two
        // queue items were consumed and its job list is idle.  A later outer
        // item may legitimately belong to the other independently rooted
        // slot; using its live producer as an idle condition reintroduced the
        // full render/render lease at every cross-VM handoff.
        Ok(())
    }

    /// Retire one VM's job-private graph before another VM constructs its own
    /// graph on the already-installed physical QID pair.  Queue producer state
    /// and the monotonically increasing submission ordinal stay retained; no
    /// global context-0 mapping is repointed.
    fn park_render_sksm_backing_for_graph_rebuild(&mut self, pair_slot: u8) -> Result {
        if self.render_slots.get(pair_slot).sksm.is_none() {
            return Ok(());
        }
        let expected = {
            let queues = self.render_slots.get(pair_slot).sksm.as_ref().ok_or(EIO)?;
            if queues.backing.is_some() {
                return Err(EIO);
            }
            [
                (queues.tiling.entry_low, queues.tiling.entry_high),
                (queues.fragment.entry_low, queues.fragment.entry_high),
            ]
        };
        let actual = {
            let storage = self.render_slots.get(pair_slot).storage.as_deref().ok_or(EIO)?;
            [
                (storage.sksm_entry_low_va(true), storage.sksm_entry_high_va(true)),
                (storage.sksm_entry_low_va(false), storage.sksm_entry_high_va(false)),
            ]
        };
        if actual != expected {
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P render graph rebuild: refusing to detach mismatched SKSM backing actual={:#x?} expected={:#x?}\n",
                actual,
                expected,
            );
            return Err(EFAULT);
        }
        // The slot is idle, but its UMA descriptor contains firmware-updated
        // cursors and growth. Save them with this graph before another pool
        // overwrites the slot; reloading the constructor values loses pages.
        //
        // A logical context keeps its FList pool across graph rebuilds, and
        // with g17p_host_path SHARED_FLIST both of its pairs use one pool.
        // If another installed pair still names this pool, the pool is not
        // leaving with this graph and may be live there: leave its control
        // object to that pair instead of rewriting it from here.
        if crate::g17_config::RENDER_USC_FREELIST != 0 {
            let storage = self.render_slots.get(pair_slot).storage.as_deref().ok_or(EIO)?;
            let pool = (storage.render_flist_id(), storage.render_flist_pointer());
            let shared_elsewhere = self.render_slots.slots.iter().enumerate().any(|(other, slot)| {
                other != usize::from(pair_slot)
                    && slot.as_deref().and_then(|slot| slot.storage.as_deref()).is_some_and(|other|
                        (other.render_flist_id(), other.render_flist_pointer()) == pool)
            });
            if !shared_elsewhere {
                self.resources.save_user_render_uma(storage)?;
            }
        }
        let backing = self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(EIO)?.take_sksm_backing()?;
        self.render_slots.get_mut(pair_slot).sksm.as_mut().ok_or(EIO)?.backing = Some(backing);
        Ok(())
    }

    fn retarget_retained_render_context(
        &mut self, pair_slot: u8,
        vm: &mmu::Vm,
        execution_context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
    ) -> Result<bool> {
        let descriptor_context_id =
            u16::try_from(execution_context.context_id()).map_err(|_| EOVERFLOW)?;
        // Logical queues in one DRM VM receive independent descriptor
        // contexts, but the retained RT/TVB/PM graph and all of its fixed
        // client addresses belong to the VM. Once its previous pair is fully
        // retired, retarget only the queue-owned context fields and preserve
        // the graph. Rebuilding it in the same VM would collide with the
        // firmware-retained mappings that deliberately survive queue close.
        let same_vm_different_context = self.render_slots.get(pair_slot).storage.as_deref().is_some_and(|storage| {
            storage.descriptor_context_id() != descriptor_context_id
                && self.render_slots.get(pair_slot)
                    .bind
                    .as_ref()
                    .is_some_and(|binding| binding.matches(vm))
                && storage.first_unreachable(self.resources.uat(), vm).is_none()
        });
        if same_vm_different_context {
            self.require_retired_render_owner(pair_slot)?;
            let context_update_pending = self.render_slots.get(pair_slot)
                .storage
                .as_deref()
                .ok_or(ENODEV)?
                .context_update_pending();
            // A failed pre-publication retarget may be replaced by another
            // queue without accumulating roots: the currently selected context
            // was never acknowledged by firmware, while the first replaced
            // (firmware-visible) root remains owned here. Once a context update
            // completed, however, no retirement owner may still be pending.
            if self.render_slots.get(pair_slot).context_retirement.is_some() && !context_update_pending {
                return Err(EIO);
            }
            let retire_after_context_epoch =
                self.next_render_context_epoch(pair_slot)?;
            self.render_slots.get_mut(pair_slot).storage
                .as_deref_mut()
                .ok_or(ENODEV)?
                .retarget_command_context(self.resources.dev(), self.resources.uat(), vm, execution_context.clone())?;
            self.render_slots.get_mut(pair_slot).flist_index = self.render_slots.get(pair_slot)
                .storage.as_deref().ok_or(ENODEV)?.render_flist_id() as usize;
            let previous_bind = self.render_slots.get_mut(pair_slot)
                .bind
                .replace(G17PRenderRootOwner::Job {
                    context: execution_context,
                })
                .ok_or(EIO)?;
            let previous_context_aliases = self.render_slots.get_mut(pair_slot).context_aliases.take();
            if self.render_slots.get(pair_slot).context_retirement.is_none() {
                self.render_slots.get_mut(pair_slot).context_retirement = Some(G17PRenderContextRetirement {
                    bind: previous_bind,
                    _context_aliases: previous_context_aliases,
                    retire_after_context_epoch,
                });
            }
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P render owner handoff: retained VM {} graph on pair {} retargeted to descriptor context {}; prior root held through context epoch {}\n",
                vm.id(),
                pair_slot,
                descriptor_context_id,
                retire_after_context_epoch,
            );
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn handoff_retained_render_vm(
        &mut self, pair_slot: u8,
        vm: &mmu::Vm,
        execution_context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
    ) -> Result {
        let state = self.render_slots.get(pair_slot);
        if state.preparing || state.flist_retired_ordinal.is_some() { return Err(EBUSY); }
        if state.flist_leases.is_some()
            && !state.bind.as_ref().is_some_and(|bind| matches!(bind,
                G17PRenderRootOwner::Job { context } if Arc::ptr_eq(context, &execution_context))) {
            // Live commands permit same-owner pipelining, never pool handoff.
            return Err(EBUSY);
        }
        if !execution_context.matches(vm) || !execution_context.is_current() {
            return Err(EFAULT);
        }
        let descriptor_context_id =
            u16::try_from(execution_context.context_id()).map_err(|_| EOVERFLOW)?;
        if self.retarget_retained_render_context(pair_slot, vm, execution_context.clone())
            .map_err(|error| Self::render_stage_fail("handoff-retarget-current", error))?
        {
            return Ok(());
        }
        // An empty pair -- fresh, or its last owner's graph was freed when
        // that owner's scheduler-state release was acknowledged -- takes a
        // graph built for this queue at this VM's own reserved addresses.
        if self.render_slots.get(pair_slot).storage.is_none() {
            return Ok(());
        }
        if !self.retained_render_has_other_owner(pair_slot, vm, descriptor_context_id) {
            return Ok(());
        }
        // Another logical queue's graph is still installed here. The registry
        // re-issues a pair only after its owner closed, so that owner is
        // draining or waiting for its scheduler-state release. Its graph is
        // never reused by this queue: it moves, whole, into that owner's
        // teardown and is freed when the firmware acknowledges the release.
        self.require_retired_render_owner_inner(pair_slot, Some((vm, descriptor_context_id)))
            .map_err(|error| Self::render_stage_fail("handoff-require-retired-owner", error))?;
        let (old_context, old_root, old_vm_id) = match self.render_slots.get(pair_slot).bind.as_ref() {
            Some(G17PRenderRootOwner::Job { context }) => {
                (context.clone(), context.root(), context.vm_id())
            }
            Some(G17PRenderRootOwner::Bootstrap(_)) | None => {
                return Err(Self::render_stage_fail("handoff-foreign-owner-root", EIO));
            }
        };
        // Reserve the teardown's room before the graph leaves the pair, so
        // an allocation failure changes nothing.
        let teardown = self.reserve_logical_queue_teardown_graph(&old_context, true)
            .map_err(|error| Self::render_stage_fail("handoff-reserve-teardown", error))?;
        self.park_render_sksm_backing_for_graph_rebuild(pair_slot)
            .map_err(|error| Self::render_stage_fail("handoff-park-sksm-backing", error))?;
        let displaced = G17PRetiredRenderGraph {
            bind: self.render_slots.get_mut(pair_slot).bind.take().ok_or(EIO)?,
            context_aliases: self.render_slots.get_mut(pair_slot).context_aliases.take(),
            storage: self.render_slots.get_mut(pair_slot).storage.take().ok_or(ENODEV)?,
        };
        self.render_slots.get_mut(pair_slot).auxiliary_timestamp = 0;
        self.logical_queue_teardowns[teardown]
            .graphs
            .push_within_capacity(displaced)
            .map_err(|_| Self::render_stage_fail("handoff-park-displaced-graph", ENOMEM))?;
        if crate::util::g17p_verbose() {
            dev_info!(self.resources.dev().as_ref(),
                "G17P render VM handoff: pair {} graph of closed owner vm={} root={:#x} moved to its teardown; incoming vm={} builds a new graph at ordinal {}, QID producer state retained\n",
                pair_slot, old_vm_id, old_root, vm.id(),
                self.render_slots.get(pair_slot).submission_ordinal);
        }
        Ok(())
    }

    /// Close one logical queue's firmware lifetime. Called from the DRM
    /// queue's drop, after its scheduler entities drained, so no submission of
    /// this context can still be pending on the host side; hardware packets
    /// still in flight are retained by the runtime and gate the release.
    ///
    /// Graphs the queue still has installed stay on their pairs until the
    /// release is acknowledged: a new queue in the same VM can retarget them
    /// in place meanwhile, and a foreign successor moves them into this
    /// teardown. Nothing is freed before the firmware consumes the release.
    pub(crate) fn release_render_context_owner(
        &mut self,
        context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
    ) -> Result {
        let now = kernel::time::Instant::<kernel::time::Monotonic>::now();
        if let Some(index) = self.find_logical_queue_teardown(context) {
            let teardown = &mut self.logical_queue_teardowns[index];
            if teardown.phase == G17PLogicalQueueTeardownPhase::Deferred {
                teardown.phase = G17PLogicalQueueTeardownPhase::Draining;
                teardown.armed = now;
            }
            return Ok(());
        }
        // Firmware never saw this queue's scheduler state, so there is
        // nothing to release and no installed graph can name it: the page
        // goes with the queue's context.
        if !context.channel_control.is_published() {
            return Ok(());
        }
        self.push_logical_queue_teardown(context, G17PLogicalQueueTeardownPhase::Draining, now)
    }

    fn find_logical_queue_teardown(
        &self,
        context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
    ) -> Option<usize> {
        self.logical_queue_teardowns
            .iter()
            .position(|teardown| Arc::ptr_eq(&teardown.context, context))
    }

    fn push_logical_queue_teardown(
        &mut self,
        context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
        phase: G17PLogicalQueueTeardownPhase,
        now: kernel::time::Instant<kernel::time::Monotonic>,
    ) -> Result {
        let descriptor_context_id =
            u16::try_from(context.context_id()).map_err(|_| EOVERFLOW)?;
        let teardown = G17PLogicalQueueTeardown {
            graphs: KVec::new(),
            context: context.clone(),
            descriptor_context_id,
            phase,
            armed: now,
            warned: false,
        };
        // Capacity equals the number of contexts that can exist at once, so
        // this cannot fail; if it ever did, the page would be freed without
        // its release, which the log must name.
        self.logical_queue_teardowns
            .push_within_capacity(teardown)
            .map_err(|_| {
                dev_err!(
                    self.resources.dev().as_ref(),
                    "G17P teardown: no room for context {}; its scheduler state cannot be released\n",
                    descriptor_context_id,
                );
                ENOSPC
            })
    }

    /// Find or create the teardown that will own a graph its context is
    /// giving up, and reserve room for exactly one more graph. A still-open
    /// owner (`owner_closed == false`) gets a `Deferred` entry that starts
    /// draining when it closes; a closed owner whose close did not arm one
    /// (its page was never marked published, which an installed graph rules
    /// out) gets a `Draining` entry so its graph is still freed.
    fn reserve_logical_queue_teardown_graph(
        &mut self,
        context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
        owner_closed: bool,
    ) -> Result<usize> {
        let index = match self.find_logical_queue_teardown(context) {
            Some(index) => index,
            None => {
                let phase = if owner_closed {
                    dev_warn!(
                        self.resources.dev().as_ref(),
                        "G17P teardown: closed owner context {} had no teardown armed; arming one for its displaced graph\n",
                        context.context_id(),
                    );
                    G17PLogicalQueueTeardownPhase::Draining
                } else {
                    G17PLogicalQueueTeardownPhase::Deferred
                };
                self.push_logical_queue_teardown(
                    context,
                    phase,
                    kernel::time::Instant::<kernel::time::Monotonic>::now(),
                )?;
                self.logical_queue_teardowns.len() - 1
            }
        };
        self.logical_queue_teardowns[index].graphs.reserve(1, GFP_KERNEL)?;
        Ok(index)
    }

    /// Whether one pair is quiescent for `descriptor_context_id`'s release:
    /// either it does not carry that context's graph, or nothing of it is
    /// tracked, queued or leased, the pair's FList release has been consumed,
    /// and firmware's job list and both queues are idle in DRAM. A snapshot
    /// refused with EBUSY counts as not idle.
    fn render_slot_idle_for_context(
        state: &mut G17PRenderSlotState,
        descriptor_context_id: u16,
        _owners: &[Option<G17PBootstrapUmaLifecycle>],
    ) -> Result<bool> {
        if state.preparing {
            return Ok(!state.bind.as_ref().is_some_and(|bind|
                bind.root_slot().ok() == u8::try_from(descriptor_context_id).ok()));
        }
        let owns_context = state
            .storage
            .as_deref()
            .is_some_and(|storage| storage.descriptor_context_id() == descriptor_context_id);
        if !owns_context {
            return Ok(true);
        }
        if Self::render_slot_graph_never_kicked(state) {
            // A pair warmed for this context (warm second pair) whose owner
            // closed before ever kicking on it: no tag-15 install, no HWPB
            // publication, no tracker, no completion. Firmware holds nothing
            // of this graph, so it is idle by construction and there is no
            // paired retirement to snapshot.
            return Ok(true);
        }
        if state.tracker.is_some()
            || state.queued_tracker.is_some()
            || state.hardware_buffer_id.is_some()
            || state.hardware_buffer_published
            || state.prepared_render_flist.is_some()
            || state.submission_ordinal == 0
            || state.flist_leases.is_some() || state.flist_retired_ordinal.is_some()
        {
            return Ok(false);
        }
        let Some(storage) = state.storage.as_deref_mut() else { return Ok(true) };
        match storage.snapshot(state.submission_ordinal) {
            Ok(snapshot) => Ok((storage.has_shared_tsq() || snapshot.job_list_empty)
                && snapshot.tiling.idle()
                && snapshot.fragment.idle()),
            Err(error) if error == EBUSY => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// A graph constructed on a pair that never published a kick: the pair's
    /// submission ordinal is still 0 (paired completion is what advances it)
    /// and nothing of a render is tracked, queued, leased or published. The
    /// warm second pair builds exactly such graphs before the owner's first
    /// kick, and an owner that only ever rendered on its other pair leaves
    /// one behind at close.
    fn render_slot_graph_never_kicked(state: &G17PRenderSlotState) -> bool {
        !state.preparing && state.storage.is_some()
            && state.submission_ordinal == 0
            && state.tracker.is_none()
            && state.queued_tracker.is_none()
            && state.hardware_buffer_id.is_none()
            && !state.hardware_buffer_published
            && state.prepared_render_flist.is_none()
            && state.flist_leases.is_none() && state.flist_retired_ordinal.is_none()
    }

    fn render_pairs_idle_for_context(&mut self, descriptor_context_id: u16) -> Result<bool> {
        let pair_slot = 0;
        let active_idle = Self::render_slot_idle_for_context(
            self.render_slots.get_mut(pair_slot), descriptor_context_id, &self.render_flist_owners);
        if !active_idle? {
            return Ok(false);
        }
        for state in self.render_slots.parked_mut(pair_slot) {
            if !Self::render_slot_idle_for_context(state, descriptor_context_id, &self.render_flist_owners)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Pool release is independent of physical-pair ownership. Also inspect
    /// the strong context's pools when a same-VM successor reused its graph.
    fn displaced_graphs_flists_released(&self, index: usize) -> bool {
        let Some(teardown) = self.logical_queue_teardowns.get(index) else { return false };
        let released = |id: u32, pointer: u64| {
            match self.render_flist_owners.get(id as usize).and_then(Option::as_ref) {
                None => true, // This independently leased pool never published.
                Some(owner) if owner.shared_control != pointer => {
                    // Only a never-published new pool can still see the old
                    // occupant's row; ID reuse requires its complete release.
                    owner.state == G17PBootstrapUmaRequestState::Completed
                        && !owner.hardware_buffer_owned && !owner.has_release_witness()
                }
                Some(owner) => owner.state == G17PBootstrapUmaRequestState::Completed
                    && !owner.hardware_buffer_owned && !owner.has_release_witness(),
            }
        };
        teardown.context.render_flists_released(released)
            && teardown.graphs.iter().all(|graph|
                released(graph.storage.render_flist_id(), graph.storage.render_flist_pointer()))
    }

    /// The first `Draining` teardown at or after `from` whose render pairs are
    /// quiescent and whose displaced graphs' FLists are released. The runtime
    /// adds the compute condition before publishing.
    pub(crate) fn logical_queue_teardown_render_ready(
        &mut self,
        from: usize,
    ) -> Result<Option<usize>> {
        for index in from..self.logical_queue_teardowns.len() {
            if self.logical_queue_teardowns[index].phase != G17PLogicalQueueTeardownPhase::Draining {
                continue;
            }
            let descriptor_context_id = self.logical_queue_teardowns[index].descriptor_context_id;
            if self.render_pairs_idle_for_context(descriptor_context_id)?
                && self.displaced_graphs_flists_released(index)
            {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    pub(crate) fn logical_queue_teardown_context(
        &self,
        index: usize,
    ) -> Option<Arc<crate::g17_resources::G17PLogicalQueueContext>> {
        self.logical_queue_teardowns.get(index).map(|teardown| teardown.context.clone())
    }

    /// Publish the scheduler-state release for teardown `index`: opcode
    /// `0x14`, the four identity bytes read from the live page, and the page's
    /// firmware VA, exactly as `AGXCommandQueue::freeReclaimableResources`
    /// submits it. The caller announces the record (EP 0x21, mode 1) like the
    /// FList release. EAGAIN means the ring is full or a mandatory reply is
    /// pending; the teardown stays `Draining` and is retried.
    pub(crate) fn publish_logical_queue_release(
        &mut self,
        index: usize,
    ) -> Result<g17_resources::G17PDeviceControlPublication> {
        if self.pending_primary_control.is_some() {
            return Err(EAGAIN);
        }
        let teardown = self.logical_queue_teardowns.get(index).ok_or(EINVAL)?;
        if teardown.phase != G17PLogicalQueueTeardownPhase::Draining {
            return Err(EINVAL);
        }
        let control = &teardown.context.channel_control;
        let identity = control.release_identity()?;
        let resource_va = control.gpu_va();
        let descriptor_context_id = teardown.descriptor_context_id;
        let mut record = [0u8; g17_initdata::CONTROL_RECORD_SIZE];
        g17_initdata::encode_scheduler_state_release_record(resource_va, identity, &mut record)
            .map_err(|_| EINVAL)?;
        let publication = self.resources.publish_primary_device_control_raw(&record)?;
        self.logical_queue_teardowns[index].phase =
            G17PLogicalQueueTeardownPhase::Released { producer_after: publication.producer_after };
        if crate::util::g17p_verbose() {
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P teardown: context {} scheduler-state release published va={:#x} identity={:02x?} slot={} producer {}->{}\n",
                descriptor_context_id, resource_va, identity, publication.slot,
                publication.producer_before, publication.producer_after,
            );
        }
        Ok(publication)
    }

    /// Nanoseconds since teardown `index` was armed (the client's close).
    pub(crate) fn logical_queue_teardown_age_ns(&self, index: usize) -> i64 {
        self.logical_queue_teardowns.get(index)
            .map_or(0, |teardown| teardown.armed.elapsed().as_nanos())
    }

    /// `g17p_exit_kill`: which render QIDs belong to `context` and which to
    /// anyone else. `own` and `own_slots` are the pairs whose graph is bound
    /// to exactly this context and not being prepared; `foreign` is every
    /// other bound or preparing pair (the bootstrap pair included). A pair
    /// with no bound graph is in neither set.
    pub(crate) fn exit_kill_render_scope(
        &self,
        context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
    ) -> (u128, u128, u64) {
        let (mut own, mut foreign, mut own_slots) = (0u128, 0u128, 0u64);
        for slot in 0..g17_resources::G17PRenderQueuePair::SLOT_COUNT {
            let Ok(state) = self.render_slots.allocated(slot) else { continue };
            let Ok(pair) = self.retained_render_queue_pair(slot) else { continue };
            let qids = u8::try_from(pair.tiling).map_or(0, crate::g17_exit_kill::qid_bit)
                | u8::try_from(pair.fragment).map_or(0, crate::g17_exit_kill::qid_bit);
            let mine = !state.preparing && matches!(&state.bind,
                Some(G17PRenderRootOwner::Job { context: owner }) if Arc::ptr_eq(owner, context));
            if mine {
                own |= qids;
                own_slots |= 1u64 << slot;
            } else if state.preparing || state.bind.is_some() {
                foreign |= qids;
            }
        }
        (own, foreign, own_slots)
    }

    /// The two QIDs of render pair `slot` as a mask (0 if it has none).
    pub(crate) fn render_slot_qids(&self, slot: u8) -> u128 {
        self.retained_render_queue_pair(slot).map_or(0, |pair|
            u8::try_from(pair.tiling).map_or(0, crate::g17_exit_kill::qid_bit)
                | u8::try_from(pair.fragment).map_or(0, crate::g17_exit_kill::qid_bit))
    }

    /// Publish the process-exit kill of `context`'s scheduler-state page,
    /// exactly as `AGXCommandQueue::processExitTimeoutHandler` submits it
    /// (opcode 0x29, page VA, cookie; submit flags 1: no completion wait).
    /// The caller announces it (EP 0x21, mode 1). EAGAIN: ring full or a
    /// mandatory reply is pending; retried on the next teardown turn.
    pub(crate) fn publish_exit_kill(
        &mut self,
        context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
        cookie: u64,
    ) -> Result<g17_resources::G17PDeviceControlPublication> {
        if self.pending_primary_control.is_some() {
            return Err(EAGAIN);
        }
        let mut record = [0u8; g17_initdata::CONTROL_RECORD_SIZE];
        g17_initdata::encode_context_kill_record(context.channel_control.gpu_va(), cookie, &mut record)
            .map_err(|_| EINVAL)?;
        self.resources.publish_primary_device_control_raw(&record)
    }

    /// Detach every graph on any pair whose current root owner is exactly
    /// `context`. A graph a same-VM successor retargeted in place belongs to
    /// that successor now and is left alone. EBUSY if such a pair is not idle.
    fn detach_render_slot_graph(
        state: &mut G17PRenderSlotState,
        context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
        descriptor_context_id: u16,
        ids: &mut G17PHardwareBufferIdAllocator,
        out: &mut KVec<G17PRetiredRenderGraph>,
        owners: &[Option<G17PBootstrapUmaLifecycle>],
    ) -> Result {
        if state.preparing {
            return if state.bind.as_ref().is_some_and(|bind|
                matches!(bind, G17PRenderRootOwner::Job { context: owner } if Arc::ptr_eq(owner, context)))
                { Err(EBUSY) } else { Ok(()) };
        }
        let owned = state.storage.as_deref().is_some_and(|storage| {
            storage.descriptor_context_id() == descriptor_context_id
        }) && state.bind.as_ref().is_some_and(|bind| {
            matches!(bind, G17PRenderRootOwner::Job { context: owner } if Arc::ptr_eq(owner, context))
        });
        if !owned {
            return Ok(());
        }
        if !Self::render_slot_idle_for_context(state, descriptor_context_id, owners)? {
            return Err(EBUSY);
        }
        out.reserve(1, GFP_KERNEL)?;
        // The pair keeps its hardware queue state: the SKSM entry rings go
        // back to the slot exactly as park_render_sksm_backing_for_graph_rebuild
        // does for the selected slot.
        if let Some(queues) = state.sksm.as_mut() {
            if queues.backing.is_some() {
                return Err(EIO);
            }
            let storage = state.storage.as_deref_mut().ok_or(EIO)?;
            let expected = [
                (queues.tiling.entry_low, queues.tiling.entry_high),
                (queues.fragment.entry_low, queues.fragment.entry_high),
            ];
            let actual = [
                (storage.sksm_entry_low_va(true), storage.sksm_entry_high_va(true)),
                (storage.sksm_entry_low_va(false), storage.sksm_entry_high_va(false)),
            ];
            if actual != expected {
                return Err(EFAULT);
            }
            queues.backing = Some(storage.take_sksm_backing()?);
        }
        let storage = state.storage.take().ok_or(EIO)?;
        if let Some(id) = storage.retained_hardware_buffer_id() {
            ids.unreserve(id);
        }
        let graph = G17PRetiredRenderGraph {
            bind: state.bind.take().ok_or(EIO)?,
            context_aliases: state.context_aliases.take(),
            storage,
        };
        state.auxiliary_timestamp = 0;
        // The pair's installed QIDs still name this scheduler page until the
        // next owner's context update; keep it mapped with the pair.
        if state.channel_control.is_none() {
            state.channel_control = Some(context.channel_control.clone());
        }
        out.push_within_capacity(graph).map_err(|_| ENOMEM)?;
        Ok(())
    }

    fn detach_render_graphs_of_context(
        &mut self,
        context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
        descriptor_context_id: u16,
    ) -> Result<KVec<G17PRetiredRenderGraph>> {
        let pair_slot = 0;
        let mut graphs = KVec::new();
        let result = Self::detach_render_slot_graph(
            self.render_slots.get_mut(pair_slot), context, descriptor_context_id,
            &mut self.render_hardware_buffer_ids, &mut graphs, &self.render_flist_owners,
        );
        result?;
        for state in self.render_slots.parked_mut(pair_slot) {
            Self::detach_render_slot_graph(
                state, context, descriptor_context_id,
                &mut self.render_hardware_buffer_ids, &mut graphs, &self.render_flist_owners,
            )?;
        }
        Ok(graphs)
    }

    /// Complete every teardown whose release the firmware has consumed:
    /// detach the graphs it still has installed, return the queue's
    /// HardwareBufferIDs, drop its VM render-pool aliases, and hand the whole
    /// entry back so the caller can drop it -- root, aliases, then every
    /// kernel backing -- with the runtime mutex released. An entry whose pair
    /// is unexpectedly busy stays and is retried.
    pub(crate) fn settle_logical_queue_teardowns(
        &mut self,
    ) -> Result<KVec<G17PLogicalQueueTeardown>> {
        let mut settled = KVec::new();
        if !self.logical_queue_teardowns.iter().any(|teardown| {
            matches!(teardown.phase, G17PLogicalQueueTeardownPhase::Released { .. })
        }) {
            return Ok(settled);
        }
        let counters = self.resources.control_counters()?;
        let consumer = counters.primary[g17_initdata::CHANNEL_STATE_CONSUMER];
        let producer = counters.primary[g17_initdata::CHANNEL_STATE_PRODUCER];
        let mut index = 0;
        while index < self.logical_queue_teardowns.len() {
            let consumed = match self.logical_queue_teardowns[index].phase {
                G17PLogicalQueueTeardownPhase::Released { producer_after } => {
                    g17p_control_ring_target_consumed(consumer, producer, producer_after)
                        .map_err(|_| EIO)?
                }
                _ => false,
            };
            if !consumed {
                index += 1;
                continue;
            }
            let context = self.logical_queue_teardowns[index].context.clone();
            let descriptor_context_id = self.logical_queue_teardowns[index].descriptor_context_id;
            settled.reserve(1, GFP_KERNEL)?;
            if !self.displaced_graphs_flists_released(index) { index += 1; continue; }
            let detached = match self.detach_render_graphs_of_context(&context, descriptor_context_id) {
                Ok(detached) => detached,
                Err(error) if error == EBUSY => {
                    index += 1;
                    continue;
                }
                Err(error) => return Err(error),
            };
            self.logical_queue_teardowns[index].graphs.reserve(detached.len(), GFP_KERNEL)?;
            let mut teardown = self.logical_queue_teardowns.remove(index).map_err(|_| EIO)?;
            for graph in detached {
                teardown.graphs.push_within_capacity(graph).map_err(|_| ENOMEM)?;
            }
            for graph in teardown.graphs.iter() {
                if let Some(id) = graph.storage.retained_hardware_buffer_id() {
                    self.render_hardware_buffer_ids.unreserve(id);
                }
                // VM alias removal and its GPUVM/TLB work belong to the
                // returned teardown, after the runtime mutex is released.
            }
            // One line per closed queue, the counterpart of the queue's
            // creation: this is the moment its memory actually goes back.
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P teardown: context {} release consumed (consumer={} producer={}); freeing {} graph(s)\n",
                descriptor_context_id, consumer, producer, teardown.graphs.len(),
            );
            settled.push_within_capacity(teardown).map_err(|_| ENOMEM)?;
        }
        Ok(settled)
    }

    pub(crate) fn logical_queue_teardowns_outstanding(&self) -> usize {
        self.logical_queue_teardowns.len()
    }

    /// True while some closed queue's teardown is young enough to deserve
    /// millisecond re-polling by the event worker.
    pub(crate) fn logical_queue_teardown_polling(&self) -> bool {
        self.logical_queue_teardowns.iter().any(|teardown| {
            teardown.phase != G17PLogicalQueueTeardownPhase::Deferred
                && teardown.armed.elapsed().as_nanos() < G17P_LOGICAL_QUEUE_TEARDOWN_POLL_NS
        })
    }

    /// Report, once each, teardowns that have waited longer than the warning
    /// budget. Nothing is freed on a timeout: the firmware witness is the
    /// only release, and processor stop drains whatever never arrives.
    pub(crate) fn warn_stalled_logical_queue_teardowns(&mut self) {
        for teardown in self.logical_queue_teardowns.iter_mut() {
            if teardown.warned
                || teardown.phase == G17PLogicalQueueTeardownPhase::Deferred
                || teardown.armed.elapsed().as_nanos() < G17P_LOGICAL_QUEUE_RELEASE_WARN_NS
            {
                continue;
            }
            teardown.warned = true;
            dev_warn!(
                self.resources.dev().as_ref(),
                "G17P teardown: context {} still {:?} after {} ms; retaining {} displaced graph(s) and its installed pairs until the firmware acknowledges\n",
                teardown.descriptor_context_id, teardown.phase,
                teardown.armed.elapsed().as_nanos() / 1_000_000, teardown.graphs.len(),
            );
        }
    }

    /// Commit one per-pair context-supersession epoch. This runs only after the
    /// normal executed paired-completion path has also observed EventControl
    /// unlink; an abandoned/unexecuted group deliberately does not advance it.
    fn complete_render_context_update_retirement(&mut self, pair_slot: u8, context_update: bool) -> Result {
        if !context_update {
            return Ok(());
        }
        let slot = usize::from(pair_slot);
        let next = self.next_render_context_epoch(pair_slot)?;
        if self.render_slots.get(pair_slot)
            .context_retirement
            .as_ref()
            .is_some_and(|retirement| retirement.retire_after_context_epoch != next)
        {
            return Err(EIO);
        }
        self.render_slots.get_mut(pair_slot).storage
            .as_deref_mut()
            .ok_or(ENODEV)?
            .complete_context_update();
        self.render_context_epochs[slot] = next;
        self.render_slots.get_mut(pair_slot).context_retirement = None;
        Ok(())
    }

    /// Rebuild a retained graph whose ring producer no longer matches the
    /// ordinal, instead of refusing every later submission on it.
    ///
    /// A submission stages its items into the retained ring before the outer
    /// publication and the tag-14/tag-15 that make them visible. A failure in
    /// between advances `write` without advancing the ordinal, and from then on
    /// `restage_command`'s admission guard refuses this client forever -- one
    /// lost frame becomes a permanently dead context, which for the compositor
    /// means a dead session.
    ///
    /// The repair is the cross-VM handoff's, because the situation is the same:
    /// retire the job-private graph and let the next submission build a fresh
    /// one, whose `rebase_empty_queue_cursors` puts its cursors where this
    /// ordinal expects them while the physical QID producer stays monotonic.
    /// It is gated on `require_retired_render_owner`, so it can never run while
    /// the firmware still owns the previous group -- in that case the guard
    /// still refuses, and now says so in full.
    fn recycle_desynced_render_storage(&mut self, pair_slot: u8) -> Result {
        let ordinal = self.render_slots.get(pair_slot).submission_ordinal;
        let desynced = match self.render_slots.get_mut(pair_slot).storage.as_deref_mut() {
            Some(storage) => storage.restage_admission_desynced(ordinal)?,
            None => return Ok(()),
        };
        if !desynced {
            return Ok(());
        }
        // Exactly the evidence a cross-VM handoff requires before it may drop a
        // job-private graph: no tracker, no HardwareBufferID, no staged FList,
        // empty job list and two idle queues.
        if self.require_retired_render_owner(pair_slot).is_err() {
            return Ok(());
        }
        let cursors = self.render_slots.get_mut(pair_slot)
            .storage
            .as_deref_mut()
            .ok_or(ENODEV)?
            .restage_admission_cursors(ordinal)?;
        let root = self.render_slots.get(pair_slot).bind.as_ref().map_or(0, |binding| binding.root());
        let vm_id = self.render_slots.get(pair_slot).bind.as_ref().map_or(0, |binding| binding.vm_id());
        // The abandoned graph is firmware-visible under this queue's live
        // scheduler state, so it waits in the queue's (deferred) teardown and
        // is freed with the queue's own release acknowledgement.
        let context = match self.render_slots.get(pair_slot).bind.as_ref() {
            Some(G17PRenderRootOwner::Job { context }) => context.clone(),
            Some(G17PRenderRootOwner::Bootstrap(_)) | None => return Err(EIO),
        };
        let teardown = self.reserve_logical_queue_teardown_graph(&context, false)?;
        self.park_render_sksm_backing_for_graph_rebuild(pair_slot)?;
        let abandoned = G17PRetiredRenderGraph {
            bind: self.render_slots.get_mut(pair_slot).bind.take().ok_or(EIO)?,
            context_aliases: self.render_slots.get_mut(pair_slot).context_aliases.take(),
            storage: self.render_slots.get_mut(pair_slot).storage.take().ok_or(ENODEV)?,
        };
        self.logical_queue_teardowns[teardown]
            .graphs
            .push_within_capacity(abandoned)
            .map_err(|_| ENOMEM)?;
        dev_warn!(
            self.resources.dev().as_ref(),
            "G17P render: retained graph desynced at ordinal {} (expected TA/3D bases {:?}, tiling write {}, fragment write {}) while both queues are idle; rebuilding it for vm {} root {:#x} instead of refusing the client\n",
            ordinal,
            cursors.0,
            cursors.1,
            cursors.2,
            vm_id,
            root,
        );
        Ok(())
    }

    /// End the active slot's retained storage claim on its TA HardwareBufferID.
    /// Call only where that storage is about to be dropped.
    fn unreserve_render_storage_hardware_buffer_id(&mut self, pair_slot: u8) {
        if let Some(id) = self.render_slots.get(pair_slot)
            .storage
            .as_deref()
            .and_then(g17_resources::G17PUserRenderStorage::retained_hardware_buffer_id)
        {
            self.render_hardware_buffer_ids.unreserve(id);
        }
    }

    fn prepare_render_pm_manager_generation(&mut self) -> Result<Option<u64>> {
        if g17_resources::g17p_render_tvb_mode() < 2 {
            return Ok(None);
        }
        let (reserved, next) = g17_resources::g17p_pm_manager_generation_plan(
            self.next_render_pm_manager_generation,
        )?;
        // Reserve before graph construction. Independent cold builders must
        // never encode the same identity while either is still preparing.
        // This preserves the existing global Scene-cookie allocation policy.
        // Native's cookie is per PM replacement, not a unique host build ID;
        // off-runtime BuildLease identity must be separated from it before
        // relying on reservation gaps or out-of-order firmware installation.
        self.next_render_pm_manager_generation = next;
        Ok(Some(reserved))
    }

    /// Check the graph against its own reservation, independent of the order
    /// in which other pairs reserve or finish constructing their graphs.
    fn commit_render_pm_manager_generation(
        &self, pair_slot: u8,
        reserved: Option<u64>,
    ) -> Result {
        let actual = self.render_slots.get(pair_slot)
            .storage
            .as_deref()
            .ok_or(ENODEV)?
            .parameter_manager_generation();
        match (reserved, actual) {
            (None, None) => Ok(()),
            (Some(reserved), Some(actual)) if reserved == actual => Ok(()),
            _ => Err(EIO),
        }
    }

    /// Validate the incoming retained storage's indexed PBDesc identity.
    /// Owner switches must preserve the allocation state firmware left there.
    fn retarget_render_parameter_buffer(&mut self, pair_slot: u8) -> Result {
        // Without slot affinity every client alternation rebuilds the graph,
        // and `new_user_render_storage` stamps the loader on the way through.
        if crate::g17_config::RENDER_SLOT_AFFINITY == 0 {
            return Ok(());
        }
        let owner = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?.render_pool_id();
        if owner == self.render_parameter_buffer_owner {
            return Ok(());
        }
        let storage = self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?;
        self.resources.stage_render_parameter_buffer_view(storage)?;
        let previous = self.render_parameter_buffer_owner;
        self.render_parameter_buffer_owner = owner;
        // Rate-limited: one alternating pair of clients would otherwise print
        // this twice a frame, and a dockchannel console write costs about a
        // millisecond -- enough to distort the timing this records.
        if G17P_PARAMETER_BUFFER_RETARGETS.fetch_add(1, Ordering::Relaxed) < 8 {
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P render PM: retained parameter-buffer owner validated from {:#x} to {:#x}\n",
                previous,
                owner,
            );
        }
        Ok(())
    }

    pub(crate) fn retained_render_needs_recycle(
        &self, pair_slot: u8,
        vm: &mmu::Vm,
        descriptor_context_id: u16,
    ) -> bool {
        let retained_graph = self.render_slots.get(pair_slot).storage.is_some() || self.render_slots.get(pair_slot).context_aliases.is_some();
        let same_owner = !self.retained_render_has_other_owner(pair_slot, vm, descriptor_context_id);
        let recycle = g17_lifecycle::retained_render_requires_recycle(
            retained_graph,
            same_owner,
            self.render_slots.get(pair_slot).submission_ordinal,
        );
        if recycle {
            dev_warn!(self.resources.dev().as_ref(),
                "G17P render: session recycle requested ordinal={} retained={} same-owner={} descriptor-context={}\n",
                self.render_slots.get(pair_slot).submission_ordinal, retained_graph, same_owner, descriptor_context_id);
        }
        recycle
    }

    /// Prepare the two render KSM kick queues without touching the hardware
    /// bridge: QID 0 (TA, data master 0) and QID 1 (3D, data master 1).
    ///
    /// PROVEN by the valid cold macOS doorbell-1 capture: the first render uses
    /// QID 0 for TA/DM0 and QID 1 for 3D/DM1, both on priority zero. The QIDs,
    /// data masters, outer slots, tag-14 records, and SKSM entries below retain
    /// that exact initial profile.
    /// Name the render publish step that failed. `submit_translated_render`
    /// reports only a propagated errno, so before this every EINVAL raised
    /// below `begin_translated_render` -- render storage setup, the two-QID
    /// SKSM staging, the outer publish -- was indistinguishable at the ioctl.
    fn render_stage_fail(stage: &str, error: kernel::error::Error) -> kernel::error::Error {
        pr_info!(
            "G17P render: STAGE FAIL stage={} errno={}\n",
            stage,
            error.to_errno()
        );
        error
    }

    fn new_render_sksm_queues(
        &self,
        pair: g17_resources::G17PRenderQueuePair,
        hardware_buffer_id: u8,
        entry_vas: [(u64, u64); 2],
    ) -> Result<G17PRenderSksmQueues> {
        const RENDER_COMPLETION_SELECTOR: u8 =
            g17_submission::G17P_NATIVE_RENDER_DOORBELL_PRIORITY;
        let geometry = self.sksm_queue_geometry();
        let scratch = self.sksm_scratch();
        let build = |queue_id: u8, data_master: u32, tiling: bool, entry_gpu_va: u64| -> Result<G17PRenderSksmQueue> {
            let routing = g17_submission::G17PComputeQueueRouting {
                queue_id,
                completion_selector: RENDER_COMPLETION_SELECTOR as u32,
                entry_gpu_va,
                geometry,
            };
            let config = g17_submission::prepare_g17p_render_sksm_queue_config(
                routing,
                data_master,
                scratch,
            )
            .map_err(|_| Self::render_stage_fail("sksm-queue-config", EINVAL))?;
            Ok(G17PRenderSksmQueue {
                queue_id,
                tiling,
                entry_low: entry_gpu_va,
                entry_high: if tiling { entry_vas[0].1 } else { entry_vas[1].1 },
                config,
                producer: geometry
                    .initial_producer_state(
                        queue_id,
                        RENDER_COMPLETION_SELECTOR,
                        g17_submission::G17PClQosConfig {
                            // `encodeKickEntry` obtains this byte from the
                            // queue's AGXQOSQueue HardwareBufferID. It must
                            // match both ConfigUpdate +0x56 and the qid*8
                            // willSubmit record. Native gives each retained
                            // TA/3D graph one ID (captured qid9/10 use 4 while
                            // qid15/16 use 5), which is the same retained ID
                            // that owns the graph's PBDesc slot.
                            queue_byte_32: match crate::g17_config::RENDER_KICK_QOS
                            {
                                1 if tiling => 1,
                                2 => 1,
                                _ => hardware_buffer_id,
                            },
                            // G17P's stage wrappers select row 1 of the native
                            // 9x3 QoS table: TA=0x0c, 3D=0x18, CL=0x08. The
                            // former render path copied CL's class into both
                            // graphics queues, leaving 3D on the wrong launch
                            // class even after TA completed successfully.
                            class_40: match crate::g17_config::RENDER_KICK_QOS
                            {
                                1 if tiling => 0x08,
                                2 => 0x08,
                                _ if tiling => 0x0c,
                                _ => 0x18,
                            },
                            // Direct entry-ring captures show 0xffff here for
                            // both ordinary Metal TA and 3D kicks. Earlier
                            // values 1/0 came from the wrong adjacent global,
                            // and made Linux's dependency-control qword differ
                            // before either RCE program could execute.
                            word_48: 0xffff,
                        },
                    )
                    .map_err(|_| Self::render_stage_fail("sksm-producer-state", EINVAL))?,
                registered: false,
                hardware_enabled: false,
            })
        };
        let tiling_queue = build(
            pair.tiling as u8,
            g17_submission::G17P_TA_DATA_MASTER,
            true,
            entry_vas[0].0,
        )?;
        let fragment_queue = build(
            pair.fragment as u8,
            match crate::g17_config::RENDER_3D_DM {
                0xff => g17_submission::G17P_3D_DATA_MASTER,
                other => other,
            },
            false,
            entry_vas[1].0,
        )?;
        Ok(G17PRenderSksmQueues {
            scratch,
            uat_owner: self.uat().ttb_base(),
            tiling: tiling_queue,
            fragment: fragment_queue,
            backing: None,
        })
    }

    /// Publish one render group's SKSM half and return the KSM producer stamp
    /// its tag-14 AddKicks must announce.
    ///
    /// Ordering mirrors `submit_translated_first_bind_channel` exactly: the
    /// caller has already staged the group's command record and tag-15
    /// ConfigUpdate pointer. This writes the kick entry body before the caller
    /// publishes tag-14; tag 15 itself asks firmware to install a fresh QID.
    fn log_render_context_route(&mut self, pair_slot: u8, label: &'static str) -> Result {
        let (pair, descriptor, tiling, fragment, root_slot, ctxdesc) = {
            let storage = self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?;
            let pair = storage.queue_pair();
            let descriptor = storage.descriptor_context_ids()?;
            let tiling = storage.tiling_config_update_header()?;
            let fragment = storage.fragment_config_update_header()?;
            let root_slot = storage.render_root_slot();
            let ctxdesc = storage.render_channel_control_prefix()?;
            (pair, descriptor, tiling, fragment, root_slot, ctxdesc)
        };
        // Slot zero is already qualified and would add four lines to every
        // boot. The unresolved route is the independently retained qid2/3
        // pair, so keep this diagnostic bounded to that first installation.
        if pair.slot() == 0 {
            return Ok(());
        }
        let qos_context = u8::try_from(fragment.field_56).map_err(|_| EINVAL)?;
        let descriptor_context = u8::try_from(fragment.context_id).map_err(|_| EINVAL)?;
        let contexts = [1, qos_context, descriptor_context, root_slot];
        let roots = self
            .resources
            .uat()
            .t8140_selected_context_roots(contexts)?;
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P render route[{}]: qids={}/{} descriptor+0x0c={}/{} tag15+0x32={}/{} tag15+0x56={}/{} contexts(opening,qos,descriptor,bind)={:?} roots=[{:#x}/{:#x},{:#x}/{:#x},{:#x}/{:#x},{:#x}/{:#x}] fw-ctxdesc(slot={},ctx={},state={},bound={})\n",
            label,
            pair.tiling,
            pair.fragment,
            descriptor[0],
            descriptor[1],
            tiling.context_id,
            fragment.context_id,
            tiling.field_56,
            fragment.field_56,
            contexts,
            roots[0].0,
            roots[0].1,
            roots[1].0,
            roots[1].1,
            roots[2].0,
            roots[2].1,
            roots[3].0,
            roots[3].1,
            ctxdesc[0],
            ctxdesc[1],
            ctxdesc[0x26],
            ctxdesc[0x33],
        );
        Ok(())
    }

    /// Check that tag 15 describes the only legal firmware install transition.
    /// A live queue may never be installed twice, while a fresh production
    /// queue must carry the install bit. The explicit no-install module
    /// parameter remains a diagnostic and deliberately leaves the queue fresh.
    fn validate_render_firmware_install(&self, pair_slot: u8, tiling: bool, install: u16) -> Result<bool> {
        let queues = self.render_slots.get(pair_slot).sksm.as_ref().ok_or(ENODEV)?;
        let queue = if tiling {
            &queues.tiling
        } else {
            &queues.fragment
        };
        match (queue.registered, install != 0) {
            (false, true) => Ok(true),
            (true, false) => Ok(false),
            (true, true) => Err(EBUSY),
            (false, false) if crate::g17_config::RENDER_NO_INSTALL != 0 => {
                Ok(false)
            }
            (false, false) => Err(EINVAL),
        }
    }

    /// Prove that every independently authored queue-install surface names
    /// the same physical queue before its first outer producer is published.
    fn validate_render_queue_consistency(
        &mut self, pair_slot: u8,
        dev: &AsahiDevice,
        tiling: bool,
        header: g17_resources::G17PRenderConfigUpdateHeader,
        qos_record: [u8; 8],
        prepared_contexts: Option<[u32; 2]>,
    ) -> Result {
        let (scratch, queue_id, entry_low, entry_high, config, producer) = {
            let queues = self.render_slots.get(pair_slot).sksm.as_ref().ok_or(ENODEV)?;
            let queue = if tiling {
                &queues.tiling
            } else {
                &queues.fragment
            };
            (
                queues.scratch,
                queue.queue_id,
                queue.entry_low,
                queue.entry_high,
                queue.config,
                queue.producer,
            )
        };
        let (pair, descriptor_context_id, descriptor_contexts, render_root_slot, pb_binding,
            context_update, scratch_equal) = {
            let storage = self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?;
            (
                storage.queue_pair(),
                storage.descriptor_context_id(),
                match prepared_contexts {
                    Some(contexts) => contexts,
                    None => storage.descriptor_context_ids()?,
                },
                storage.render_root_slot(),
                storage.parameter_buffer_binding(),
                storage.context_update_pending(),
                storage.scratch_handoff_matches(tiling, header),
            )
        };
        let header_qid = u8::try_from(header.queue_id).map_err(|_| EINVAL)?;
        // Native sources call queue +0x1c both the channel priority and the
        // completion selector. It feeds tag-15 +0x1e, entry-header bits 59+
        // and the SKSM routing payload. It is not a completion-ring ordinal:
        // normal CL selector 2 is consumed from descriptor ordinal 0.
        let queue_selector = u8::try_from(header.priority).map_err(|_| EINVAL)?;
        let data_master = u8::try_from(header.data_master).map_err(|_| EINVAL)?;
        let expected = g17_submission::prepare_g17p_render_sksm_queue_config(
            g17_submission::G17PComputeQueueRouting {
                queue_id: header_qid,
                completion_selector: u32::from(queue_selector),
                entry_gpu_va: header.entry_low,
                geometry: producer.geometry,
            },
            u32::from(data_master),
            scratch,
        )
        .map_err(|_| EINVAL)?;
        let expected_data_master = if tiling {
            g17_submission::G17P_TA_DATA_MASTER
        } else {
            g17_submission::G17P_3D_DATA_MASTER
        };
        let class_equal = qos_record[1] == producer.qos.class_40;
        let descriptor_context = descriptor_contexts[usize::from(!tiling)];
        let context_equal = header.context_id == descriptor_context_id
            && u32::from(header.context_id) == descriptor_context;
        let pb_equal = !tiling
            || (header.parameter_buffer_object == pb_binding.0
                && header.parameter_buffer_token == pb_binding.1
                && header.parameter_buffer_id == pb_binding.2
                && header.parameter_buffer_bit == u32::from(pair.fragment));
        if header.selector != 0x0f
            || header_qid != queue_id
            || header_qid != producer.queue_id
            || header.entry_low != entry_low
            || header.entry_high != entry_high
            || !scratch_equal
            || u32::from(data_master) != expected_data_master
            || queue_selector != producer.completion_selector
            || header.field_56 != u16::from(producer.qos.queue_byte_32)
            || qos_record[0] != producer.qos.queue_byte_32
            || !context_equal
            || header.context_update != u16::from(context_update)
            || !pb_equal
            || config != expected
        {
            dev_err!(
                dev.as_ref(),
                "G17P render install split: stage={} qid={}/{}/{} dm={}/{} selector={}/{} entry={:#x}/{:#x} expected={:#x}/{:#x} hwid={}/{}/{} qos={:02x?}\n",
                if tiling { "TA" } else { "3D" },
                header.queue_id,
                queue_id,
                producer.queue_id,
                header.data_master,
                expected_data_master,
                header.priority,
                producer.completion_selector,
                header.entry_low,
                header.entry_high,
                entry_low,
                entry_high,
                header.field_56,
                producer.qos.queue_byte_32,
                qos_record[0],
                qos_record,
            );
            dev_err!(
                dev.as_ref(),
                "G17P render binding split: stage={} context(tag/owner/descriptor)={}/{}/{} update={}/{} root-slot={} pb(object/token/id/partner)={:#x}/{:#x}/{}/{} expected={:#x}/{:#x}/{}/{}\n",
                if tiling { "TA" } else { "3D" },
                header.context_id,
                descriptor_context_id,
                descriptor_context,
                header.context_update,
                u16::from(context_update),
                render_root_slot,
                header.parameter_buffer_object,
                header.parameter_buffer_token,
                header.parameter_buffer_id,
                header.parameter_buffer_bit,
                pb_binding.0,
                pb_binding.1,
                pb_binding.2,
                pair.fragment,
            );
            return Err(EINVAL);
        }
        if header.install != 0 && pair.slot() != 0 {
            dev_info!(
                dev.as_ref(),
                "G17P render install consistent: stage={} qid={} dm={} selector={} entry={:#x}/{:#x} tag={:#x} payload={:#x} masks={:#x}/{:#x} context={} hwid={} pb={:#x}/id{}/bit{:#x} qos-class={} kick-class={} class-equal={}\n",
                if tiling { "TA" } else { "3D" },
                queue_id,
                data_master,
                queue_selector,
                entry_low,
                entry_high,
                config.tag,
                config.payload,
                config.enable_pair.word0,
                config.enable_pair.word1,
                header.context_id,
                header.field_56,
                header.parameter_buffer_object,
                header.parameter_buffer_id,
                header.parameter_buffer_bit,
                qos_record[1],
                producer.qos.class_40,
                class_equal,
            );
        }
        Ok(())
    }

    /// Commit firmware ownership only after the complete inner and outer
    /// channel publication succeeded. This is intentionally infallible: once
    /// the producer is visible to firmware, host state must describe it.
    fn commit_render_firmware_install(&mut self, pair_slot: u8, tiling: bool, install: bool) {
        if !install {
            return;
        }
        let queues = self.render_slots.get_mut(pair_slot)
            .sksm
            .as_mut()
            .expect("validated render firmware install has queue storage");
        let queue = if tiling {
            &mut queues.tiling
        } else {
            &mut queues.fragment
        };
        queue.registered = true;
        queue.hardware_enabled = true;
    }

    /// Directly install both halves of a fresh non-bootstrap render pair after
    /// the live-boot owner has completed its retained-provider/GPC preflight.
    ///
    /// The cold qid0/1 pair is proven to install through its simultaneous
    /// firmware tag-15 publication. A secondary pair is different: qid2 TA
    /// executes, but its qid3 parent reaches DM1 without launching unless the
    /// two native KSMFE routes already exist. Native configures both queue
    /// objects before publishing either outer command. Never expose a fresh
    /// pair after only one configuration landed.
    fn configure_fresh_nonbootstrap_render_pair(
        &mut self, pair_slot: u8,
        registers: &regs::Resources,
        fragment_install: bool,
        tiling_install: Option<bool>,
    ) -> Result {
        if !fragment_install && !tiling_install.unwrap_or(false) {
            return Ok(());
        }
        let pair = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?.queue_pair();
        if pair.slot() == 0 {
            return Ok(());
        }
        if !fragment_install || tiling_install != Some(true) {
            return Err(Self::render_stage_fail("secondary-pair-partial-install", EINVAL));
        }
        let disable_before_reconfigure =
            module_parameters::g17p_render_disable_before_reconfigure.value() != 0;
        let fender_bracket = module_parameters::g17p_render_fender_bracket.value() != 0;
        let (fragment, tiling, disable) = {
            let queues = self.render_slots.get(pair_slot).sksm.as_ref().ok_or(ENODEV)?;
            // Native `configureHardware` on a queue it already enabled
            // (`+0xa8` set) writes the disable mask before the tag/data pair.
            // Production rewrote an enabled queue's routing without it (the
            // sibling's first kick repeated its warm-up configure).
            let already_enabled =
                queues.fragment.hardware_enabled || queues.tiling.hardware_enabled;
            (
                (
                    queues.fragment.config.write_pair(),
                    queues.fragment.config.enable_pair,
                ),
                (
                    queues.tiling.config.write_pair(),
                    queues.tiling.config.enable_pair,
                ),
                (disable_before_reconfigure && already_enabled).then_some((
                    queues.fragment.config.disable_pair,
                    queues.tiling.config.disable_pair,
                )),
            )
        };
        let issued = registers.g17p_sksm_configure_powered_render_pair(
            fragment, tiling, disable, fender_bracket,
        )?;
        if !issued {
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P render: pair {}/{} direct configuration did not issue while powered; outer publication withheld\n",
                pair.tiling,
                pair.fragment,
            );
            return Err(EAGAIN);
        }
        if let Some(queues) = self.render_slots.get_mut(pair_slot).sksm.as_mut() {
            queues.fragment.hardware_enabled = true;
            queues.tiling.hardware_enabled = true;
        }
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P render: pair {}/{} directly configured as one transaction before simultaneous outer publication{}{}\n",
            pair.tiling,
            pair.fragment,
            if disable.is_some() { " (disable-first)" } else { "" },
            if fender_bracket { " (fender bracket)" } else { "" },
        );
        Ok(())
    }

    fn publish_render_sksm_group(
        &mut self, pair_slot: u8,
        dev: &AsahiDevice,
        tiling: bool,
        queue_record_gpu_va: u64,
        barriers: &[g17_submission::G17PClBarrierDependency],
        prepared: Option<(g17_submission::G17PClKickProducerState,
            g17_submission::G17PClKickProducerState)>,
    ) -> Result<u64> {
        let ordinal = self.render_slots.get(pair_slot).submission_ordinal;
        let auxiliary_timestamp = self.render_slots.get(pair_slot).auxiliary_timestamp;
        if auxiliary_timestamp == 0 {
            return Err(Self::render_stage_fail("sksm-auxiliary-timestamp", EINVAL));
        }
        let scratch = self.sksm_scratch();
        let uat_owner = self.uat().ttb_base();
        let slot = self.render_slots.get_mut(pair_slot);
        let storage = slot.storage.as_deref_mut().ok_or(ENODEV)?;
        let queues = slot.sksm.as_mut().ok_or(ENODEV)?;
        if queues.scratch != scratch || queues.uat_owner != uat_owner {
            let scratch_differs = queues.scratch != scratch;
            let recorded_uat = queues.uat_owner;
            pr_info!(
                "G17P render: SKSM mismatch scratch_differs={} uat {:#x} vs {:#x}\n",
                scratch_differs,
                recorded_uat,
                uat_owner
            );
            return Err(Self::render_stage_fail("sksm-scratch-uat-mismatch", EINVAL));
        }
        let queue = if tiling {
            &mut queues.tiling
        } else {
            &mut queues.fragment
        };
        if queue.tiling != tiling {
            return Err(Self::render_stage_fail("sksm-queue-role-mismatch", EINVAL));
        }
        let stamp = queue.producer.current_timestamp;
        let after = if let Some((before, after)) = prepared {
            if before != queue.producer { return Err(EIO); }
            after
        } else {
            prepare_render_sksm_entry(storage, dev, tiling, queue.producer,
                ordinal, auxiliary_timestamp, queue_record_gpu_va, barriers)?
        };
        // Entry bytes and their barrier were completed by the exclusive pair
        // preparer. Publish exactly that frontier, as native's announce does.
        queue.producer = after;
        Ok(stamp)
    }

    /// Native QID/stamp of the exact staged fragment command. The completion
    /// retains this token after another physical pair becomes selected.
    pub(crate) fn staged_render_fragment_dependency(&self, pair_slot: u8) -> Result<u64> {
        let queues = self.render_slots.get(pair_slot).sksm.as_ref().ok_or(ENODEV)?;
        let timestamp = if crate::g17_config::RENDER_SPLIT_LAUNCH != 0 {
            queues.fragment.producer.current_timestamp
        } else {
            queues.fragment.producer.last_add_kicks_timestamp
        };
        g17_submission::encode_kick_word(timestamp, queues.fragment.queue_id)
            .map_err(|_| EINVAL)
    }

    /// Stage both render groups with the SKSM half interleaved, in the
    /// publication order the captures show:
    ///
    /// ```text
    /// 3D record -> 3D tag-15 install -> 3D kick entry -> 3D tag-14
    /// TA record -> TA tag-15 install -> TA kick entry -> TA tag-14
    /// ```
    ///
    /// The 3D kick carries the one explicit barrier every captured pass has:
    /// `{count 1, event_word = ta_stamp << 8, queue_id = qta}`, naming the
    /// stamp the TA group is about to announce. The TA kick carries the exact
    /// live logical-prefix dependencies supplied by the runtime, including
    /// predecessors on another physical pair. No host completion wait is used.
    /// TA group's records are handed back as a `G17PLateTilingPublication`
    /// because the outer publisher applies them between the two ring slots.
    /// An overlapping submission is anchored to its immutable predecessor,
    /// not a racing firmware snapshot. Native fragment-first diagnostics still
    /// sample the actual TA write cursor to prove their delayed publication.
    fn render_admission_indices(&mut self, pair_slot: u8)
        -> Result<g17_resources::G17PRenderQueueIndices> {
        let slot = self.render_slots.get_mut(pair_slot);
        if !g17p_native_fragment_first_mode(crate::g17_config::RENDER_NATIVE_DOORBELLS) {
            if let Some(active) = slot.tracker.as_ref() {
                let previous = |target| g17_completion::QueueIndices { done: target, read: target, write: target };
                return Ok(g17_resources::G17PRenderQueueIndices {
                    tiling: previous(active.windows[0].target()),
                    fragment: previous(active.windows[1].target()),
                });
            }
        }
        slot.storage.as_deref_mut().ok_or(ENODEV)?.admission_queue_indices()
    }

    fn stage_render_groups_with_sksm(
        &mut self, pair_slot: u8,
        dev: &AsahiDevice,
        previous_fragment_barriers: &[g17_submission::G17PClBarrierDependency],
        prepared_sksm: Option<G17PRenderPreparedSksm>,
    ) -> Result<(
        u64,
        u64,
        g17_resources::G17PRenderQueueIndices,
        Option<g17_resources::G17PLateTilingPublication>,
        g17_resources::G17PRenderConfigUpdateHeader,
    )> {
        let ordinal = self.render_slots.get(pair_slot).submission_ordinal;
        let hardware_buffer_id = u8::try_from(
            self.render_slots.get(pair_slot).storage
                .as_deref()
                .ok_or(ENODEV)?
                .qos_hardware_buffer_id()?,
        )
        .map_err(|_| EOVERFLOW)?;
        {
            // QoS belongs to the logical queue across all three engines;
            // physical PB/PM allocation and QID identity are separate.
            let queues = self.render_slots.get_mut(pair_slot).sksm.as_mut().ok_or(ENODEV)?;
            queues.tiling.producer.qos.queue_byte_32 = hardware_buffer_id;
            queues.fragment.producer.qos.queue_byte_32 = hardware_buffer_id;
        }
        if let Some(prepared) = prepared_sksm {
            let queues = self.render_slots.get(pair_slot).sksm.as_ref().ok_or(ENODEV)?;
            if prepared.before != [queues.tiling.producer, queues.fragment.producer] {
                return Err(Self::render_stage_fail("prepared-sksm-frontier-changed", EIO));
            }
        }
        // Coherent native protocol: the logical channel starts at payload
        // zero even when its physical QID retains a nonzero timestamp base.
        // Keep the legacy experiment isolated until hardware qualification.
        let channel_payload = if module_parameters::g17p_ta_previous_fragment_probe.value() == 2 {
            self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?
                .logical_channel_payload(ordinal)?
        } else {
            g17_submission::g17p_serialized_render_scratch_payload(ordinal)
        };
        let fragment_command_timestamp = self.render_slots.get(pair_slot)
            .sksm
            .as_ref()
            .ok_or(ENODEV)?
            .fragment
            .producer
            .current_timestamp;
        let tiling_command_timestamp = self.render_slots.get(pair_slot).sksm.as_ref().ok_or(ENODEV)?
            .tiling.producer.current_timestamp;
        // Sample before either engine publishes this command. The host stamp
        // uses the retained ordinal, independently of the full SKSM base.
        let scratch_handoff = if module_parameters::g17p_ta_previous_fragment_probe.value() == 2 {
            let pair = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?.queue_pair();
            let stamps = self.resources.render_completion_stamps(pair)?;
            let mut seeds = [None; 2];
            for (index, timestamp) in [tiling_command_timestamp, fragment_command_timestamp]
                .into_iter().enumerate()
            {
                if g17_compute::completion_stamp_covers(stamps[index], ordinal) {
                    seeds[index] = Some(g17_submission::encode_g17p_3d_completion_scratch(
                        timestamp.wrapping_sub(1) & g17_submission::KICK_TS_MASK,
                        channel_payload.wrapping_sub(1),
                    ).map_err(|_| EINVAL)?);
                }
            }
            Some(seeds)
        } else { None };
        let (fragment_queue, tiling_queue, fragment_header) = {
            let storage = self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?;
            let scratch = storage
                .stage_fragment_completion_scratch(
                    tiling_command_timestamp, fragment_command_timestamp, channel_payload,
                )
                .inspect_err(|error| {
                    dev_err!(
                        dev.as_ref(),
                        "G17P render: fragment completion scratch stage failed ({:?})\n",
                        error
                    );
                })?;
            if let Some(seeds) = scratch_handoff {
                storage.stage_render_scratch_handoff(seeds)?;
            }
            if crate::util::g17p_verbose() {
                dev_info!(
                    dev.as_ref(),
                    "G17P render: 3D completion scratch={:#018x} timestamp={:#x} payload={}\n",
                    scratch,
                    fragment_command_timestamp,
                    channel_payload,
                );
            }
            storage
                .dump_submit_ready_fragment_descriptor(dev, ordinal)
                .inspect_err(|error| {
                    dev_err!(
                        dev.as_ref(),
                        "G17P render: submit-ready fragment descriptor dump failed ({:?})\n",
                        error
                    );
                })?;
            let header = storage.stage_fragment_prefix(ordinal).inspect_err(|error| {
                dev_err!(
                    dev.as_ref(),
                    "G17P render: fragment prefix stage failed ({:?})\n",
                    error
                );
            })?;
            let (fragment_queue, tiling_queue) = storage.queue_gpu_vas();
            (fragment_queue, tiling_queue, header)
        };
        let (ta_stamp, ta_queue_id) = {
            let queues = self.render_slots.get(pair_slot).sksm.as_ref().ok_or(ENODEV)?;
            (
                queues.tiling.producer.current_timestamp,
                queues.tiling.queue_id,
            )
        };
        let split_launch =
            crate::g17_config::RENDER_SPLIT_LAUNCH != 0;
        let native_fragment_first = g17p_native_fragment_first_mode(
            crate::g17_config::RENDER_NATIVE_DOORBELLS,
        );
        if !split_launch {
            let barrier = [g17_submission::g17p_render_barrier_from_timestamp(
                ta_stamp, ta_queue_id,
            ).map_err(|_| Self::render_stage_fail("sksm-event-word", EINVAL))?];
            let event_word = barrier[0].event_word;
            // DIAGNOSTIC, not a fix. Metal reserves a barrier SLOT per render
            // pass; suppressing the dependency asks only whether that missing
            // lifecycle is what stalls the fragment group.
            let suppress_barrier =
                crate::g17_config::RENDER_NO_BARRIER != 0;
            let barrier_slice: &[g17_submission::G17PClBarrierDependency] =
                if suppress_barrier { &[] } else { &barrier };
            if suppress_barrier {
                pr_info!(
                    "G17P render: DIAGNOSTIC barrier dependency suppressed (event_word {:#x} qid {})\n",
                    event_word,
                    ta_queue_id
                );
            }
            let fragment_first_install = self.render_slots.get(pair_slot)
                .sksm
                .as_ref()
                .is_some_and(|queues| !queues.fragment.registered);
            if fragment_first_install {
                if let Err(error) = self.log_render_context_route(pair_slot, "before-3d-qid-install") {
                    dev_warn!(dev.as_ref(), "G17P render route: pre-3D snapshot failed ({:?})\n", error);
                }
            }
            let fragment_stamp = self
                .publish_render_sksm_group(pair_slot, dev, false, fragment_queue, barrier_slice,
                    prepared_sksm.map(|prepared| (prepared.before[1], prepared.after[1])))
                .map_err(|error| Self::render_stage_fail("sksm-publish-fragment", error))?;
            if fragment_first_install {
                if let Err(error) = self.log_render_context_route(pair_slot, "after-3d-qid-install") {
                    dev_warn!(dev.as_ref(), "G17P render route: post-3D snapshot failed ({:?})\n", error);
                }
            }
            self.render_slots.get_mut(pair_slot).storage
                .as_deref_mut()
                .ok_or(ENODEV)?
                .stage_fragment_kick(ordinal, fragment_stamp)
                .inspect_err(|error| {
                    dev_err!(
                        dev.as_ref(),
                        "G17P render: fragment AddKicks stage failed ({:?})\n",
                        error
                    );
                })?;
        } else {
            dev_info!(
                dev.as_ref(),
                "G17P render split-launch: retaining 3D SKSM entry and AddKicks until TA completion\n"
            );
        }
        let previous = self.render_admission_indices(pair_slot)
            .inspect_err(|error| {
                dev_err!(
                    dev.as_ref(),
                    "G17P render: initial snapshot failed ({:?})\n",
                    error
                );
            })?;
        if native_fragment_first {
            // Modes 2/3 return with a COMPLETE 3D group but no TA item/SKSM
            // publication. The caller retains the TA outer producer, sends
            // the 3D doorbell, and invokes `publish_native_render_tiling_group`
            // to construct TA in its exact tag15 -> SKSM -> tag14 chronology.
            let queues = self.render_slots.get(pair_slot).sksm.as_ref().ok_or(ENODEV)?;
            if queues.tiling.producer.current_timestamp != ta_stamp {
                return Err(Self::render_stage_fail(
                    "native-ta-sksm-advanced-before-3d-doorbell",
                    EINVAL,
                ));
            }
            dev_info!(
                dev.as_ref(),
                "G17P render native chronology: complete 3D visible; retaining complete TA group until after Fragment doorbell\n"
            );
            return Ok((fragment_queue, tiling_queue, previous, None, fragment_header));
        }
        let tiling_first_install = self.render_slots.get(pair_slot)
            .sksm
            .as_ref()
            .is_some_and(|queues| !queues.tiling.registered);
        if tiling_first_install {
            if let Err(error) = self.log_render_context_route(pair_slot, "before-ta-qid-install") {
                dev_warn!(dev.as_ref(), "G17P render route: pre-TA snapshot failed ({:?})\n", error);
            }
        }
        let tiling_stamp = self
            .publish_render_sksm_group(pair_slot, dev, true, tiling_queue, previous_fragment_barriers,
                prepared_sksm.map(|prepared| (prepared.before[0], prepared.after[0])))
            .map_err(|error| Self::render_stage_fail("sksm-publish-tiling", error))?;
        if tiling_first_install {
            if let Err(error) = self.log_render_context_route(pair_slot, "after-ta-qid-install") {
                dev_warn!(dev.as_ref(), "G17P render route: post-TA snapshot failed ({:?})\n", error);
            }
        }
        // The barrier the 3D kick already carries names this exact value.
        if tiling_stamp != ta_stamp {
            pr_info!(
                "G17P render: SKSM tiling stamp {} != barrier stamp {}\n",
                tiling_stamp,
                ta_stamp
            );
            return Err(Self::render_stage_fail("sksm-tiling-stamp-mismatch", EINVAL));
        }
        let late_tiling = self.render_slots.get_mut(pair_slot)
            .storage
            .as_deref_mut()
            .ok_or(ENODEV)?
            .prepare_late_tiling_publication_with_stamp(ordinal, tiling_stamp)
            .inspect_err(|error| {
                dev_err!(dev.as_ref(), "G17P render: tiling stage failed ({:?})\n", error);
            })?;
        Ok((fragment_queue, tiling_queue, previous, Some(late_tiling), fragment_header))
    }

    /// A selected pair whose graph and application root are both retained:
    /// its next render takes the warm (restage) path, never construction.
    pub(crate) fn render_pair_is_warm(&self, pair_slot: u8) -> bool {
        self.render_slots.get(pair_slot).storage.is_some() && self.render_slots.get(pair_slot).bind.is_some()
    }

    /// Construct the selected pair's graph for `execution_context` without
    /// publishing a render: the construction half of
    /// `begin_translated_render_with_hardware_buffer`'s fresh branch, run by
    /// `G17PLiveRuntime::warm_render_pair` while the owning logical queue has
    /// no live kick. The per-kick half -- FList lease, PB lease token, SKSM
    /// entries, tag-15 install, AddKicks, doorbell -- stays with the first
    /// render, which then finds a retained graph and restages onto it.
    ///
    /// `command` only sizes the initial layout (render-state, RT memory, TVB
    /// floor); a later render with a larger target resizes through
    /// `restage_command`'s existing capacity path.
    pub(crate) fn warm_render_pair_storage(
        &mut self,
        dev: &AsahiDevice,
        vm: &mmu::Vm,
        queue_pair: g17_resources::G17PRenderQueuePair,
        execution_context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
        command: &g17_uapi::TranslatedRenderCommand,
        registers: &regs::Resources,
        registration_power: &mut impl FnMut(&mut Self) -> Result,
    ) -> Result {
        let pair_slot = queue_pair.slot();
        if self.render_queue_pair(queue_pair.slot())? != queue_pair { return Err(EIO); }
        if let Some(leases) = self.render_channel_leases[usize::from(queue_pair.slot())] {
            for lease in leases {
                if !self.channel_ids.publish(lease) { return Err(EIO); }
            }
        }
        self.ensure_render_slot(queue_pair.slot())?;
        if !execution_context.matches(vm) || !execution_context.is_current() {
            return Err(EFAULT);
        }
        if self.render_pair_is_warm(pair_slot) {
            return Ok(());
        }
        if self.render_slots.get(pair_slot).storage.is_some() || self.render_slots.get(pair_slot).bind.is_some() {
            return Err(Self::render_stage_fail("warm-pair-half-retained", EBUSY));
        }
        if self.render_slots.get(pair_slot).context_aliases.is_some() {
            return Err(Self::render_stage_fail("legacy-context0-alias-live", EBUSY));
        }
        if self.render_slots.get(pair_slot).tracker.is_some() || !self.render_slots.get(pair_slot).queued_tracker.is_none() {
            return Err(Self::render_stage_fail("warm-pair-tracker-live", EBUSY));
        }
        let descriptor_context_id = execution_context.context_id() as u16;
        let render_root_slot = u8::try_from(execution_context.context_id())
            .map_err(|_| Self::render_stage_fail("render-root-slot-width", EIO))?;
        self.resources
            .stage_render_region_views()
            .map_err(|error| Self::render_stage_fail("stage-region-views", error))?;
        // A fresh storage is encoded against a TA HardwareBufferID and reserves
        // it for its lifetime (see `begin_translated_render`). Take one exactly
        // as a first render would; the per-render reference is returned below
        // because no render is published here.
        let warm_state = Arc::new(
            g17_resources::G17PWorkStateLease::deferred(execution_context.clone()), GFP_KERNEL,
        )?;
        let (hardware_buffer_id, _parameter_buffer_token) = self
            .render_hardware_buffer_ids
            .allocate_with_token()
            .map_err(|_| EBUSY)?;
        let command_buffer_state_word = match self.allocate_active_render_command_buffer_state(pair_slot, warm_state) {
            Ok(word) => word,
            Err(error) => {
                self.render_hardware_buffer_ids
                    .release(hardware_buffer_id)
                    .map_err(|_| EIO)?;
                return Err(error);
            }
        };
        let mut command = *command;
        command.parameters.ta_hardware_buffer_id = hardware_buffer_id;
        command.parameters.context_generation = execution_context.generation();
        let result = self.warm_render_pair_storage_inner(
            dev,
            vm,
            queue_pair,
            execution_context,
            descriptor_context_id,
            render_root_slot,
            &command,
            [command_buffer_state_word; 2],
            registers,
            registration_power,
        );
        // No render was published: return the per-render command-buffer state
        // and the per-render ID reference. The storage's own reservation of
        // the ID (taken inside) survives, exactly like a retained pair between
        // two renders.
        if let Err(error) = self.release_active_render_command_buffer_states(pair_slot) {
            dev_warn!(
                dev.as_ref(),
                "G17P render warm-up: command-buffer state not returned ({:?})\n",
                error,
            );
        }
        if let Err(_) = self.render_hardware_buffer_ids.release(hardware_buffer_id) {
            dev_warn!(
                dev.as_ref(),
                "G17P render warm-up: TA HardwareBufferID {} reference not returned\n",
                hardware_buffer_id,
            );
        }
        result
    }

    fn warm_render_pair_storage_inner(
        &mut self,
        dev: &AsahiDevice,
        vm: &mmu::Vm,
        queue_pair: g17_resources::G17PRenderQueuePair,
        execution_context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
        descriptor_context_id: u16,
        render_root_slot: u8,
        command: &g17_uapi::TranslatedRenderCommand,
        command_buffer_state_words: [u32; 2],
        registers: &regs::Resources,
        registration_power: &mut impl FnMut(&mut Self) -> Result,
    ) -> Result {
        let pair_slot = queue_pair.slot();
        let storage_box = KBox::<g17_resources::G17PUserRenderStorage>::new_uninit(GFP_KERNEL)?;
            let mut retained_sksm_backing = self.render_slots.get_mut(pair_slot)
            .sksm
            .as_mut()
            .and_then(|queues| queues.backing.take());
        let pm_generation = self.prepare_render_pm_manager_generation()?;
        let storage = self.resources.new_user_render_storage(
            storage_box,
            dev,
            vm,
            queue_pair,
            Some(execution_context.clone()),
            descriptor_context_id,
            Some(render_root_slot),
            command,
            self.num_clusters,
            command_buffer_state_words,
            self.render_slots.get(pair_slot).submission_ordinal,
            pm_generation,
            &mut retained_sksm_backing,
        );
        self.render_slots.get_mut(pair_slot).storage = Some(match storage {
            Ok(storage) if retained_sksm_backing.is_none() => storage,
            Ok(_) => return Err(Self::render_stage_fail(
                "warm-render-sksm-backing-unclaimed", EIO,
            )),
            Err(error) => {
                if let Some(backing) = retained_sksm_backing.take() {
                    let queues = self.render_slots.get_mut(pair_slot).sksm.as_mut().ok_or(EIO)?;
                    if queues.backing.replace(backing).is_some() {
                        return Err(EIO);
                    }
                }
                return Err(Self::render_stage_fail("warm-render-storage", error));
            }
        });
        self.finish_warm_render_pair_storage(dev, vm, queue_pair,
            execution_context, pm_generation, registers, registration_power)
    }

    fn finish_warm_render_pair_storage(
        &mut self, dev: &AsahiDevice, vm: &mmu::Vm,
        queue_pair: g17_resources::G17PRenderQueuePair,
        execution_context: Arc<g17_resources::G17PLogicalQueueContext>,
        pm_generation: Option<u64>, registers: &regs::Resources,
        registration_power: &mut impl FnMut(&mut Self) -> Result,
    ) -> Result {
        let pair_slot = queue_pair.slot();
        let descriptor_context_id = execution_context.context_id() as u16;
        self.commit_render_pm_manager_generation(pair_slot, pm_generation)
            .map_err(|error| Self::render_stage_fail("commit-pm-manager-generation", error))?;
        if let Some(id) = self.render_slots.get(pair_slot)
            .storage
            .as_deref()
            .and_then(g17_resources::G17PUserRenderStorage::retained_hardware_buffer_id)
        {
            self.render_hardware_buffer_ids
                .reserve_with_owner_ref(id, crate::g17_host_path::enabled(crate::g17_host_path::PB_OWNER_REF))
                .map_err(|_| Self::render_stage_fail("reserve-hardware-buffer-id", EIO))?;
        }
        if self.render_slots.get(pair_slot).submission_ordinal != 0 {
            // A slot that already published renders keeps its stamp sequence;
            // the fresh graph's rings start at that ordinal. The first render
            // restages its own command onto the retained graph.
            let slot = self.render_slots.get_mut(pair_slot);
            slot.storage
                .as_deref_mut()
                .ok_or(ENODEV)?
                .rebase_empty_queue_cursors(slot.submission_ordinal)
                .map_err(|error| Self::render_stage_fail("rebase-warm-queue", error))?;
        }
        // The constructor stamped the device-global PB loader from this graph.
        self.render_parameter_buffer_owner =
            self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?.render_pool_id();
        // From here the graph and its root are one reusable, retained pair:
        // every later step is one the retained render path repeats itself.
        self.render_slots.get_mut(pair_slot).bind = Some(G17PRenderRootOwner::Job {
            context: execution_context.clone(),
        });
        if crate::g17_config::RENDER_USC_FREELIST != 0 {
            let id = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?.render_flist_id() as usize;
            if id >= self.render_flist_owners.len() { return Err(EINVAL); }
            if self.render_slots.get(pair_slot).flist_index != id
                && self.render_slots.get(pair_slot).flist_leases.is_some() { return Err(EBUSY); }
            self.render_slots.get_mut(pair_slot).flist_index = id;
            let storage = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?;
            // A context's new pool can recycle the previous owner's FList ID
            // and every VA in the tuple. It is still a new owner: its image
            // and UMA descriptor row must be staged (native
            // populateInitialFirmwareState + halLoadUMADesc), never the
            // retained row whose cursors firmware advanced for the old pool.
            let owner_matches = self.render_flist_owners[self.render_slots.get(pair_slot).flist_index].as_ref().is_some_and(|owner| {
                owner.matches_owner(
                    storage.render_flist_id(),
                    storage.render_flist_pointer(),
                    storage.render_flist_operand_table(),
                    storage.render_flist_cookie(),
                ) && owner.same_pool_object(storage.render_flist_serial())
            });
            if !owner_matches {
                if self.render_flist_owners[id].as_ref().is_some_and(|old|
                    old.state != G17PBootstrapUmaRequestState::Completed
                        || old.hardware_buffer_owned || old.has_release_witness()) { return Err(EBUSY); }
                // Warming may prepare a descriptor but does not acquire a
                // firmware lease. Apply the same ownership preflight as the
                // submission path before touching the global UMA slot.
                G17PBootstrapUmaLifecycle::acquire_owner(
                    self.render_flist_owners[self.render_slots.get(pair_slot).flist_index].as_ref(),
                    self.render_flist_owners[self.render_slots.get(pair_slot).flist_index].as_ref().map_or(Ok(G17P_BOOTSTRAP_UMA_SEQUENCE), |owner| owner.sequence.checked_add(1).ok_or(EOVERFLOW))?,
                    storage.render_flist_id(),
                    storage.render_flist_pointer(),
                    storage.render_flist_operand_table(),
                    storage.render_flist_cookie(),
                    storage.render_flist_needs_grow(),
                ).map_err(|_| EBUSY)?;
                if let Ok((id, [submitted, completed])) = storage.render_flist_takeover_counters() {
                    let takeovers = G17P_FLIST_TAKEOVERS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
                    let outstanding = submitted != completed;
                    if outstanding || crate::g17_recovery_path::log_budget(takeovers, 16, 256) {
                        let note = if outstanding { " OUTSTANDING" } else { "" };
                        let row = self.resources.uma_row_counters(u32::from(id));
                        dev_info!(self.resources.dev().as_ref(),
                            "G17P FList takeover #{}: pair={} id={} submitted={} completed={} previous-uma-row(a,b,total)={:x?}{}\n",
                            takeovers, pair_slot, id, submitted, completed, row, note);
                    }
                }
                storage.repopulate_render_flist()
                    .map_err(|error| Self::render_stage_fail("repopulate-render-flist", error))?;
                self.resources.stage_user_render_uma(storage)
                    .map_err(|error| Self::render_stage_fail("stage-render-uma-owner", error))?;
            }
        }
        {
            let storage = self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?;
            if let Some(failed) = storage.first_unreachable_cached(self.resources.uat(), vm) {
                storage.log_reachability(dev, self.resources.uat(), vm);
                dev_err!(
                    dev.as_ref(),
                    "G17P render warm-up: storage unreachable, first failing predicate is {}\n",
                    failed
                );
                return Err(EFAULT);
            }
        }
        if crate::g17_config::RENDER_SKSM != 0 && self.render_slots.get(pair_slot).sksm.is_none() {
            let (pair, hardware_buffer_id, entry_vas) = {
                let storage = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?;
                (
                    storage.queue_pair(),
                    u8::try_from(storage.qos_hardware_buffer_id()?)
                        .map_err(|_| EOVERFLOW)?,
                    [
                        (
                            storage.sksm_entry_low_va(true),
                            storage.sksm_entry_high_va(true),
                        ),
                        (
                            storage.sksm_entry_low_va(false),
                            storage.sksm_entry_high_va(false),
                        ),
                    ],
                )
            };
            self.render_slots.get_mut(pair_slot).sksm = Some(
                self.new_render_sksm_queues(pair, hardware_buffer_id, entry_vas)
                    .map_err(|error| Self::render_stage_fail("new-render-sksm-queues", error))?,
            );
        }
        // Native configures both queue objects at work-queue creation, before
        // any outer command. Do the same direct KSMFE configuration now, under
        // registration power, while no kick of this owner is live. The tag-15
        // install record itself stays with the first kick (`registered` is
        // left false), which repeats this configuration idempotently.
        // `g17p_render_configure_at_warmup=0` is the native shape: creation
        // does no KSMFE MMIO (AGXKSMKickQueue::init 0x88cc5d4 has none); the
        // first kick configures the queue (`configureHardware` from the
        // submitBuffer gate).
        let fresh_routes = crate::g17_config::RENDER_SKSM != 0
            && queue_pair.slot() != 0
            && module_parameters::g17p_render_configure_at_warmup.value() != 0
            && self.render_slots.get(pair_slot).sksm.as_ref().is_some_and(|queues| {
                !queues.tiling.registered && !queues.fragment.registered
            });
        if fresh_routes {
            registration_power(self)?;
            self.configure_fresh_nonbootstrap_render_pair(pair_slot, registers, true, Some(true))?;
        }
        let (registered, hardware_enabled) = self.render_slots.get(pair_slot).sksm.as_ref()
            .map_or((false, false), |queues| (
                queues.tiling.registered && queues.fragment.registered,
                queues.tiling.hardware_enabled && queues.fragment.hardware_enabled,
            ));
        dev_info!(
            dev.as_ref(),
            "G17P render: pair {}/{} warmed for context {} before any kick (graph, PM, FList, SKSM queues{}) [slot {} decision: {} registered={} hardware_enabled={}]\n",
            queue_pair.tiling,
            queue_pair.fragment,
            descriptor_context_id,
            if fresh_routes { ", KSMFE routes" } else { "" },
            pair_slot,
            if registered { "rebind (+0x1a=0 at kick, no host KSMFE write)" }
            else { "first-install pending (+0x1a=1 at first kick)" },
            registered,
            hardware_enabled,
        );
        Ok(())
    }

    /// Capture only identities and ownership under Runtime. No graph build,
    /// page allocation or VM-alias construction is performed here.
    pub(crate) fn take_warm_render_graph(
        &mut self, vm: &mmu::Vm, pair: g17_resources::G17PRenderQueuePair,
        context: Arc<g17_resources::G17PLogicalQueueContext>,
        work_state: Arc<g17_resources::G17PWorkStateLease>,
        command: &g17_uapi::TranslatedRenderCommand,
    ) -> Result<Option<KBox<G17PWarmRenderGraph>>> {
        let index = pair.slot();
        self.ensure_render_slot(index)?;
        if self.render_queue_pair(index)? != pair { return Err(EIO); }
        if !context.matches(vm) || !context.is_current() { return Err(EFAULT); }
        let slot = self.render_slots.get(index);
        if slot.preparing { return Err(EAGAIN); }
        if self.render_pair_is_warm(index) { return Ok(None); }
        if slot.storage.is_some() || slot.bind.is_some() || slot.context_aliases.is_some()
            || slot.tracker.is_some() || slot.queued_tracker.is_some()
            || slot.restaged.is_some() || slot.hardware_buffer_id.is_some() {
            return Err(EBUSY);
        }
        let ordinal = slot.submission_ordinal;
        if work_state.word() == u32::MAX { return Err(EINVAL); }
        self.resources.stage_render_region_views()?;
        let parameter_metrics = self.resources.acquire_parameter_metrics_region()?;
        let pm_generation = self.prepare_render_pm_manager_generation()?;
        let num_clusters = self.num_clusters;
        let uat = self.resources.owned_uat();
        let mut request = KBox::init(try_init!(G17PWarmRenderGraph {
            installed: false, graph: None, retained_sksm_backing: None,
            parameter_metrics: Some(parameter_metrics), queue_pair: pair, ordinal,
            hardware_buffer_id: None, pm_generation, context, work_state,
            vm: vm.clone(), command: *command, num_clusters, uat,
        }), GFP_KERNEL)?;
        if let Some(leases) = self.render_channel_leases[usize::from(index)] {
            for lease in leases { if !self.channel_ids.publish(lease) { return Err(EIO); } }
        }
        let (id, _) = self.render_hardware_buffer_ids.allocate_with_token().map_err(|_| EBUSY)?;
        request.hardware_buffer_id = Some(id);
        request.command.parameters.ta_hardware_buffer_id = id;
        request.command.parameters.context_generation = request.context.generation();
        let slot = self.render_slots.get_mut(index);
        request.retained_sksm_backing = slot.sksm.as_mut().and_then(|q| q.backing.take());
        slot.preparing = true;
        Ok(Some(request))
    }

    /// Always finish ownership, even on cancellation or invalidated epoch.
    /// The caller keeps `request` and drops its uninstalled graph off Runtime.
    #[inline(never)]
    pub(crate) fn return_warm_render_graph(
        &mut self, dev: &AsahiDevice, request: &mut G17PWarmRenderGraph, commit: bool,
        registers: &regs::Resources,
        registration_power: &mut impl FnMut(&mut Self) -> Result,
    ) -> Result {
        let index = request.queue_pair.slot();
        let slot = self.render_slots.get(index);
        assert!(slot.preparing && slot.storage.is_none());
        assert_eq!(slot.submission_ordinal, request.ordinal);
        let result = (|| {
            if !commit { return Ok(()); }
            if !request.context.is_current() || request.vm.status().get() != 0 { return Err(EFAULT); }
            self.resources.finish_user_render_storage(request.graph.as_mut().ok_or(EIO)?,
                &mut request.retained_sksm_backing)?;
            let graph = request.graph.take().ok_or(EIO)?;
            self.render_slots.get_mut(index).storage = Some(graph.into_storage());
            request.installed = true;
            self.finish_warm_render_pair_storage(dev, &request.vm, request.queue_pair,
                request.context.clone(), request.pm_generation, registers, registration_power)
        })();
        // These identities were exclusively removed from this pair and the
        // epoch prevents replacement/reset until they have been returned.
        let restored = (|| {
            if let Some(graph) = request.graph.as_mut() {
                graph.reclaim_retained_sksm_backing(&mut request.retained_sksm_backing)?;
            }
            if request.retained_sksm_backing.is_some() {
                let queues = self.render_slots.get_mut(index).sksm.as_mut()
                    .expect("reserved warm backing retains its SKSM owner");
                assert!(queues.backing.is_none());
                queues.backing = request.retained_sksm_backing.take();
            }
            Ok(())
        })();
        self.render_slots.get_mut(index).preparing = false;
        self.rekick_parked_render_grows(Some(index));
        let released = self.render_hardware_buffer_ids.release(
            request.hardware_buffer_id.take().expect("warm build owns temporary HWPB reference"),
        ).map_err(|_| EIO);
        result.and(restored).and(released)
    }

    pub(crate) fn take_render_restage(
        &mut self, dev: &AsahiDevice, vm: &mmu::Vm,
        queue_pair: g17_resources::G17PRenderQueuePair,
        context: Arc<g17_resources::G17PLogicalQueueContext>,
        work_state: Arc<g17_resources::G17PWorkStateLease>,
        command: &g17_uapi::TranslatedRenderCommand,
        dependencies: &G17PRenderDependencies,
    ) -> Result<Option<G17PRenderRestage>> {
        let pair_slot = queue_pair.slot();
        let slot = self.render_slots.allocated(pair_slot)?;
        if slot.preparing || slot.restaged.is_some() { return Err(EBUSY); }
        if slot.storage.is_none() { return Ok(None); }
        self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(EIO)?
            .ensure_sksm_dependency_buffer()?;
        let reservation = self.reserve_translated_render(dev, vm, queue_pair,
            context, work_state.clone(), command)?;
        if self.render_slots.get(pair_slot).storage.is_none() {
            self.cancel_render_restage(pair_slot)?;
            return Ok(None);
        }
        let producers = if crate::g17_config::RENDER_SKSM != 0
            && crate::g17_config::RENDER_NATIVE_DOORBELLS <= 1
            && crate::g17_config::RENDER_SPLIT_LAUNCH == 0 {
            self.render_slots.get(pair_slot).sksm.as_ref().map(|queues|
                [queues.tiling.producer, queues.fragment.producer])
        } else { None };
        let slot = self.render_slots.get_mut(pair_slot);
        let mut storage = slot.storage.take().ok_or(EIO)?;
        let mut dependency_buffer = storage.take_sksm_dependency_buffer();
        // Heap to heap: the ~1 KiB dependency set never stages on the stack.
        dependency_buffer.entries = dependencies.entries;
        dependency_buffer.count = dependencies.count;
        slot.preparing = true;
        Ok(Some(G17PRenderRestage { work_state, storage, reservation, num_clusters: self.num_clusters,
            uat: self.resources.owned_uat(), producers, dependencies: dependency_buffer }))
    }

    pub(crate) fn return_render_restage(&mut self, prepared: G17PRenderRestage) -> (u8, Result) {
        let G17PRenderRestage { mut storage, mut reservation, dependencies, .. } = prepared;
        let host_tvb_grow = reservation.host_tvb_grow.take();
        storage.return_sksm_dependency_buffer(dependencies);
        let pair_slot = reservation.queue_pair.slot();
        let slot = self.render_slots.get_mut(pair_slot);
        // Recovery/removal joins the epoch and all pair mutation paths defer
        // while preparing. No fallible exit may discard this installed graph.
        debug_assert!(slot.preparing && slot.storage.is_none());
        debug_assert_eq!(slot.submission_ordinal, reservation.ordinal);
        if self.render_flist_owners[slot.flist_index].as_ref().is_some_and(|owner|
            owner.request.is_some() && matches!(owner.state, G17PBootstrapUmaRequestState::Ready)
                && owner.matches_owner(storage.render_flist_id(),
                    storage.render_flist_pointer(), storage.render_flist_operand_table(),
                    storage.render_flist_cookie())
                && owner.same_pool_object(storage.render_flist_serial())) {
            storage.mark_render_flist_grown();
        }
        slot.storage = Some(storage);
        slot.restaged = Some(reservation);
        slot.preparing = false;
        self.rekick_parked_render_grows(Some(pair_slot));
        // Return the installed graph before any fallible shared-table write.
        // A failed restage may already have enlarged its private PM inventory;
        // cancellation must preserve that inventory and finish its descriptor.
        let result = match host_tvb_grow {
            Some(grow) => self.resources.commit_host_tvb_grow(&grow),
            None => Ok(()),
        };
        (pair_slot, result)
    }

    pub(crate) fn render_sksm_prepared(&self, pair_slot: u8) -> bool {
        self.render_slots.allocated(pair_slot).is_ok_and(|slot|
            slot.restaged.as_ref().is_some_and(|r| r.prepared_sksm.is_some()))
    }

    pub(crate) fn render_pair_preparing(&self, pair_slot: u8) -> bool {
        self.render_slots.allocated(pair_slot).is_ok_and(|slot| slot.preparing)
    }

    pub(crate) fn take_render_observation(&mut self, pair_slot: u8)
        -> Result<Option<G17PRenderObservation>>
    {
        let slot = self.render_slots.allocated(pair_slot)?;
        if slot.preparing || slot.restaged.is_some() {
            crate::g17_timing_buffer::record_render_gate(pair_slot,
                slot.tracker.as_ref().map_or(u64::MAX, |t| t.ordinal),
                if slot.preparing { 1 } else { 2 }, 0, [0; 2], [0; 4]);
            return Ok(None);
        }
        let Some(tracker) = slot.tracker.as_ref() else { return Ok(None); };
        // UMA completion writes this graph; preserve the existing grow owner.
        if self.render_flist_owners[slot.flist_index].as_ref().is_some_and(|owner|
            matches!(owner.state, G17PBootstrapUmaRequestState::Submitted)) {
            crate::g17_timing_buffer::record_render_gate(pair_slot, tracker.ordinal,
                3, 0, [0; 2], [0; 4]);
            return Ok(None);
        }
        let windows = tracker.windows;
        if windows[0].target() != tracker.tiling_prefix || windows[1].target() != tracker.fragment_prefix {
            return Err(EIO);
        }
        let ordinal = tracker.ordinal;
        let tiling_previous = tracker.tiling_previous;
        let fragment_previous = tracker.fragment_previous;
        if tracker.work_state.is_some() {
            // Both post-maintenance lanes are mandatory in the final commit.
            // If either is still pending, borrowing/snapshotting this graph
            // cannot retire its front. This is only a rejection prefilter:
            // commit still repeats every ownership/cursor/timestamp/stamp check.
            let pair = slot.storage.as_deref().ok_or(ENODEV)?.queue_pair();
            let expected = ordinal.checked_add(1)
                .map(|value| (value as u32).wrapping_shl(8)).ok_or(EOVERFLOW)?;
            let stamps = self.resources.render_completion_stamps(pair)?;
            let visible = |stamp: u32| {
                let delta = stamp.wrapping_sub(expected);
                delta & 0xff == 0 && delta < 0x8000_0000
            };
            let ready = stamps.into_iter().all(visible);
            crate::g17_timing_buffer::record_render_gate(pair_slot, ordinal,
                if ready { 5 } else { 4 }, expected, stamps, [0; 4]);
            if crate::g17_timing_buffer::sample_tsq_gate() {
                if let Some(work) = tracker.work_state.as_ref() {
                    let before = crate::g17_timing_buffer::arch_counter();
                    let counters = work.tsq_counters();
                    let list = work.tsq_list_diagnostic();
                    let identity = slot.storage.as_deref().ok_or(ENODEV)
                        .and_then(|storage| storage.render_tsq_pointers(ordinal));
                    let mut words = [0; crate::g17_timing_buffer::WORDS];
                    words[0] = 8;
                    words[1] = u64::from(pair_slot);
                    words[2] = ordinal;
                    words[3] = u64::from(work.word());
                    words[4] = work.tsq_gpu_va().unwrap_or(0);
                    words[5] = u64::from(ready);
                    words[6] = u64::from(expected);
                    words[7] = u64::from(stamps[0]) | (u64::from(stamps[1]) << 32);
                    words[8] = before;
                    words[9] = crate::g17_timing_buffer::arch_counter();
                    match counters {
                        Ok(values) => { words[10] = u64::from(values[0]); words[11] = u64::from(values[1]); }
                        Err(error) => words[12] = error.to_errno() as i64 as u64,
                    }
                    match list {
                        Ok((head, values)) => { words[13] = head; words[14..16].copy_from_slice(&values); }
                        Err(error) => words[16] = error.to_errno() as i64 as u64,
                    }
                    match identity {
                        Ok(values) => words[17..21].copy_from_slice(&values),
                        Err(error) => words[21] = error.to_errno() as i64 as u64,
                    }
                    crate::g17_timing_buffer::record(words);
                }
            }
            if !ready { return Ok(None); }
        }
        let slot = self.render_slots.get_mut(pair_slot);
        let storage = slot.storage.take().ok_or(ENODEV)?;
        slot.preparing = true;
        Ok(Some(G17PRenderObservation {
            storage, pair_slot, ordinal, windows, tiling_previous, fragment_previous,
        }))
    }

    pub(crate) fn return_render_observation(&mut self, observation: G17PRenderObservation) {
        let slot = self.render_slots.get_mut(observation.pair_slot);
        debug_assert!(slot.preparing && slot.storage.is_none());
        debug_assert_eq!(slot.tracker.as_ref().map(|t| t.ordinal), Some(observation.ordinal));
        slot.storage = Some(observation.storage);
        slot.preparing = false;
        let pair_slot = observation.pair_slot;
        self.rekick_parked_render_grows(Some(pair_slot));
    }

    pub(crate) fn preparation_grow_pending(&self) -> bool { !self.pending_render_grows.is_empty() }

    /// v96: a borrow of `pair_slot` (or control-ring backpressure) just ended.
    /// A firmware TVB grow parked meanwhile keeps its pool's allocator paused
    /// until opcode 8, so retry it now rather than on the next firmware event
    /// (GROW_REKICK). Always counted, so a bit-off run measures the exposure.
    pub(crate) fn rekick_parked_render_grows(&self, pair_slot: Option<u8>) {
        if self.pending_render_grows.is_empty() { return; }
        let parked = crate::g17_recovery_path::GROW_PARKED_AT_RETURN
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
        let rekick = crate::g17_recovery_path::enabled(crate::g17_recovery_path::GROW_REKICK);
        if crate::g17_recovery_path::log_budget(parked, 32, 1024) {
            dev_info!(self.resources.dev().as_ref(),
                "G17P TVB grow parked #{} at borrow end (pair {:?}): {} request(s); rekick={}\n",
                parked, pair_slot, self.pending_render_grows.len(), rekick);
        }
        if rekick {
            crate::g17_recovery_path::GROW_REKICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::driver::queue_g17p_tvb_grow_worker(self.resources.owned_dev());
        }
    }

    /// v96 handshake step 6: the firmware consumed the last opcode-8 reply.
    /// Only reads the control cursors while a reply is being watched.
    pub(crate) fn observe_grow_reply_consumption(&mut self) {
        use core::sync::atomic::Ordering;
        let watch = crate::g17_recovery_path::GROW_REPLY_WATCH.load(Ordering::Acquire);
        if watch >> 63 == 0 { return; }
        let target = (watch >> 32) as u32 & 0x7fff_ffff;
        let Ok((consumer, producer)) = self.primary_control_cursors() else { return };
        if !g17p_control_ring_target_consumed(consumer, producer, target).unwrap_or(false) { return; }
        if crate::g17_recovery_path::GROW_REPLY_WATCH
            .compare_exchange(watch, 0, Ordering::AcqRel, Ordering::Relaxed).is_err() { return; }
        let consumed = crate::g17_recovery_path::GROW_REPLY_CONSUMED.fetch_add(1, Ordering::Relaxed) + 1;
        if crate::g17_recovery_path::log_budget(consumed, 64, 256) {
            let ticks = crate::g17_timing_buffer::arch_counter()
                .saturating_sub(crate::g17_recovery_path::GROW_REPLY_AT.load(Ordering::Relaxed));
            dev_info!(self.resources.dev().as_ref(),
                "G17P TVB grow ack consumed #{}: slot={} producer-after={} consumer={} after {} ticks\n",
                consumed, watch as u32, target, consumer, ticks);
        }
    }

    /// v96 recovery diagnostics: every parked/active firmware TVB grow and
    /// each allocated render pair's borrow, tracker, TVB capacity and FList
    /// lease state. Read-only; called when firmware requests a recovery.
    pub(crate) fn log_render_grow_state(&mut self, label: &str) {
        let dev = self.resources.owned_dev();
        let watch = crate::g17_recovery_path::GROW_REPLY_WATCH.load(core::sync::atomic::Ordering::Acquire);
        let unconsumed_reply: i64 = if watch >> 63 != 0 { (watch >> 32) as i64 & 0x7fff_ffff } else { -1 };
        let cursors = self.primary_control_cursors().ok();
        dev_info!(dev.as_ref(),
            "G17P grow state[{}]: control-cursors={:?} unconsumed-reply-producer={}\n",
            label, cursors, unconsumed_reply);
        dev_info!(dev.as_ref(),
            "G17P grow state[{}]: generation={} pending={} active={:?} backpressured={} tvb-reply-pending={} counters ingress={} replied={} deferred={} parked-at-return={} rekicks={}\n",
            label, self.accepted_firmware_recovery_generation, self.pending_render_grows.len(),
            self.active_render_grow, self.primary_device_control_backpressured(),
            self.primary_tvb_reply_pending(),
            crate::g17_recovery_path::GROW_INGRESS.load(core::sync::atomic::Ordering::Relaxed),
            crate::g17_recovery_path::GROW_REPLIED.load(core::sync::atomic::Ordering::Relaxed),
            crate::g17_recovery_path::GROW_DEFERRED.load(core::sync::atomic::Ordering::Relaxed),
            crate::g17_recovery_path::GROW_PARKED_AT_RETURN.load(core::sync::atomic::Ordering::Relaxed),
            crate::g17_recovery_path::GROW_REKICKS.load(core::sync::atomic::Ordering::Relaxed));
        for request in self.pending_render_grows.iter() {
            dev_info!(dev.as_ref(),
                "G17P grow state[{}]: parked slot={} context={} counter={} subpipe={} generation={} owner-preparing={}\n",
                label, request.buffer_slot, request.vm_slot, request.counter, request.subpipe,
                request.halt_count, self.render_grow_owner_preparing(*request));
        }
        for (index, slot) in self.render_slots.slots.iter().enumerate() {
            let Some(slot) = slot.as_deref() else { continue };
            dev_info!(dev.as_ref(),
                "G17P grow state[{}]: pair={} preparing={} storage={} tvb-blocks={:?} hwbuf={:?} published={} tracker={:?} queued={} ordinal={} flist={:?}\n",
                label, index, slot.preparing, slot.storage.is_some(),
                slot.storage.as_deref().map(|storage| storage.tvb_capacity()),
                slot.hardware_buffer_id, slot.hardware_buffer_published,
                slot.tracker.as_ref().map(|tracker| (tracker.hardware_buffer_id, tracker.ordinal)),
                slot.queued_tracker.iter().count(), slot.submission_ordinal,
                self.render_flist_owners[slot.flist_index].as_ref().map(|owner| (owner.sequence, owner.state,
                    owner.hardware_buffer_references, owner.pending_release_count)));
        }
        // Firmware-owned parameter-buffer state of every unborrowed pair:
        // PBDesc words and the PM HWPB head, block control and Stats (page
        // high-water). Read-only; a borrowed graph is owned by its worker.
        let slots: KVec<u8> = self.render_slots.slots.iter().enumerate()
            .filter_map(|(index, slot)| slot.as_deref()
                .filter(|slot| !slot.preparing && slot.storage.is_some())
                .map(|_| index as u8))
            .fold(KVec::new(), |mut all, index| { let _ = all.push(index, GFP_KERNEL); all });
        for index in slots {
            let hardware_buffer_id = self.render_slots.get(index).storage.as_deref()
                .and_then(g17_resources::G17PUserRenderStorage::retained_hardware_buffer_id);
            if let Some(id) = hardware_buffer_id {
                dev_info!(dev.as_ref(), "G17P grow state[{}]: pair={} hwbuf={} pbdesc={:x?}\n",
                    label, index, id, self.resources.pbdesc_words(id).ok());
            }
            if let Some(storage) = self.render_slots.get_mut(index).storage.as_deref_mut() {
                storage.log_parameter_management(&dev, label);
            }
        }
    }

    pub(crate) fn cancel_render_restage(&mut self, pair_slot: u8) -> Result {
        let slot = self.render_slots.get_mut(pair_slot);
        if let Some(reservation) = slot.restaged.take() {
            slot.storage.as_deref_mut().ok_or(EIO)?.cancel_unpublished_command(reservation.ordinal)?;
        }
        if !slot.hardware_buffer_published {
            slot.prepared_render_flist = None;
            slot.auxiliary_timestamp = 0;
            if let Some(id) = slot.hardware_buffer_id.take() {
                self.release_active_render_command_buffer_states(pair_slot)?;
                self.render_hardware_buffer_ids.release(id).map_err(|_| EIO)?;
            }
        }
        Ok(())
    }

    fn reserve_translated_render(
        &mut self, dev: &AsahiDevice, vm: &mmu::Vm,
        queue_pair: g17_resources::G17PRenderQueuePair,
        execution_context: Arc<g17_resources::G17PLogicalQueueContext>,
        work_state: Arc<g17_resources::G17PWorkStateLease>,
        command: &g17_uapi::TranslatedRenderCommand,
    ) -> Result<G17PRenderReservation> {
        let pair_slot = queue_pair.slot();
        self.ensure_render_slot(pair_slot)?;
        if self.render_slots.get(pair_slot).preparing { return Err(EBUSY); }
        if self.render_queue_pair(queue_pair.slot())? != queue_pair { return Err(EIO); }
        if let Some(leases) = self.render_channel_leases[usize::from(queue_pair.slot())] {
            // Retain on any later ambiguous failure, including registration.
            // Returning a slot needs a detach witness or processor stop.
            for lease in leases {
                if !self.channel_ids.publish(lease) { return Err(EIO); }
            }
        }
        self.ensure_render_slot(queue_pair.slot())?;
        let descriptor_context_id = execution_context.context_id() as u16;
        if !execution_context.matches(vm) || !execution_context.is_current() {
            return Err(EFAULT);
        }
        // From here on firmware-visible records name this scheduler page:
        // the queue's close must release it, and this pair keeps it mapped
        // until a later owner's context update replaces it.
        execution_context.channel_control.mark_published();
        self.render_slots.get_mut(pair_slot).channel_control = Some(execution_context.channel_control.clone());
        if self.render_slots.get(pair_slot).hardware_buffer_id.is_some()
            || self.render_slots.get(pair_slot).hardware_buffer_published
            || self.render_slots.get(pair_slot).prepared_render_flist.is_some()
            || !self.render_slots.get(pair_slot).queued_tracker.has_room()
        {
            return Err(EBUSY);
        }
        // Before anything is allocated for this submission, deal with a graph
        // whose ring producer has drifted ahead of the ordinal. Nothing this
        // function does later can recover from it, and leaving it retires the
        // client for good.
        if self.render_slots.get(pair_slot).tracker.is_none() {
            self.recycle_desynced_render_storage(pair_slot)?;
        }
        let mut host_tvb_grow = None;
        if g17_resources::g17p_render_tvb_mode() >= 2 {
            let dimension_required = g17_resources::required_render_tvb_blocks(
                &command.parameters,
                self.num_clusters,
            ).ok_or(EINVAL)?;
            // Firmware's Stats object belongs to the retained PM graph. Read
            // and reset it only after the exact TA/3D pair has retired; a
            // recovery notification is not a retirement witness. The first
            // submission has fresh Stats(reset=1) and no prior sample.
            let feedback_required = if self.render_slots.get(pair_slot).tracker.is_none()
                && self.render_slots.get(pair_slot).storage.is_some()
                && self.render_slots.get(pair_slot).submission_ordinal != 0
            {
                self.require_retired_render_owner(pair_slot)?;
                self.render_slots.get_mut(pair_slot).storage
                    .as_deref_mut()
                    .ok_or(EIO)?
                    .take_tvb_auto_grow_target()?
            } else {
                None
            };
            let required = feedback_required
                .unwrap_or(0)
                .max(dimension_required);
            if self.render_slots.get(pair_slot).storage.as_deref().is_some_and(|storage| storage.tvb_capacity()<required) {
                // The host-side pre-grow rebuilds PBDesc and needs the pair
                // idle. While a predecessor is in flight the render is
                // published at the current capacity instead of failing:
                // firmware grows the pool on demand through its type-6
                // event (`service_render_tvb_grow`), the same path
                // AGXAsyncGrowEngine takes on native.
                if self.render_slots.get(pair_slot).tracker.is_none() && !self.render_slots.get(pair_slot).queued_tracker.is_some() {
                    self.require_retired_render_owner(pair_slot)?;
                    host_tvb_grow = Some(self.resources.prepare_host_tvb_grow(
                        self.render_slots.get(pair_slot).storage.as_deref().ok_or(EIO)?, required)?);
                } else if crate::util::g17p_verbose() {
                    dev_info!(
                        dev.as_ref(),
                        "G17P render: TVB {} blocks below the {} the next scene wants; pair busy, deferring to firmware grow\n",
                        self.render_slots.get(pair_slot).storage.as_deref().map_or(0, |storage| storage.tvb_capacity()),
                        required,
                    );
                }
            }
        }
        self.render_slots.get_mut(pair_slot).auxiliary_timestamp = G17P_NATIVE_RENDER_KICK_AUXILIARY;
        // The TA HardwareBufferID names this storage's PBDesc slot and its
        // retained parameter-management graph was encoded against it, so it
        // belongs to the storage for as long as the storage lives: a retained
        // storage re-acquires its own reserved ID, and only a storage about to
        // be built takes a fresh one.
        //
        // Allocating per submission handed out whichever ID the shared LIFO had
        // on top, which is the same number only while a single client renders.
        // As soon as another slot held an ID -- in flight, or pinned by an
        // abandoned render -- the next restage here received a different ID
        // and prepare_retained_scene refused it with ENOTSUPP, which reached
        // the compositor as a lost device.
        let retained_hardware_buffer_id = self.render_slots.get(pair_slot)
            .storage
            .as_deref()
            .and_then(g17_resources::G17PUserRenderStorage::retained_hardware_buffer_id);
        let (hardware_buffer_id, parameter_buffer_token) = match retained_hardware_buffer_id {
            Some(id) => self
                .render_hardware_buffer_ids
                .acquire_reserved_with_token(id)
                .map_err(|_| EBUSY)?,
            None => self
                .render_hardware_buffer_ids
                .allocate_with_token()
                .map_err(|_| EBUSY)?,
        };
        self.render_slots.get_mut(pair_slot).hardware_buffer_id = Some(hardware_buffer_id);
        self.render_slots.get_mut(pair_slot).hardware_buffer_published = false;
        let command_buffer_state_word = match self.allocate_active_render_command_buffer_state(pair_slot, work_state) {
            Ok(word) => word,
            Err(error) => {
                self.render_slots.get_mut(pair_slot).hardware_buffer_id = None;
                self.render_hardware_buffer_ids
                    .release(hardware_buffer_id)
                    .map_err(|_| EIO)?;
                return Err(error);
            }
        };

        if crate::util::g17p_verbose() {
            dev_info!(
                dev.as_ref(),
                "G17P render: allocated TA HardwareBufferID {} for ordinal {}\n",
                hardware_buffer_id,
                self.render_slots.get(pair_slot).submission_ordinal,
            );
        }
        Ok(G17PRenderReservation {
            hardware_buffer_id, parameter_buffer_token, command_buffer_state_word,
            descriptor_context_id, execution_context, queue_pair,
            ordinal: self.render_slots.get(pair_slot).submission_ordinal, prepared_sksm: None,
            host_tvb_grow,
        })
    }

    pub(crate) fn begin_translated_render(
        &mut self,
        dev: &AsahiDevice,
        vm: &mmu::Vm,
        queue_pair: g17_resources::G17PRenderQueuePair,
        execution_context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
        work_state: Arc<g17_resources::G17PWorkStateLease>,
        command: &g17_uapi::TranslatedRenderCommand,
        render_dependencies: &[g17_submission::G17PClBarrierDependency],
        registers: &regs::Resources,
        registration_power: &mut impl FnMut(&mut Self) -> Result,
        stages: &mut crate::g17_perf::RenderStageTimes,
    ) -> Result<g17_submission::PreparedG17PPartialOpeningWorkPair> {
        let pair_slot = queue_pair.slot();
        stages.lap(crate::g17_perf::SUB_DEPS);
        let restaged = self.render_slots.get(pair_slot).restaged.is_some();
        let reservation = match self.render_slots.get_mut(pair_slot).restaged.take() {
            Some(reservation) => reservation,
            None => self.reserve_translated_render(dev, vm, queue_pair,
                execution_context, work_state, command)?,
        };
        let G17PRenderReservation { hardware_buffer_id, parameter_buffer_token,
            command_buffer_state_word, descriptor_context_id, execution_context,
            queue_pair, ordinal, prepared_sksm, host_tvb_grow } = reservation;
        let result = (|| {
        // Only legacy inline callers reach this branch. Normal DRM preparation
        // consumes and commits the grow while returning its detached graph.
        if let Some(mut grow) = host_tvb_grow {
            grow.grow(dev, self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(EIO)?, vm)?;
            self.resources.commit_host_tvb_grow(&grow)?;
        }
        if ordinal != self.render_slots.get(pair_slot).submission_ordinal {
            return Err(EIO);
        }
        // The command copy lives in the manager's preallocated heap scratch,
        // not on the publication stack (same bytes, same overrides).
        let mut scratch = match self.render_command_scratch.take() {
            Some(scratch) => scratch,
            None => KBox::new_uninit(GFP_KERNEL)?,
        };
        let result = (|| {
        let command = core::mem::MaybeUninit::write(&mut *scratch, *command);
        command.parameters.ta_hardware_buffer_id = hardware_buffer_id;
        command.parameters.context_generation = execution_context.generation();
        let command: &g17_uapi::TranslatedRenderCommand = command;
        self.begin_translated_render_with_hardware_buffer(
            dev,
            vm,
            queue_pair,
            execution_context,
            descriptor_context_id,
            command,
            render_dependencies,
            parameter_buffer_token,
            [command_buffer_state_word; 2],
            restaged,
            prepared_sksm,
            registers,
            registration_power,
            stages,
        )
        })();
        self.render_command_scratch = Some(scratch);
        result
        })();
        if result.is_err() && !self.render_slots.get(pair_slot).hardware_buffer_published {
            // acquire() stages a copy of the completed FList owner. Until
            // outer publication it holds no firmware reference and must be
            // cancelled with the HardwareBufferID reservation. Otherwise a
            // fallible PM allocation leaves every later VM handoff EBUSY.
            self.render_slots.get_mut(pair_slot).prepared_render_flist = None;
            self.render_slots.get_mut(pair_slot).auxiliary_timestamp = 0;
            self.release_active_render_command_buffer_states(pair_slot)?;
            let id = self.render_slots.get_mut(pair_slot).hardware_buffer_id.take().ok_or(EIO)?;
            self.render_hardware_buffer_ids.release(id).map_err(|_| EIO)?;
        }
        result
    }

    /// Finalize one successfully doorbelled render publication. The tracker
    /// already owns every per-command lease; clearing the staging fields here
    /// admits the next ordered group without weakening retirement.
    pub(crate) fn commit_render_publication(&mut self, pair_slot: u8) -> Result {
        let ordinal = self.render_slots.get(pair_slot).submission_ordinal;
        let tracker_matches = self.render_slots.get(pair_slot).tracker.as_ref()
            .is_some_and(|tracker| tracker.ordinal == ordinal)
            || self.render_slots.get(pair_slot).queued_tracker.iter()
                .any(|tracker| tracker.ordinal == ordinal);
        if !tracker_matches
            || !self.render_slots.get(pair_slot).hardware_buffer_published
            || self.render_slots.get(pair_slot).hardware_buffer_id.is_none()
            || self.render_command_buffer_state_pairs[pair_slot as usize].is_some()
            || self.render_slots.get(pair_slot).prepared_render_flist.is_some()
        {
            return Err(EIO);
        }
        let next_ordinal = ordinal.checked_add(1).ok_or(EOVERFLOW)?;
        self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?
            .commit_render_publication(ordinal)?;
        self.render_slots.get_mut(pair_slot).submission_ordinal = next_ordinal;
        self.render_slots.get_mut(pair_slot).hardware_buffer_id = None;
        self.render_slots.get_mut(pair_slot).hardware_buffer_published = false;
        self.render_slots.get_mut(pair_slot).auxiliary_timestamp = 0;
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn install_cold_render_storage(
        &mut self,
        dev: &AsahiDevice,
        vm: &mmu::Vm,
        queue_pair: g17_resources::G17PRenderQueuePair,
        execution_context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
        descriptor_context_id: u16,
        render_root_slot: u8,
        command: &g17_uapi::TranslatedRenderCommand,
        command_buffer_state_words: [u32; 2],
    ) -> Result {
        let pair_slot = queue_pair.slot();
        if self.render_slots.get(pair_slot).bind.is_some() {
            return Err(EIO);
        }
        let storage_box = KBox::<g17_resources::G17PUserRenderStorage>::new_uninit(GFP_KERNEL)?;
        let mut retained_sksm_backing = self.render_slots.get_mut(pair_slot)
            .sksm
            .as_mut()
            .and_then(|queues| queues.backing.take());
        let pm_generation = self.prepare_render_pm_manager_generation()?;
        let storage = self.resources.new_user_render_storage(
            storage_box,
            dev,
            vm,
            queue_pair,
            Some(execution_context.clone()),
            descriptor_context_id,
            Some(render_root_slot),
            command,
            self.num_clusters,
            command_buffer_state_words,
            self.render_slots.get(pair_slot).submission_ordinal,
            pm_generation,
            &mut retained_sksm_backing,
        );
        self.render_slots.get_mut(pair_slot).storage = Some(match storage {
            Ok(storage) if retained_sksm_backing.is_none() => storage,
            Ok(_) => return Err(Self::render_stage_fail(
                "new-user-render-sksm-backing-unclaimed", EIO,
            )),
            Err(error) => {
                if let Some(backing) = retained_sksm_backing.take() {
                    let queues = self.render_slots.get_mut(pair_slot).sksm.as_mut().ok_or(EIO)?;
                    if queues.backing.replace(backing).is_some() {
                        return Err(EIO);
                    }
                }
                return Err(Self::render_stage_fail("new-user-render-storage", error));
            }
        });
        self.commit_render_pm_manager_generation(pair_slot, pm_generation)
            .map_err(|error| Self::render_stage_fail("commit-pm-manager-generation", error))?;
        // The fresh graph was encoded against this ID; keep the ID off the
        // shared free list for as long as the storage lives.
        if let Some(id) = self.render_slots.get(pair_slot)
            .storage
            .as_deref()
            .and_then(g17_resources::G17PUserRenderStorage::retained_hardware_buffer_id)
        {
            self.render_hardware_buffer_ids
                .reserve_with_owner_ref(id, crate::g17_host_path::enabled(crate::g17_host_path::PB_OWNER_REF))
                .map_err(|_| Self::render_stage_fail("reserve-hardware-buffer-id", EIO))?;
        }
        if self.render_slots.get(pair_slot).submission_ordinal != 0 {
            let parameter_page_list_vas = self.render_slots.get(pair_slot)
                .storage
                .as_deref()
                .ok_or(ENODEV)?
                .parameter_buffer_page_list_vas()?;
            let slot = self.render_slots.get_mut(pair_slot);
            let storage = slot.storage.as_deref_mut().ok_or(ENODEV)?;
            storage
                .rebase_empty_queue_cursors(slot.submission_ordinal)
                .map_err(|error| Self::render_stage_fail("rebase-new-vm-queue", error))?;
            storage.restage_command(
                    dev,
                    vm,
                    descriptor_context_id,
                    parameter_page_list_vas,
                    command,
                    self.num_clusters,
                    command_buffer_state_words,
                    slot.submission_ordinal,
                    true,
                )
                .map_err(|error| Self::render_stage_fail("restage-new-vm-command", error))?;
        }
        // `new_user_render_storage` has just stamped the device-global PB
        // loader from this fresh graph, so record it as the owner without
        // repeating the write.
        self.render_parameter_buffer_owner =
            self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?.render_pool_id();
        Ok(())
    }

    fn begin_translated_render_with_hardware_buffer(
        &mut self,
        dev: &AsahiDevice,
        vm: &mmu::Vm,
        queue_pair: g17_resources::G17PRenderQueuePair,
        execution_context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
        descriptor_context_id: u16,
        command: &g17_uapi::TranslatedRenderCommand,
        render_dependencies: &[g17_submission::G17PClBarrierDependency],
        parameter_buffer_token: u64,
        command_buffer_state_words: [u32; 2],
        restaged: bool,
        prepared_sksm: Option<G17PRenderPreparedSksm>,
        registers: &regs::Resources,
        registration_power: &mut impl FnMut(&mut Self) -> Result,
        stages: &mut crate::g17_perf::RenderStageTimes,
    ) -> Result<g17_submission::PreparedG17PPartialOpeningWorkPair> {
        let pair_slot = queue_pair.slot();
        let shared_work_state = self.render_command_buffer_state_pairs[
            pair_slot as usize].as_ref().cloned().ok_or(EIO)?;
        if command_buffer_state_words != [shared_work_state.word(); 2] { return Err(EINVAL); }
        let render_sksm = crate::g17_config::RENDER_SKSM;
        let native_doorbell_mode =
            crate::g17_config::RENDER_NATIVE_DOORBELLS;
        let split_launch = crate::g17_config::RENDER_SPLIT_LAUNCH != 0;
        if native_doorbell_mode > 3
            || (native_doorbell_mode != 0 && split_launch)
            || (g17p_native_fragment_first_mode(native_doorbell_mode) && render_sksm == 0)
        {
            return Err(EINVAL);
        }
        // Reserve tracker capacity before staging either engine. Their exact
        // item targets are selected after scratch and resource updates below;
        // ConfigUpdate may be omitted independently on either lane.
        if !self.render_slots.get(pair_slot).queued_tracker.has_room() {
            return Err(EBUSY);
        }
        // One asynchronous grow may remain outstanding while this retained
        // owner accepts more commands. acquire_owner retains its exact tuple
        // and adds command references without issuing another grow request.
        self.render_slots.get_mut(pair_slot).prepared_render_flist = None;
        self.resources
            .stage_render_region_views()
            .map_err(|error| Self::render_stage_fail("stage-region-views", error))?;
        // The render storage owns the complete SecureGart low-view closure in
        // its application-GART root.  User work must never repoint the global
        // context-0 lower VM; that singleton was the fundamental cross-client
        // exclusion and made a second independently rooted render impossible.
        if self.render_slots.get(pair_slot).context_aliases.is_some() {
            return Err(Self::render_stage_fail("legacy-context0-alias-live", EBUSY));
        }
        let retained = self.render_slots.get(pair_slot).storage.is_some();
        if stages.enabled {
            stages.retained = retained;
        }
        // Context 1 is the stable firmware-service root. Each retained render
        // owner maps a distinct FList backing into it, while the queue's
        // independently allocated application-GART context owns command data
        // and is the MCache selector. Taking a second VmBind here both
        // overwrote that service root and imposed the artificial four-client
        // ceiling.
        let render_root_slot = if retained {
            self.render_slots.get(pair_slot).bind
                .as_ref()
                .ok_or_else(|| Self::render_stage_fail("retained-render-root-missing", EBUSY))?
                .root_slot()
                .map_err(|error| Self::render_stage_fail("render-root-slot", error))?
        } else {
            u8::try_from(execution_context.context_id())
            .map_err(|_| Self::render_stage_fail("render-root-slot-width", EIO))?
        };
        // Which construction path a render takes has never been logged, and a
        // cold boot was measured taking the RESTAGE branch on what should be a
        // first render (93cc292: neither log line inside new_user_render_storage
        // appears cold, so the UMA page-pool descriptor is never staged, while
        // the tiler parameter-management graph that faults binds that pool).
        // Name the branch and its inputs so the two readings -- genuinely
        // retained, versus construction attempted and abandoned -- separate in
        // one run.
        if crate::util::g17p_verbose() {
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P render storage path: retained={} ordinal={} bind={} tracker={} root-slot={}\n",
                retained,
                self.render_slots.get(pair_slot).submission_ordinal,
                self.render_slots.get(pair_slot).bind.is_some(),
                self.render_slots.get(pair_slot).tracker.is_some(),
                render_root_slot,
            );
        }
        if retained
            && !self.render_slots.get(pair_slot)
                .bind
                .as_ref()
                .is_some_and(|binding| binding.matches(vm))
        {
            return Err(EBUSY);
        }
        if retained {
            // The retained descriptors already encode the bound context as
            // their MCache write-range selector. If those two ever disagreed,
            // this client's fragment write ranges would be resolved against
            // another client's page tables, so refuse instead of restaging.
            if self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?.render_root_slot()
                != render_root_slot
            {
                return Err(Self::render_stage_fail("retained-root-slot-moved", EBUSY));
            }
            if !restaged {
            let parameter_page_list_vas = self.render_slots.get(pair_slot)
                .storage
                .as_deref()
                .ok_or(ENODEV)?
                .parameter_buffer_page_list_vas()?;
            stages.lap(crate::g17_perf::SUB_BEGIN);
            let restage_started = stages.enabled.then(
                kernel::time::Instant::<kernel::time::Monotonic>::now);
            let slot = self.render_slots.get_mut(pair_slot);
            slot.storage
                .as_deref_mut()
                .ok_or(ENODEV)?
                .restage_command(
                    dev,
                    vm,
                    descriptor_context_id,
                    parameter_page_list_vas,
                    command,
                    self.num_clusters,
                    command_buffer_state_words,
                    slot.submission_ordinal,
                    true,
                )
                .map_err(|error| Self::render_stage_fail("restage-command", error))?;
            if let Some(started) = restage_started {
                stages.restage_ns = started.elapsed().as_nanos().max(0) as u64;
            }
            stages.skip();
            }
            // The retained PM owner validates its unchanged PBDesc identity.
            // Firmware owns the live page-list/PBDesc cursors; do not reset
            // them to cold values when preparing another Scene. The cold
            // new_user_render_storage path below still initializes them.
            //
            // Each owner has its own HardwareBufferID-indexed PBDesc. Check
            // that identity when switching owners without rewriting firmware's
            // live counters or flags with constructor defaults.
            self.retarget_render_parameter_buffer(pair_slot)
                .map_err(|error| Self::render_stage_fail("retarget-parameter-buffer", error))?;
        } else {
            // Cold path: no storage on this pair at publication. Kept out of
            // line so its G17PUserRenderStorage temporary (~2.8 KiB) is not
            // part of the hot publication frame.
            self.install_cold_render_storage(dev, vm, queue_pair, &execution_context,
                descriptor_context_id, render_root_slot, command, command_buffer_state_words)?;
        }
        if !restaged {
            self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?
                .bind_shared_work_state(&shared_work_state)?;
        }
        // A fresh storage and its application-GART root are one reusable
        // graph. Install that ownership before the fallible direct secondary
        // pair registration. If power disappears before the transaction, the
        // outer pair is still unpublished, but the next submit can safely
        // reuse this complete graph instead of finding storage without its
        // root and returning EBUSY forever.
        if !retained {
            self.render_slots.get_mut(pair_slot).bind = Some(G17PRenderRootOwner::Job {
                context: execution_context.clone(),
            });
        }
        if crate::g17_config::RENDER_USC_FREELIST != 0 {
            let id = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?.render_flist_id() as usize;
            if id >= self.render_flist_owners.len() { return Err(EINVAL); }
            if self.render_slots.get(pair_slot).flist_index != id
                && self.render_slots.get(pair_slot).flist_leases.is_some() { return Err(EBUSY); }
            self.render_slots.get_mut(pair_slot).flist_index = id;
            let storage = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?;
            // A context's new pool can recycle the previous owner's FList ID
            // and every VA in the tuple. It is still a new owner: its image
            // and UMA descriptor row must be staged (native
            // populateInitialFirmwareState + halLoadUMADesc), never the
            // retained row whose cursors firmware advanced for the old pool.
            let owner_matches = self.render_flist_owners[self.render_slots.get(pair_slot).flist_index].as_ref().is_some_and(|owner| {
                owner.matches_owner(
                    storage.render_flist_id(),
                    storage.render_flist_pointer(),
                    storage.render_flist_operand_table(),
                    storage.render_flist_cookie(),
                ) && owner.same_pool_object(storage.render_flist_serial())
            });
            // Validate the lifecycle before repopulating memory or replacing
            // the global UMA descriptor. A rejected owner/sequence must leave
            // the preceding owner's published image untouched.
            let mut prepared = G17PBootstrapUmaLifecycle::acquire_owner(
                self.render_flist_owners[self.render_slots.get(pair_slot).flist_index].as_ref(),
                self.render_flist_owners[self.render_slots.get(pair_slot).flist_index].as_ref().map_or(Ok(G17P_BOOTSTRAP_UMA_SEQUENCE), |owner| owner.sequence.checked_add(1).ok_or(EOVERFLOW))?,
                storage.render_flist_id(),
                storage.render_flist_pointer(),
                storage.render_flist_operand_table(),
                storage.render_flist_cookie(),
                storage.render_flist_needs_grow(),
            ).map_err(|_| EBUSY)?;
            prepared.pool_serial = storage.render_flist_serial();
            if !owner_matches {
                if self.render_flist_owners[id].as_ref().is_some_and(|old|
                    old.state != G17PBootstrapUmaRequestState::Completed
                        || old.hardware_buffer_owned || old.has_release_witness()) { return Err(EBUSY); }
                // A different pool takes over this pair's FList ID. The
                // preflight above validates the predecessor's release state;
                // a sent release witness may still be carried by `prepared`.
                // Initialize only the incoming owner's image here.
                if let Ok((id, [submitted, completed])) = storage.render_flist_takeover_counters() {
                    let takeovers = G17P_FLIST_TAKEOVERS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
                    let outstanding = submitted != completed;
                    if outstanding || crate::g17_recovery_path::log_budget(takeovers, 16, 256) {
                        let note = if outstanding { " OUTSTANDING" } else { "" };
                        let row = self.resources.uma_row_counters(u32::from(id));
                        dev_info!(self.resources.dev().as_ref(),
                            "G17P FList takeover #{}: pair={} id={} submitted={} completed={} previous-uma-row(a,b,total)={:x?}{}\n",
                            takeovers, pair_slot, id, submitted, completed, row, note);
                    }
                }
                storage.repopulate_render_flist()
                    .map_err(|error| Self::render_stage_fail("repopulate-render-flist", error))?;
                self.resources.stage_user_render_uma(storage)
                    .map_err(|error| Self::render_stage_fail("stage-render-uma-owner", error))?;
            }
            self.render_slots.get_mut(pair_slot).prepared_render_flist = Some(prepared);
        }
        let mut mapping_proof_reused = false;
        {
            let slot = self.render_slots.get_mut(pair_slot);
            let storage = slot.storage.as_deref_mut().ok_or(ENODEV)?;
            storage
                .set_parameter_buffer_lease_token(parameter_buffer_token)
                .map_err(|error| Self::render_stage_fail("set-pb-lease-token", error))?;
            if g17p_render_slot_globals() & G17P_RENDER_SLOT_GLOBAL_FLIST_GENERATION != 0 {
                // tag-15 +0x3e is the USC FList lifecycle generation. Publish
                // the sequence acquired for this retained owner, not its
                // submit ordinal. Independent slots carry independent
                // sequences with their FList state.
                storage.set_flist_generation(slot.prepared_render_flist.map_or(
                    g17_submission::g17p_render_flist_generation(slot.submission_ordinal),
                    |lifecycle| lifecycle.sequence,
                ));
            }
            stages.lap(crate::g17_perf::SUB_FLIST);
            let reachability_started = stages.enabled.then(
                kernel::time::Instant::<kernel::time::Monotonic>::now);
            let unreachable = storage.first_unreachable_cached(self.resources.uat(), vm);
            mapping_proof_reused = storage.last_reachability_cache_hit();
            if let Some(started) = reachability_started {
                stages.reachability_ns = stages.reachability_ns.saturating_add(
                    started.elapsed().as_nanos().max(0) as u64);
                stages.reachability_cache_hit = storage.last_reachability_cache_hit();
                stages.tvb_blocks = u32::try_from(storage.tvb_capacity()).unwrap_or(u32::MAX);
            }
            stages.skip();
            if let Some(failed) = unreachable {
                storage.log_reachability(dev, self.resources.uat(), vm);
                dev_err!(
                    dev.as_ref(),
                    "G17P render: storage unreachable, first failing predicate is {}\n",
                    failed
                );
                return Err(EFAULT);
            }
        }
        if render_sksm != 0 && self.render_slots.get(pair_slot).sksm.is_none() {
            let (pair, hardware_buffer_id, entry_vas) = {
                let storage = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?;
                (
                    storage.queue_pair(),
                    u8::try_from(storage.qos_hardware_buffer_id()?)
                        .map_err(|_| EOVERFLOW)?,
                    [
                        (
                            storage.sksm_entry_low_va(true),
                            storage.sksm_entry_high_va(true),
                        ),
                        (
                            storage.sksm_entry_low_va(false),
                            storage.sksm_entry_high_va(false),
                        ),
                    ],
                )
            };
            self.render_slots.get_mut(pair_slot).sksm = Some(
                self.new_render_sksm_queues(pair, hardware_buffer_id, entry_vas)
                    .map_err(|error| Self::render_stage_fail("new-render-sksm-queues", error))?,
            );
        }
        // A populated firmware recovery may preserve the host graph. Its
        // configuration cache must nevertheless force the next full update.
        let recovery_generation = self.accepted_firmware_recovery_generation;
        self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?
            .update_render_recovery_generation(recovery_generation)?;
        let (fragment_queue, tiling_queue, previous, late_tiling, fragment_header) = if render_sksm != 0 {
            self.stage_render_groups_with_sksm(pair_slot,
                dev,
                render_dependencies,
                prepared_sksm,
            )
                .map_err(|error| Self::render_stage_fail("stage-render-groups-sksm", error))?
        } else {
            let slot = self.render_slots.get_mut(pair_slot);
            let storage = slot.storage.as_deref_mut().ok_or(ENODEV)?;
            storage
                .stage_fragment(slot.submission_ordinal)
                .inspect_err(|error| {
                    dev_err!(dev.as_ref(), "G17P render: fragment stage failed ({:?})\n", error);
                })?;
            let previous = storage.admission_queue_indices().inspect_err(|error| {
                dev_err!(dev.as_ref(), "G17P render: initial snapshot failed ({:?})\n", error);
            })?;
            let late_tiling = storage
                .prepare_late_tiling_publication(slot.submission_ordinal)
                .inspect_err(|error| {
                    dev_err!(dev.as_ref(), "G17P render: tiling stage failed ({:?})\n", error);
                })?;
            let (fragment_queue, tiling_queue) = storage.queue_gpu_vas();
            let fragment_header = storage.fragment_config_update_header()?;
            (fragment_queue, tiling_queue, previous, Some(late_tiling), fragment_header)
        };
        // The actual per-engine groups may independently omit ConfigUpdate.
        // Retain their exact cursors before exposing either outer producer.
        let windows = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?
            .current_render_item_windows(self.render_slots.get(pair_slot).submission_ordinal)?;
        let published_prefixes = windows.map(|window| window.target());
        if stages.enabled {
            stages.config_present = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?
                .current_render_config_present(self.render_slots.get(pair_slot).submission_ordinal)?;
            stages.item_targets = published_prefixes;
            stages.item_plan_valid = true;
        }
        // Headers describe each lane's effective configuration, whether it
        // needs a new optional record or retains the already published state.
        // TA likewise owns its late-publication fields. Legacy diagnostics
        // retain their graph readback. No cross-generation header cache.
        stages.lap(crate::g17_perf::SUB_SKSM);
        let tiling_header = late_tiling
            .as_ref()
            .and_then(|publication| publication.config_update_header());
        let (fragment_install, tiling_install) = if render_sksm != 0 {
            let fragment = self
                .validate_render_firmware_install(pair_slot, false, fragment_header.install)
                .map_err(|error| {
                    Self::render_stage_fail("validate-fragment-tag15-install", error)
                })?;
            let tiling = tiling_header
                .map(|header| {
                    self.validate_render_firmware_install(pair_slot, true, header.install).map_err(|error| {
                        Self::render_stage_fail("validate-tiling-tag15-install", error)
                    })
                })
                .transpose()?;
            (fragment, tiling)
        } else {
            (false, None)
        };
        if crate::util::g17p_verbose() {
            dev_info!(
                dev.as_ref(),
                "G17P render tag15: 3D selector {:#x} qid {} install {} dm {} gfx {} ctx {} flist={:#x}/id{}/generation{:#x} scheduler-state {:#x} +0x56 {} entry {:#x}/{:#x} pb={:#x}/{:#x}/id{}/bit{}\n",
                fragment_header.selector,
                fragment_header.queue_id,
                fragment_header.install,
                fragment_header.data_master,
                fragment_header.scratch_seed_valid,
                fragment_header.context_id,
                fragment_header.flist_pointer,
                fragment_header.flist_id,
                fragment_header.flist_generation,
                fragment_header.scheduler_state,
                fragment_header.field_56,
                fragment_header.entry_low,
                fragment_header.entry_high,
                fragment_header.parameter_buffer_object,
                fragment_header.parameter_buffer_token,
                fragment_header.parameter_buffer_id,
                fragment_header.parameter_buffer_bit,
            );
        }
        if let Some(header) = tiling_header {
            if crate::util::g17p_verbose() {
                dev_info!(
                    dev.as_ref(),
                    "G17P render tag15: TA selector {:#x} qid {} install {} dm {} gfx {} ctx {} flist={:#x}/id{}/generation{:#x} scheduler-state {:#x} +0x56 {} entry {:#x}/{:#x} pb={:#x}/{:#x}/id{}/bit{}\n",
                    header.selector,
                    header.queue_id,
                    header.install,
                    header.data_master,
                    header.scratch_seed_valid,
                    header.context_id,
                    header.flist_pointer,
                    header.flist_id,
                    header.flist_generation,
                    header.scheduler_state,
                    header.field_56,
                    header.entry_low,
                    header.entry_high,
                    header.parameter_buffer_object,
                    header.parameter_buffer_token,
                    header.parameter_buffer_id,
                    header.parameter_buffer_bit,
                );
            }
        }

        // Native calls AGXQOSQueue::willSubmit before making the tag-15 item
        // visible. Populate the same QID-indexed firmware resource first. In
        // native fragment-first modes TA does not exist yet; its own record is
        // published by `publish_native_render_tiling_group` immediately
        // before that routine exposes the delayed TA tag-15 prefix.
        let mut qos_publications = [None; 2];
        // v91 fine laps (e2e option bit4) for SUB_CONFIG; 0 when disabled.
        let mut config_fine = [crate::g17_e2e::fine(), 0, 0, 0, 0];
        let qos_result = (|| {
            let fragment_qos = self
                .resources
                .publish_render_qos_queue_record(fragment_header)
                .map_err(|error| Self::render_stage_fail("publish-fragment-qos", error))?;
            if config_fine[0] != 0 { config_fine[1] = crate::g17_e2e::now(); }
            qos_publications[0] = Some(fragment_qos);
            if render_sksm != 0 {
                self.validate_render_queue_consistency(pair_slot, dev, false, fragment_header, fragment_qos.record,
                    prepared_sksm.map(|prepared| prepared.descriptor_contexts))
                    .map_err(|error| {
                        Self::render_stage_fail("validate-fragment-qid-consistency", error)
                    })?;
            }
            if config_fine[0] != 0 { config_fine[2] = crate::g17_e2e::now(); }
            if crate::util::g17p_verbose() {
                dev_info!(
                    dev.as_ref(),
                    "G17P render QoS: 3D qid {} record {:?}\n",
                    fragment_header.queue_id,
                    fragment_qos.record,
                );
            }
            if let Some(header) = tiling_header {
                let tiling_qos = self
                    .resources
                    .publish_render_qos_queue_record(header)
                    .map_err(|error| Self::render_stage_fail("publish-tiling-qos", error))?;
                qos_publications[1] = Some(tiling_qos);
                if config_fine[0] != 0 { config_fine[3] = crate::g17_e2e::now(); }
                if render_sksm != 0 {
                    self.validate_render_queue_consistency(pair_slot, dev, true, header, tiling_qos.record,
                        prepared_sksm.map(|prepared| prepared.descriptor_contexts))
                        .map_err(|error| {
                            Self::render_stage_fail("validate-tiling-qid-consistency", error)
                        })?;
                }
                if config_fine[0] != 0 { config_fine[4] = crate::g17_e2e::now(); }
                if crate::util::g17p_verbose() {
                    dev_info!(
                        dev.as_ref(),
                        "G17P render QoS: TA qid {} record {:?}\n",
                        header.queue_id,
                        tiling_qos.record,
                    );
                }
            }
            // Construction and validation above are CPU-only and can take several
            // milliseconds on a dockchannel console.  Acquiring the powered KSMFE
            // aperture before that work left a timing race: firmware could enter
            // idle after the final power witness but before these four writes.
            // Worse, an EAGAIN here returned after the command/SKSM producers had
            // already been staged, so the userspace retry encountered a half-built
            // ordinal-zero graph and poisoned this physical pair for every later
            // client.  Native ensurePoweredHardware(true) brackets the actual
            // configureHardware transaction.  Do the same: reacquire immediately
            // at the transaction boundary and retry only the still-unpublished
            // four-write registration, without restaging the command.
            // Log the install decision for every FIRST install (a few dozen per
            // boot) and, under g17p_verbose, for rebinds too. A per-publication
            // printk on the steady-state path costs about 16% at depth 2 on a
            // console-backed log (measured on boot d5250d86: 132k REBIND lines).
            let routes_configured = self.render_slots.get(pair_slot).sksm.as_ref()
                .is_some_and(|queues| queues.tiling.hardware_enabled && queues.fragment.hardware_enabled);
            let first_kick_reconfigure =
                module_parameters::g17p_render_first_kick_reconfigure.value() != 0;
            let first_install = fragment_install || tiling_install.unwrap_or(false);
            if first_install || crate::util::g17p_verbose() {
            dev_info!(
                dev.as_ref(),
                "G17P render install decision: pair {}/{} slot {} context {} ordinal {}: {} (fragment install={} tiling install={:?}, routes_configured={}, first_kick_reconfigure={})\n",
                queue_pair.tiling,
                queue_pair.fragment,
                pair_slot,
                descriptor_context_id,
                self.render_slots.get(pair_slot).submission_ordinal,
                if first_install { "FIRST-INSTALL tag-15 +0x1a=1" } else { "REBIND tag-15 +0x1a=0" },
                fragment_header.install,
                tiling_header.map(|header| header.install),
                routes_configured,
                first_kick_reconfigure,
            );
            }
            if queue_pair.slot() != 0 && first_install && routes_configured && !first_kick_reconfigure {
                // Native count: configureHardware runs once per queue. The
                // warm-up transaction already enabled both routes; skip the
                // repeat and its IdlePowerOff(0)/(1) bracket. The firmware's
                // tag-15 install (+0x1a=1) still reprograms the queue.
                dev_info!(
                    dev.as_ref(),
                    "G17P render: pair {}/{} first kick keeps the warm-up KSMFE routes (no host reconfigure, no power bracket)\n",
                    queue_pair.tiling,
                    queue_pair.fragment,
                );
            } else if queue_pair.slot() != 0 && first_install {
                crate::g17_stackwatch::FIRST_INSTALL_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                // Power guarantee modes (see `g17p_render_registration_power`):
                // 1 production IdlePowerOff bracket; 0 native shape (no
                // device-control record, aperture must already read powered);
                // 2 bracket only when it reads unpowered.
                let power_mode = module_parameters::g17p_render_registration_power.value();
                let mut configured = false;
                let mut bracketed = false;
                for attempt in 0..3 {
                    let bracket = match power_mode {
                        0 => false,
                        2 => !registers.g17p_sksm_configuration_port_powered(),
                        _ => true,
                    };
                    if bracket {
                        bracketed = true;
                        crate::g17_stackwatch::BRACKET_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        registration_power(self)
                            .map_err(|error| Self::render_stage_fail(
                                "power-secondary-render-pair", error,
                            ))?;
                    }
                    match self.configure_fresh_nonbootstrap_render_pair(pair_slot,
                        registers,
                        fragment_install,
                        tiling_install,
                    ) {
                        Ok(()) => {
                            configured = true;
                            dev_info!(
                                dev.as_ref(),
                                "G17P render: pair {}/{} first-kick KSMFE configure done (power mode {}, bracket used={}, attempt {})\n",
                                queue_pair.tiling, queue_pair.fragment, power_mode, bracketed, attempt + 1,
                            );
                            break;
                        }
                        Err(error) if error == EAGAIN => {
                            dev_info!(
                                dev.as_ref(),
                                "G17P render: pair {}/{} first-kick KSMFE configure: aperture unpowered (power mode {}, attempt {})\n",
                                queue_pair.tiling, queue_pair.fragment, power_mode, attempt + 1,
                            );
                            continue;
                        }
                        Err(error) => return Err(Self::render_stage_fail(
                            "configure-secondary-render-pair", error,
                        )),
                    }
                }
                if !configured {
                    return Err(Self::render_stage_fail(
                        "configure-secondary-render-pair", EAGAIN,
                    ));
                }
            } else {
                self.configure_fresh_nonbootstrap_render_pair(pair_slot,
                    registers,
                    fragment_install,
                    tiling_install,
                )
                .map_err(|error| Self::render_stage_fail(
                    "configure-secondary-render-pair", error,
                ))?;
            }
            if config_fine[0] != 0 {
                crate::g17_e2e::record(crate::g17_e2e::K_FINE_CONFIG, 0,
                    u64::from(pair_slot) << 32 | self.render_slots.get(pair_slot).submission_ordinal,
                    config_fine);
            }
            stages.lap(crate::g17_perf::SUB_CONFIG);
            let slot = self.render_slots.get_mut(pair_slot);
            let submitted_kicks =
                if g17p_native_fragment_first_mode(native_doorbell_mode) { 1 } else { 2 };
            let (pm_submission, flist_submission) = slot.storage.as_deref_mut().ok_or(ENODEV)?
                .prepare_render_submitted_operations(submitted_kicks)
                .map_err(|error| Self::render_stage_fail("prepare-pm-submitted-operation", error))?;
            let pm_submitted_count = pm_submission.as_ref().map(|operation| operation.next());
            // The TA/3D descriptors execute in their independently allocated
            // application context, and this VM's own TTBAT context was bound at
            // the top of this function -- both before anything here is visible to
            // firmware. Publication itself no longer touches the context table.
            //
            // The outer ring entry's "channel started" byte (`!chan[0x3c]` in
            // native's encodeAcceleratorRingCommand) belongs to the selected
            // AGXChannel. A fresh QID2/3 pair therefore publishes `1` even after
            // QID0/1 has run, while repeats on either pair publish `0`.
            let first_outer_submit =
                if g17p_render_slot_globals() & G17P_RENDER_SLOT_GLOBAL_CHANNEL_STARTED != 0 {
                    g17p_render_pair_first_outer_submit(
                        &self.render_outer_channels_started,
                        queue_pair.slot(),
                    )
                    .ok_or(EINVAL)?
                } else {
                    slot.submission_ordinal == 0
                };
            let prepared = self
                .resources
                .publish_partial_opening_pair(
                    true,
                    queue_pair,
                    g17p_render_deferred_outer_mode(native_doorbell_mode, split_launch),
                    fragment_queue,
                    published_prefixes[1],
                    tiling_queue,
                    published_prefixes[0],
                    first_outer_submit,
                    move || {
                        // Count just this publication, never all queued renders.
                        // A native fragment-first diagnostic publishes TA later.
                        shared_work_state.add_submitted_kicks(submitted_kicks)?;
                        flist_submission.commit();
                        if let Some(operation) = pm_submission {
                            operation.commit();
                        }
                        Ok(())
                    },
                    move || {
                        if let Some(publication) = late_tiling {
                            publication.publish();
                        }
                    },
                )
                .map_err(|error| Self::render_stage_fail("publish-partial-opening-pair", error))?;
            Ok((prepared, pm_submitted_count))
        })();
        let (prepared, pm_submitted_count) = match qos_result {
            Ok(published) => published,
            Err(error) => {
                // publish_partial_opening_pair acquires all mappings/checks
                // before either producer store and cannot fail afterwards.
                for ticket in qos_publications.into_iter().rev().flatten() {
                    self.resources.cancel_qos_publication(ticket)?;
                }
                return Err(error);
            }
        };
        self.commit_render_firmware_install(pair_slot, false, fragment_install);
        if let Some(install) = tiling_install {
            self.commit_render_firmware_install(pair_slot, true, install);
        }
        stages.lap(crate::g17_perf::SUB_OPENING);
        if crate::util::g17p_verbose() {
            dev_info!(dev.as_ref(), "G17P render PM: submitted-operations committed {:?}\n", pm_submitted_count);
        }
        // The ID is now firmware/GPU-owned. Any later validation failure must
        // retain it until semantic completion or a stopped-CPU teardown.
        self.render_slots.get_mut(pair_slot).hardware_buffer_published = true;
        self.render_outer_channels_started[usize::from(queue_pair.slot())] = true;
        if crate::util::g17p_verbose() {
            pr_info!(
                "G17P render: outer publish OK, retained={} root-slot={}\n",
                retained,
                render_root_slot,
            );
        }
        if crate::util::g17p_verbose() {
            let roots = self.resources.uat().t8140_compute_context_roots();
            dev_info!(
                dev.as_ref(),
                "G17P render context roots pre-kick: ctx0=[{:#x},{:#x}] ctx1=[{:#x},{:#x}] ctx2=[{:#x},{:#x}] ctx3=[{:#x},{:#x}]\n",
                roots[0].0,
                roots[0].1,
                roots[1].0,
                roots[1].1,
                roots[2].0,
                roots[2].1,
                roots[3].0,
                roots[3].1,
            );
        }
        // Prove the client regime reaches native's low view 0 before anything
        // is armed. gUPM's first fragment traffic writes 0x1e00..0x1f80
        // through this VM's own slot, and an unmapped write there is exactly
        // the fault this view exists to remove; a wrong or missing leaf must
        // fail here, on the host, rather than as another opaque dIPP miss.
        // The null-view probe and the RCE readback below are mapping
        // validations. When the retained graph's reachability proof was reused
        // for this generation (same VM and kernel mapping epochs), neither can
        // observe anything new, and each costs page-table walks under VM exec
        // locks on every render.
        for offset in [0x1e00u64, 0x1f80u64].into_iter().filter(|_| !mapping_proof_reused) {
            let (canonical, client) = vm
                .t8140_client_null_view_probe(offset)
                .map_err(|error| {
                    dev_err!(
                        dev.as_ref(),
                        "G17P render client null view: +{:#x} does not translate ({:?})\n",
                        offset,
                        error,
                    );
                    Self::render_stage_fail("client-null-view-unmapped", error)
                })?;
            if canonical != client {
                dev_err!(
                    dev.as_ref(),
                    "G17P render client null view: +{:#x} client PA {:#x} != canonical PA {:#x}\n",
                    offset,
                    client,
                    canonical,
                );
                return Err(Self::render_stage_fail("client-null-view-mismatch", EFAULT));
            }
            if crate::util::g17p_verbose() {
                dev_info!(
                    dev.as_ref(),
                    "G17P render client null view: +{:#x} -> PA {:#x}, identical to canonical {:#x}+{:#x}\n",
                    offset,
                    client,
                    0x0010_0000_0000u64,
                    offset,
                );
            }
        }
        if !mapping_proof_reused {
            self.render_slots.get_mut(pair_slot).storage
                .as_deref_mut()
                .ok_or(ENODEV)?
                .audit_fragment_rce_mapping(dev, vm)
                .map_err(|error| Self::render_stage_fail("fragment-rce-vm-audit", error))?;
        }
        if g17p_native_fragment_first_mode(native_doorbell_mode) {
            fence(Ordering::Acquire);
            let slot = self.render_slots.get_mut(pair_slot);
            let current = slot.storage.as_deref_mut().ok_or(ENODEV)?
                .admission_queue_indices()?;
            let outer = self.resources.partial_opening_outer_counters()?;
            let fragment_sksm_visible = self.render_slots.get(pair_slot)
                .sksm
                .as_ref()
                .is_some_and(|queues| queues.fragment.producer.previous_valid);
            if !fragment_sksm_visible
                || !g17p_native_fragment_only_visible(
                    previous.tiling.write,
                    current.tiling.write,
                    current.fragment.write,
                    published_prefixes[1],
                    outer[0][2],
                    prepared.fragment.next_producer,
                    outer[1][2],
                    prepared.tiling.slot_index,
                )
            {
                return Err(Self::render_stage_fail(
                    "native-fragment-only-visibility",
                    EINVAL,
                ));
            }
        }
        // A physical snapshot is a valid anchor only for the first in-flight
        // group.  While another group is active, TA and 3D can expose mixed
        // cursors from that predecessor (for example 3/6/9).  Anchoring the
        // queued tracker there makes its own modular completion window reject
        // the stale cursor forever.  Its immutable predecessor is instead the
        // active group's exact published target, including across ring wrap.
        let (tiling_previous, fragment_previous) =
            if let Some(active) = self.render_slots.get(pair_slot).tracker.as_ref() {
                let retired = |target| g17_completion::QueueIndices {
                    done: target, read: target, write: target,
                };
                (retired(active.windows[0].target()), retired(active.windows[1].target()))
            } else {
                (previous.tiling, previous.fragment)
            };
        let hardware_buffer_id = self.render_slots.get(pair_slot).hardware_buffer_id.ok_or(EIO)?;
        let storage = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?;
        let pool_a_ticket = storage.current_pool_a_ticket();
        let context_update = storage.context_update_pending();
        let queues = self.render_slots.get(pair_slot).sksm.as_ref().ok_or(EIO)?;
        let native_timestamps = Some([
            queues.tiling.producer.previous_timestamp,
            queues.fragment.producer.previous_timestamp,
        ]);
        let work_state = self.take_active_render_command_buffer_state(pair_slot)?;
        let command_buffer_state_word = work_state.word();
        let tracker = G17PPartialOpeningRenderTracker {
            ordinal: self.render_slots.get(pair_slot).submission_ordinal,
            hardware_buffer_id,
            command_buffer_state_word,
            work_state: Some(work_state),
            native_timestamps,
            qos_pending: true,
            pool_a_record_index: pool_a_ticket.0,
            pool_a_submitted: pool_a_ticket.1,
            context_update,
            tiling_previous,
            fragment_previous,
            tiling_prefix: published_prefixes[0],
            fragment_prefix: published_prefixes[1],
            windows,
        };
        if self.render_slots.get(pair_slot).tracker.is_none() {
            self.render_slots.get_mut(pair_slot).tracker = Some(tracker);
        } else if let Err(mut tracker) = self.render_slots.get_mut(pair_slot).queued_tracker.push(tracker) {
            // Publication may already be visible. Preserve the lease even
            // if an internal capacity invariant failed; stop owns cleanup.
            self.render_command_buffer_state_pairs[pair_slot as usize] = tracker.work_state.take();
            return Err(EIO);
        }
        if context_update {
            self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?.complete_context_update();
        }
        stages.lap(crate::g17_perf::SUB_TRACKER);
        Ok(prepared)
    }

    /// Expose the complete default-profile render pair after the USC FList
    /// update/grow has retired and before the one paired work doorbell.
    pub(crate) fn publish_default_render_pair_after_freelist(
        &mut self, pair_slot: u8,
        prepared: &g17_submission::PreparedG17PPartialOpeningWorkPair,
    ) -> Result {
        if crate::g17_config::RENDER_NATIVE_DOORBELLS != 0
            || crate::g17_config::RENDER_SPLIT_LAUNCH != 0
            || self.render_slots.get(pair_slot).tracker.is_none()
            || self.render_slots.get(pair_slot).storage.is_none()
            || self.render_slots.get(pair_slot).bind.is_none()
        {
            return Err(EINVAL);
        }
        self.resources.publish_deferred_partial_opening_pair(prepared)
    }

    /// Split launch's initial phase: expose only the complete TA outer slot
    /// after the same first-submit FList transaction has retired. The fragment
    /// prefix stays hidden until TA completion publishes its SKSM/AddKicks and
    /// matching deferred outer slot as one later transaction.
    pub(crate) fn publish_split_render_tiling_after_freelist(
        &mut self, pair_slot: u8,
        plan: &g17_submission::G17PPartialOpeningOuterSlotPlan,
    ) -> Result {
        if crate::g17_config::RENDER_NATIVE_DOORBELLS != 0
            || crate::g17_config::RENDER_SPLIT_LAUNCH == 0
            || self.render_slots.get(pair_slot).tracker.is_none()
            || self.render_slots.get(pair_slot).storage.is_none()
            || self.render_slots.get(pair_slot).bind.is_none()
        {
            return Err(EINVAL);
        }
        self.resources.publish_deferred_partial_opening_tiling(plan)
    }

    /// DRAM-only status-page sample; safe at any point, timeout included.
    /// Compute's status page, read exactly as render's is. Same VA.
    pub(crate) fn log_compute_status_pages(&mut self, label: &str) {
        self.resources.log_compute_status_pages(label);
    }

    /// Read both render SKSM entry regions back. Host memory we wrote the kick
    /// entries into, so a change here is direct evidence the firmware consumed
    /// them -- unlike 0x47, which only reports whether the scheduler was idling.
    /// Control for `log_render_sksm_entries`: the same readback on the compute
    /// queue, whose submissions provably complete.
    pub(crate) fn log_compute_sksm_entries(
        &mut self,
        queue: &mut G17PSksmQueue,
        label: &str,
    ) {
        let dev = self.resources.dev();
        let stride = queue.producer.geometry.entry_stride() as usize;
        queue.storage.log_entry(label, &dev, 0);
        queue.storage.log_entry(label, &dev, stride);
    }

    /// DIAGNOSTIC. Release the fragment's parent link (see
    /// `g17p_release_fragment_parent`).
    pub(crate) fn release_fragment_parent(&mut self, index: usize) -> Result<(u32, u32)> {
        self.resources.g17p_release_fragment_parent(index)
    }

    pub(crate) fn log_render_sksm_entries(&mut self, pair_slot: u8, label: &str) {
        // DIAGNOSTIC ONLY, and it must stay that way. This runs from the
        // render submit path at "pre-doorbell" -- i.e. immediately before the
        // doorbell is rung -- and hexdumps roughly 19 KB per call once the two
        // descriptor regions grew to 0x2400. Leaving that in the default path
        // put a printk storm between building the kick and ringing it, and the
        // render came back VK_ERROR_DEVICE_LOST on BOTH the cold and the warm
        // path. Gate it exactly like log_render_status_pages just below.
        if crate::g17_config::DEP_RECORDS == 0 {
            return;
        }
        let strides = match self.render_slots.get(pair_slot).sksm.as_ref() {
            Some(queues) => [
                queues.tiling.producer.geometry.entry_stride() as usize,
                queues.fragment.producer.geometry.entry_stride() as usize,
            ],
            None => [0usize, 0usize],
        };
        let dev = self.resources.dev();
        if let Some(storage) = self.render_slots.get_mut(pair_slot).storage.as_deref_mut() {
            storage.log_render_queue_record(&dev, true, label);
            storage.log_render_queue_record(&dev, false, label);
        }
        let dumpdev = self.resources.dev();
        if let Some(storage) = self.render_slots.get_mut(pair_slot).storage.as_deref_mut() {
            storage.dump_config_update_region(&dumpdev, true, label);
            storage.dump_config_update_region(&dumpdev, false, label);
            storage.dump_graph_region(&dumpdev, "evt-TA", label, 0x3200, 0x40);
            storage.dump_graph_region(&dumpdev, "evt-3D", label, 0x3600, 0x40);
            storage.dump_descriptor_region(&dumpdev, "desc-TA", label, 0x0000, 0x2400);
            // Full fragment descriptor, not the first 0x200. Native's 3D command
            // record is captured to 0x2400 (recs3.txt), so dumping the same
            // extent makes a byte diff possible -- which is what "the fragment
            // executes but writes no pixels" now needs.
            storage.dump_descriptor_region(&dumpdev, "desc-3D", label, 0x4000, 0x2400);
            // b1 FUN_23004 selects the RENDER job state machine (FUN_15bcc,
            // which ends at the DM launch strobe FUN_15a48 -> 0x1c998 = 1)
            // when the command it builds the job from has +0x866 == 3. The
            // fragment is never launched, so read that byte on both stages.
            storage.dump_descriptor_region(&dumpdev, "kind-TA", label, 0x0840, 0x40);
            storage.dump_descriptor_region(&dumpdev, "kind-3D", label, 0x4840, 0x40);
            // b1 FUN_162f8, the 3D activation, is called as
            // FUN_162f8(sku+0x79c, sku+0x7a8) from the TILING job's state 7:
            // +0x79c names the fragment's QUEUE ID and +0x7a8 the fragment
            // kick entry's index. It uses exactly those two to find the entry
            // it patches, so if either is wrong in the tiling SKU the fragment
            // is never released. Dump that window of the tiling record.
            storage.dump_descriptor_region(&dumpdev, "link-TA", label, 0x0780, 0x40);
        }
        if let Some(storage) = self.render_slots.get_mut(pair_slot).storage.as_deref_mut() {
            storage.dump_sksm_entry(&dumpdev, true, label);
            storage.dump_sksm_entry(&dumpdev, false, label);
            storage.log_sksm_entry(&dumpdev, true, label, 0);
            storage.log_sksm_entry(&dumpdev, true, label, strides[0]);
            storage.log_sksm_entry(&dumpdev, true, label, strides[0] + 0x120);
            storage.log_sksm_entry(&dumpdev, false, label, 0);
            storage.log_sksm_entry(&dumpdev, false, label, strides[1]);
            storage.log_sksm_entry(&dumpdev, false, label, strides[1] + 0x120);
        }
    }

    pub(crate) fn log_render_status_pages(&mut self, pair_slot: u8, label: &str) {
        if !crate::util::g17p_verbose() {
            return;
        }
        // Report half in its own cold frame; the per-storage page dumps
        // (which nest further) run after it has returned.
        self.log_render_status_pages_report(pair_slot, label);
        let dev = self.resources.dev();
        if let Some(storage) = self.render_slots.get_mut(pair_slot).storage.as_deref_mut() {
            storage.log_status_pages(&dev, label);
            storage.log_parameter_management(&dev, label);
        }
    }

    #[cold]
    #[inline(never)]
    fn log_render_status_pages_report(&mut self, pair_slot: u8, label: &str) {
        if crate::g17_config::DEP_RECORDS != 0 {
            let dev = self.resources.dev();
            {
                let phys = G17P_GFX_PHYSICAL_DATA_BASE
                    + (G17P_GDIRECTOR_SHADOW_LINK_BASE - G17P_GFX_LINK_DATA_BASE);
                match pgtable::LiveFirmwareU64Probe::new(phys)
                    .and_then(|p| p.observe_mask(u64::MAX, 1))
                    .map(|o| o.first)
                {
                    Ok(v) => dev_info!(
                        dev.as_ref(),
                        "G17P gdirector-shadow[{}] link={:#x} raw={:#018x} hwbuf0={{prio {:#04x},share {:#04x}}} hwbuf1={{{:#04x},{:#04x}}} hwbuf2={{{:#04x},{:#04x}}} hwbuf3={{{:#04x},{:#04x}}}\n",
                        label,
                        G17P_GDIRECTOR_SHADOW_LINK_BASE,
                        v,
                        v & 0xff, (v >> 8) & 0xff,
                        (v >> 16) & 0xff, (v >> 24) & 0xff,
                        (v >> 32) & 0xff, (v >> 40) & 0xff,
                        (v >> 48) & 0xff, (v >> 56) & 0xff,
                    ),
                    Err(e) => dev_warn!(
                        dev.as_ref(),
                        "G17P gdirector-shadow[{}] unreadable ({:?})\n",
                        label,
                        e
                    ),
                }
            }
            {
                let rd = |link: u64| -> Option<u64> {
                    let phys = G17P_GFX_PHYSICAL_DATA_BASE
                        + (link - G17P_GFX_LINK_DATA_BASE);
                    pgtable::LiveFirmwareU64Probe::new(phys)
                        .and_then(|p| p.observe_mask(u64::MAX, 1))
                        .map(|o| o.first)
                        .ok()
                };
                for dm in 0u64..3 {
                    let a = rd(G17P_DM_ACTIVE_BITMAP_LINK_BASE + dm * 0x10);
                    let b = rd(G17P_DM_ACTIVE_BITMAP_LINK_BASE + dm * 0x10 + 8);
                    dev_info!(
                        dev.as_ref(),
                        "G17P dm-active[{}] dm={} lo={:#018x} hi={:#018x}\n",
                        label,
                        dm,
                        a.unwrap_or(u64::MAX),
                        b.unwrap_or(u64::MAX),
                    );
                }
                dev_info!(
                    dev.as_ref(),
                    "G17P ready-mask[{}] = {:#018x}\n",
                    label,
                    rd(G17P_READY_MASK_LINK_BASE).unwrap_or(u64::MAX),
                );
                for qid in [
                    4u64,
                    u64::from(g17_resources::g17p_render_queue_id(true)),
                    u64::from(g17_resources::g17p_render_queue_id(false)),
                ] {
                    let park = G17P_KSM_PARK_RECORD_LINK_BASE + qid * 0x28;

                    let counters = rd(park + 0x418);
                    let deadline = rd(park + 0x400);
                    // Same record through the SECOND ASC's data window. The
                    // scheduler pass may not run on the ASC whose base the
                    // other probes here use, and a per-queue deadline that
                    // reads identically for every qid says this one does not.
                    let rd1 = |link: u64| -> Option<u64> {
                        let phys = G17P_GFX1_PHYSICAL_DATA_BASE
                            + (link - G17P_GFX1_LINK_DATA_BASE);
                        pgtable::LiveFirmwareU64Probe::new(phys)
                            .and_then(|p| p.observe_mask(u64::MAX, 1))
                            .map(|o| o.first)
                            .ok()
                    };
                    let counters1 = rd1(park + 0x418);
                    let deadline1 = rd1(park + 0x400);
                    let raw1 = counters1.unwrap_or(u64::MAX);
                    let dl1 = deadline1.unwrap_or(u64::MAX);
                    dev_info!(
                        dev.as_ref(),
                        "G17P park-gfx1[{}] qid={} raw={:#018x} deadline={:#018x}\n",
                        label,
                        qid,
                        raw1,
                        dl1,
                    );
                    let raw = counters.unwrap_or(u64::MAX);
                    let byte_a = raw & 0xff;
                    let byte_b = (raw >> 8) & 0xff;
                    let byte_c = (raw >> 16) & 0xff;
                    let parked = byte_a == byte_b;
                    let dl = deadline.unwrap_or(u64::MAX);
                    dev_info!(
                        dev.as_ref(),
                        "G17P park[{}] qid={} A={} B={} C={} A==B(park)={} raw={:#018x} deadline={:#018x}\n",
                        label,
                        qid,
                        byte_a,
                        byte_b,
                        byte_c,
                        parked,
                        raw,
                        dl,
                    );
                }
                for qid in [
                    4u64,
                    u64::from(g17_resources::g17p_render_queue_id(true)),
                    u64::from(g17_resources::g17p_render_queue_id(false)),
                ] {
                    let st = rd(G17P_QUEUE_STATE_LINK_BASE + qid * 0x28);
                    let st2 = rd(G17P_QUEUE_STATE_LINK_BASE + qid * 0x28 + 8);
                    let fl = rd(G17P_QUEUE_FLAG_LINK_BASE + qid * 0x28 + 0x40);
                    dev_info!(
                        dev.as_ref(),
                        "G17P qstate[{}] qid={} +0={:#018x} +8={:#018x} flag+0x40={:#018x}\n",
                        label,
                        qid,
                        st.unwrap_or(u64::MAX),
                        st2.unwrap_or(u64::MAX),
                        fl.unwrap_or(u64::MAX),
                    );
                }
            }
            for qid in [
                4u8,
                g17_resources::g17p_render_queue_id(true) as u8,
                g17_resources::g17p_render_queue_id(false) as u8,
            ] {
                let link = G17P_DEPENDENCY_RECORD_LINK_BASE
                    + u64::from(qid) * G17P_DEPENDENCY_RECORD_STRIDE;
                let phys = G17P_GFX_PHYSICAL_DATA_BASE
                    + (link - G17P_GFX_LINK_DATA_BASE);
                let head = pgtable::LiveFirmwareU64Probe::new(phys)
                    .and_then(|p| p.observe_mask(u64::MAX, 1))
                    .map(|o| o.first);
                let stamp = pgtable::LiveFirmwareU64Probe::new(phys + 8)
                    .and_then(|p| p.observe_mask(u64::MAX, 1))
                    .map(|o| o.first);
                match (head, stamp) {
                    (Ok(head), Ok(stamp)) => dev_info!(
                        dev.as_ref(),
                        "G17P dep-record[{}] qid={} link={:#x} phys={:#x} head={:#018x} valid={} completed={:#018x}\n",
                        label,
                        qid,
                        link,
                        phys,
                        head,
                        head & 1,
                        stamp,
                    ),
                    _ => dev_warn!(
                        dev.as_ref(),
                        "G17P dep-record[{}] qid={} unreadable at phys={:#x}\n",
                        label,
                        qid,
                        phys,
                    ),
                }
            }
        }

        {
            let dev = self.resources.dev();
            {
                let mut pa = [0u8; 0x200];
                if self.render_slots.get_mut(pair_slot)
                    .storage
                    .as_deref_mut()
                    .map(|s| s.read_pool_a_record(&mut pa).is_ok())
                    .unwrap_or(false)
                {
                    // The TA descriptor's +0x10 selects WHICH Pool-A record the
                    // firmware gates on, and it is record 1 by default -- read
                    // both. `FUN_16f7c` sets +0x94 = 1 after linking the job, so
                    // a 1 proves the classic TA enqueue ran and the job is
                    // sitting in the list; a 0 proves it never ran at all.
                    let r0 = u32::from_le_bytes([pa[0x94], pa[0x95], pa[0x96], pa[0x97]]);
                    let r1 = u32::from_le_bytes([
                        pa[0x194], pa[0x195], pa[0x196], pa[0x197],
                    ]);
                    let v1 = if r1 != 0 {
                        "rec1 QUEUED (FUN_16f7c ran, job linked, nothing dequeues DM0)"
                    } else {
                        "rec1 NOT queued (FUN_16f7c never ran)"
                    };
                    dev_info!(
                        dev.as_ref(),
                        "G17P pool-a[{}] rec0(+0x00={:02x?} +0x10={:02x?} +0x94={:#010x}) rec1(+0x00={:02x?} +0x10={:02x?} +0x94={:#010x} link={:02x?}) {}\n",
                        label,
                        &pa[0x00..0x08],
                        &pa[0x10..0x14],
                        r0,
                        &pa[0x100..0x108],
                        &pa[0x110..0x114],
                        r1,
                        &pa[0x198..0x1a8],
                        v1,
                    );
                }
            }
            let mut cc = [0u8; 0x40];
            let ok = self.render_slots.get_mut(pair_slot)
                .storage
                .as_deref_mut()
                .map(|s| s.read_channel_control(&mut cc).is_ok())
                .unwrap_or(false);
            if ok {
                dev_info!(
                    dev.as_ref(),
                    "G17P render channel-control[{}]: pid={:#04x} idx={:#04x} f2={:#04x} prio={:#04x} class={:#04x} qidmask={:02x?} hwbuf={:#04x}\n",
                    label,
                    cc[0],
                    cc[1],
                    cc[2],
                    cc[3],
                    cc[4],
                    &cc[0x12..0x1a],
                    cc[0x33],
                );
            }
        }

        self.resources.dump_render_qos_regions(label);
        if crate::module_parameters::g17p_dump_state.value() != 0
            && !G17P_STATE_DUMPED.swap(true, core::sync::atomic::Ordering::Relaxed)
        {
            let kib = crate::module_parameters::g17p_dump_state.value() as usize;
            if kib == 1 {
                self.resources.checksum_primary_state(label);
            } else {
                self.resources.dump_primary_state(label, kib * 1024);
            }
        }
        if crate::module_parameters::g17p_dump_hwdata.value() != 0
            && !G17P_HW_DATA_DUMPED.swap(true, core::sync::atomic::Ordering::Relaxed)
        {
            self.resources.dump_hw_data(label);
        }
        self.resources.log_render_parameter_buffer_descriptor(
            label,
            self.render_slots.get(pair_slot).hardware_buffer_id.unwrap_or(0),
        );
        let selected_metric = self.render_slots.get(pair_slot)
            .storage
            .as_deref()
            .ok_or(ENODEV)
            .and_then(|storage| {
                Ok((storage.parameter_metrics_offset(), storage.selected_scene_index()?))
            })
            .and_then(|(offset, scene)| {
                self.resources.selected_parameter_metric(offset, scene)
            });
        let dev = self.resources.dev();
        match selected_metric {
            Ok(value) => dev_info!(
                dev.as_ref(),
                "G17P render PM[{}]: actual-selected-metric={:#x}\n",
                label,
                value,
            ),
            Err(error) => dev_warn!(
                dev.as_ref(),
                "G17P render PM[{}]: actual selected metric read failed ({:?})\n",
                label,
                error,
            ),
        }
        if let Some(storage) = self.render_slots.get_mut(pair_slot).storage.as_deref_mut() {
            match storage.render_activation_coordinates() {
                Ok((ta_qid, ta_stamp, sku_reserved, sku_pointer, sku_header)) => dev_info!(
                    dev.as_ref(),
                    "G17P render activation[{}]: TA-link qid={} stamp={:#x} predecessor={:#x}; 3D-SKU reserved={:#x} stream={:#x} header={:#x}\n",
                    label,
                    ta_qid,
                    ta_stamp,
                    ta_stamp.wrapping_sub(1) & ((1u64 << 40) - 1),
                    sku_reserved,
                    sku_pointer,
                    sku_header,
                ),
                Err(error) => dev_warn!(
                    dev.as_ref(),
                    "G17P render activation[{}]: descriptor coordinates unreadable ({:?})\n",
                    label,
                    error,
                ),
            }
            match storage.render_activation_gate_words() {
                Ok(gate) => dev_info!(
                    dev.as_ref(),
                    "G17P render activation gate[{}]: TA(+7a4,+866,+892,+8ba,+8cb)={:x?}\n",
                    label,
                    gate,
                ),
                Err(error) => dev_warn!(
                    dev.as_ref(),
                    "G17P render activation gate[{}]: descriptor inputs unreadable ({:?})\n",
                    label,
                    error,
                ),
            }
        }
    }

    /// Snapshot every host-DRAM render progress marker while firmware is
    /// halted at recovery state 1, before the host acknowledges the restart.
    /// The recovery path retires both queue graphs and writes a synthetic 3D
    /// completion record, so the ordinary timeout dump is too late to tell a
    /// launch failure from recovery cleanup.
    /// Locate a reported MMU fault address inside the render VM's own tables.
    ///
    /// The G14X fault-address register is documented as VA >> 6, but that
    /// reading and the raw one cannot be told apart from a fault inside the
    /// first page. The GMMU report's VM slot is the descriptor/application
    /// context, not the ordinary VM bind slot. Resolve the retained storage by
    /// that exact context before walking its owner. Then report whether the
    /// selected VM maps the page and every mapped run within +/-1 MiB of it. A
    /// candidate that abuts or falls just outside a live run is the real
    /// address; one that lands in empty space at every scale is not.
    fn render_bind_for_descriptor_context(
        &self,
        descriptor_context_id: u8,
    ) -> Option<(&G17PRenderRootOwner, &'static str)> {
        let pair_slot = 0;
        let matches = |storage: &g17_resources::G17PUserRenderStorage| {
            storage.descriptor_context_id() == u16::from(descriptor_context_id)
        };
        if self.render_slots.get(pair_slot).storage.as_deref().is_some_and(matches) {
            if let Some(bind) = self.render_slots.get(pair_slot).bind.as_ref() {
                return Some((bind, "active"));
            }
        }
        if let Some(retirement) = self.render_slots.get(pair_slot).context_retirement.as_ref().filter(|retirement| {
            retirement.bind.root_slot().ok() == Some(descriptor_context_id)
        }) {
            return Some((&retirement.bind, "active-context-retirement"));
        }
        if let Some(graph) = self
            .logical_queue_teardowns
            .iter()
            .flat_map(|teardown| teardown.graphs.iter())
            .find(|graph| matches(&graph.storage))
        {
            return Some((&graph.bind, "teardown-displaced"));
        }
        for state in self.render_slots.parked(pair_slot) {
            if state.storage.as_deref().is_some_and(matches) {
                if let Some(bind) = state.bind.as_ref() {
                    return Some((bind, "parked"));
                }
            }
            if let Some(retirement) = state.context_retirement.as_ref().filter(|retirement| {
                retirement.bind.root_slot().ok() == Some(descriptor_context_id)
            }) {
                return Some((&retirement.bind, "parked-context-retirement"));
            }
        }
        None
    }

    pub(crate) fn log_fault_address_candidates(
        &self,
        label: &'static str,
        addr_word: u64,
        fault_vm_slot: u8,
    ) -> bool {
        let Some((bind, owner)) = self.render_bind_for_descriptor_context(fault_vm_slot) else {
            return false;
        };
        self.log_fault_address_candidates_for_vm(
            label,
            addr_word,
            fault_vm_slot,
            bind.vm(),
            bind.vm_id(),
            bind.root(),
            owner,
        );
        // The GMMU context is independent of the blamed queue. In particular,
        // a retained context may have no host tracker while hardware reports
        // its address space. Preserve its pool counters and queue frontiers
        // rather than silently excluding it from the live-peer snapshot.
        if module_parameters::g17p_fault_report.value() & 4 != 0 {
            let read = |link: u64| -> Result<u64> {
                pgtable::LiveFirmwareU64Probe::new(g17p_gfx_link_to_physical(link))
                    .and_then(|probe| probe.observe_mask(u64::MAX, 1))
                    .map(|sample| sample.first)
            };
            for slot in 0..g17_resources::G17PRenderQueuePair::SLOT_COUNT {
                let Ok(state) = self.render_slots.allocated(slot) else { continue };
                let Some(storage) = state.storage.as_deref().filter(|storage|
                    storage.descriptor_context_id() == u16::from(fault_vm_slot)) else { continue };
                dev_info!(self.resources.dev().as_ref(),
                    "G17P fault-address owner[{}]: slot={} context={} host-tracker={} queued={} (retained ownership, not proof of executing command)\n",
                    label, slot, fault_vm_slot, state.tracker.is_some(), state.queued_tracker.len());
                storage.log_fault_pool(self.resources.dev());
                if let Some(queues) = state.sksm.as_ref() {
                    for queue in [&queues.tiling, &queues.fragment] {
                        let qid = u64::from(queue.queue_id);
                        let dependency = G17P_DEPENDENCY_RECORD_LINK_BASE
                            + qid * G17P_DEPENDENCY_RECORD_STRIDE;
                        let cached: [Result<u64>; 5] = core::array::from_fn(|field|
                            read(G17P_QUEUE_FLAG_LINK_BASE + qid * 0x28 + 0x20 + field as u64 * 8));
                        dev_info!(self.resources.dev().as_ref(),
                            "G17P fault-address owner frontier: qid={} latest={:#x} dependency={:x?} packed-completion={:x?} cached-ksm={:x?}\n",
                            qid, queue.producer.previous_timestamp,
                            [read(dependency), read(dependency + 8), read(dependency + 16)],
                            read(0xffff_fc00_0012_5df0 + qid * 8), cached);
                    }
                }
            }
        }
        true
    }

    pub(crate) fn log_compute_fault_address_candidates(
        &self,
        label: &'static str,
        addr_word: u64,
        fault_vm_slot: u8,
        context: &crate::g17_resources::G17PLogicalQueueContext,
        owner: &'static str,
    ) {
        self.log_fault_address_candidates_for_vm(
            label,
            addr_word,
            fault_vm_slot,
            context.vm(),
            context.vm_id(),
            context.root(),
            owner,
        );
    }

    fn log_fault_address_candidates_for_vm(
        &self,
        label: &'static str,
        addr_word: u64,
        fault_vm_slot: u8,
        vm: &mmu::Vm,
        vm_id: u64,
        root: u64,
        owner: &'static str,
    ) {
        let dev = self.resources.dev();
        dev_info!(
            dev.as_ref(),
            "G17P fault address[{}]: walking {} owner vm={} root={:#x} vm-slot={}\n",
            label,
            owner,
            vm_id,
            root,
            fault_vm_slot,
        );
        // reason=3 is ReadOnly, so the leaf's write permission IS the
        // diagnosis. Name the exclusive alias the address lands in and say
        // whether this driver maps it read-only; several of them are RO by
        // construction, and a firmware write into one of those is the answer.
        for (pair_slot, slot) in self.render_slots.slots.iter().enumerate() {
            let Some(storage) = slot.as_deref().and_then(|slot| slot.storage.as_deref()) else {
                continue;
            };
            for (name, candidate) in [
                ("raw", addr_word),
                ("shift6", (addr_word & ((1u64 << 42) - 1)) << 6),
                ("shift6-masked", (addr_word << 6) & ((1u64 << 42) - 1)),
            ] {
                if let Some((alias, read_only)) = storage.name_alias_containing(candidate) {
                    dev_err!(
                        dev.as_ref(),
                        "G17PMARK fault alias[{}]: {} candidate {:#x} lands in pair {} alias {} mapped {} -- a reason=3 fault here means the firmware WROTE through it\n",
                        label,
                        name,
                        candidate,
                        pair_slot,
                        alias,
                        if read_only { "READ-ONLY" } else { "read-write" },
                    );
                }
            }
        }
        let candidates = [
            ("raw", addr_word),
            ("shift6", (addr_word & ((1u64 << 42) - 1)) << 6),
            ("shift6-masked", (addr_word << 6) & ((1u64 << 42) - 1)),
        ];
        for (name, candidate) in candidates {
            let page = candidate & !(mmu::UAT_PGSZ as u64 - 1);
            let mapped = vm.covers_range(page, mmu::UAT_PGSZ as u64, true, false);
            let translation = vm.translate_iova(candidate);
            dev_info!(
                dev.as_ref(),
                "G17P fault address[{}]: {} candidate {:#x} page {:#x} mapped={} translate={:?}\n",
                label,
                name,
                candidate,
                page,
                mapped,
                translation,
            );
            let window_start = page.saturating_sub(0x10_0000);
            let window_end = match page.checked_add(0x10_0000) {
                Some(end) => end,
                None => continue,
            };
            let mut ranges = [pgtable::MappedRange::default(); 16];
            match vm.mapped_ranges(window_start..window_end, &mut ranges) {
                Ok((count, truncated)) => {
                    dev_info!(
                        dev.as_ref(),
                        "G17P fault address[{}]: {} neighbourhood {:#x}..{:#x} has {} run(s){}\n",
                        label,
                        name,
                        window_start,
                        window_end,
                        count,
                        if truncated { " TRUNCATED" } else { "" },
                    );
                    for run in ranges.iter().take(count) {
                        dev_info!(
                            dev.as_ref(),
                            "G17P fault address[{}]:   {:#x}..{:#x} pte={:#x}\n",
                            label,
                            run.start,
                            run.end,
                            run.pte,
                        );
                    }
                }
                Err(error) => dev_info!(
                    dev.as_ref(),
                    "G17P fault address[{}]: {} neighbourhood walk failed ({:?})\n",
                    label,
                    name,
                    error,
                ),
            }
        }

        // One fault-only discriminator for Honeykrisp's sparse buffer layout:
        // a mapped lower peer with an unmapped upper address means the shadow
        // bind disappeared, while two unmapped peers point to a stale or
        // malformed address. Keep this to one line; it is not submit logging.
        let decoded = (addr_word << 6) & ((1u64 << 42) - 1);
        if let Some(candidate) = g17p_fault_sparse_ro_lower(decoded) {
            let page = candidate & !(mmu::UAT_PGSZ as u64 - 1);
            let mapped = vm.covers_range(page, mmu::UAT_PGSZ as u64, true, false);
            let translation = vm.translate_iova(candidate);
            dev_info!(
                dev.as_ref(),
                "G17P fault address[{}]: Honeykrisp sparse-RO lower peer {:#x} page {:#x} mapped={} translate={:?}\n",
                label,
                candidate,
                page,
                mapped,
                translation,
            );
        }

        // The whole client root, so any decode of the address register can be
        // tested against the real layout instead of one window at a time.
        let mut all = [pgtable::MappedRange::default(); 96];
        // The full 42-bit low half, from the client null view up to the
        // unknown page, so no decode of the address register is excluded by
        // the window rather than by the tables.
        let user_range = 0u64..(1u64 << 42);
        match vm.mapped_ranges(user_range.clone(), &mut all) {
            Ok((count, truncated)) => {
                dev_info!(
                    dev.as_ref(),
                    "G17P fault address[{}]: client root {:#x}..{:#x} holds {} run(s){}\n",
                    label,
                    user_range.start,
                    user_range.end,
                    count,
                    if truncated { " TRUNCATED" } else { "" },
                );
                for run in all.iter().take(count) {
                    dev_info!(
                        dev.as_ref(),
                        "G17P fault address[{}]: run {:#x}..{:#x} ({:#x}) pte={:#x}\n",
                        label,
                        run.start,
                        run.end,
                        run.end - run.start,
                        run.pte,
                    );
                }
            }
            Err(error) => dev_info!(
                dev.as_ref(),
                "G17P fault address[{}]: client root walk failed ({:?})\n",
                label,
                error,
            ),
        }
    }

    /// Called only by the pre-ACK halted DM0 gate. The pointer table and all
    /// accepted objects are pinned B1 DATA, not arbitrary firmware memory.
    /// Log raw callback identity; states have different meanings per callback.
    pub(crate) fn log_halted_ta_callback(&self, hardware_slot: u32) {
        if hardware_slot >= 16 { return; }
        let read = |link: u64| -> Result<u64> {
            pgtable::LiveFirmwareU64Probe::new(g17p_gfx_link_to_physical(link))
                .and_then(|probe| probe.observe_mask(u64::MAX, 1))
                .map(|sample| sample.first)
        };
        let table = 0xffff_fc00_0012_f670 + u64::from(hardware_slot) * 8;
        let pointer = match read(table) { Ok(pointer) => pointer, Err(_) => return };
        let Some(index) = g17_completion::g17p_ta_callback_index(pointer) else {
            dev_info!(self.resources.dev().as_ref(),
                "G17P halted TA callback: slot={} pointer={:#x} unavailable/outside-static-pool\n",
                hardware_slot, pointer);
            return;
        };
        let fields: [Result<u64>; 7] = core::array::from_fn(|i| read(pointer + i as u64 * 8));
        dev_info!(self.resources.dev().as_ref(),
            "G17P halted TA callback: slot={} index={} pointer={:#x} table-stable={} raw={:x?} (match descriptor before interpretation)\n",
            hardware_slot, index, pointer, read(table).is_ok_and(|p| p == pointer), fields);
    }

    /// v97d: every pair with tracked outstanding work: (pair slot, TA qid,
    /// 3D qid, last published 3D stamp, 3D stamp count, 3D entry stride).
    pub(crate) fn render_stall_candidates(&self) -> KVec<(u8, u8, u8, u64, u16, u32)> {
        let mut out = KVec::new();
        for (index, slot) in self.render_slots.slots.iter().enumerate() {
            let Some(slot) = slot.as_deref() else { continue };
            if slot.tracker.is_none() && slot.queued_tracker.iter().next().is_none() { continue; }
            let Some(queues) = slot.sksm.as_ref() else { continue };
            let geometry = queues.fragment.producer.geometry;
            if out.push((index as u8, queues.tiling.queue_id, queues.fragment.queue_id,
                queues.fragment.producer.previous_timestamp, geometry.stamp_count(),
                geometry.entry_stride()), GFP_KERNEL).is_err() { break; }
        }
        out
    }

    /// v97d: one firmware DATA qword, read exactly like the fault-only
    /// scheduler snapshots (cache-invalidated load, no write, no message).
    fn read_firmware_data_u64(link: u64) -> Result<u64> {
        pgtable::LiveFirmwareU64Probe::new(g17p_gfx_link_to_physical(link))
            .and_then(|probe| probe.observe_mask(u64::MAX, 1))
            .map(|sample| sample.first)
    }

    /// Recovery isolation bit5: the cached key (qid << 40 | stamp) of one
    /// hardware KSM slot (firmware DATA `0x1774b8 + slot * 8`, the same word
    /// the B1 DM1 slot diagnostic prints). Read-only.
    pub(crate) fn firmware_ksm_slot_key(slot: usize) -> Option<u64> {
        if slot >= 16 { return None; }
        Self::read_firmware_data_u64(0xffff_fc00_0017_74b8 + slot as u64 * 8).ok()
    }

    /// Recovery isolation bit5: whether the tracked pass with firmware stamp
    /// `stamp` (ordinal + 1) on `pair_slot` had started, i.e. its tiler
    /// stamped the pass start. None if that stamp is not a tracked pass.
    pub(crate) fn render_pass_started(&mut self, pair_slot: u8, stamp: u64) -> Option<bool> {
        if !self.render_slot_allocated(pair_slot) { return None; }
        const STAMP_MASK: u64 = 0xff_ffff_ffff;
        let slot = self.render_slots.get_mut(pair_slot);
        let matches = |ordinal: u64| ordinal.wrapping_add(1) & STAMP_MASK == stamp & STAMP_MASK;
        let ordinal = Iterator::chain(slot.tracker.iter(), slot.queued_tracker.iter())
            .map(|tracker| tracker.ordinal)
            .find(|ordinal| matches(*ordinal))?;
        let state = slot.storage.as_deref_mut()?.snapshot(ordinal).ok()?;
        Some(state.tiling_timestamps[0] != 0)
    }

    /// iso4: whether the head tracked pass of `pair_slot` had started (its
    /// tiler stamped the pass start). None without a tracked pass or storage.
    /// Read-only driver DRAM, as `render_pass_started`.
    pub(crate) fn render_slot_head_pass_started(&mut self, pair_slot: u8) -> Option<bool> {
        if !self.render_slot_allocated(pair_slot) { return None; }
        let slot = self.render_slots.get_mut(pair_slot);
        let ordinal = slot.tracker.as_ref()?.ordinal;
        let state = slot.storage.as_deref_mut()?.snapshot(ordinal).ok()?;
        Some(state.tiling_timestamps[0] != 0)
    }

    /// v97d: the firmware's completed stamp for `qid` when its dependency
    /// record is valid.
    pub(crate) fn firmware_dependency_completed(qid: u8) -> Option<u64> {
        let record = G17P_DEPENDENCY_RECORD_LINK_BASE
            + u64::from(qid) * G17P_DEPENDENCY_RECORD_STRIDE;
        match (Self::read_firmware_data_u64(record), Self::read_firmware_data_u64(record + 8)) {
            (Ok(valid), Ok(completed)) if valid & 0xff == 1 => Some(completed),
            _ => None,
        }
    }

    /// v97d: B1 scheduler state for `qids` -- the requested/sampled/deferred/
    /// next mask pairs, the runnable mask, the per-DM active bitmaps, and per
    /// QID the dependency record, park record (+0x3f8..+0x418: deadline at
    /// +0x400, A/B/C bytes at +0x418), owner, queue state and flag. Returns
    /// whether `watch_qid` is set in the requested and in the deferred mask.
    pub(crate) fn log_b1_scheduler_state(&mut self, label: &str, qids: &[u8], watch_qid: Option<u8>)
        -> (bool, bool) {
        let read = Self::read_firmware_data_u64;
        let masks: [Result<u64>; 8] = core::array::from_fn(|index|
            read(G17P_KSM_SCHEDULER_MASKS_LINK + index as u64 * 8));
        let runnable = [read(G17P_KSM_RUNNABLE_MASK_LINK), read(G17P_KSM_RUNNABLE_MASK_LINK + 8)];
        let active: [Result<u64>; 6] = core::array::from_fn(|index|
            read(G17P_DM_ACTIVE_BITMAP_LINK_BASE + (index as u64 / 2) * 0x10 + (index as u64 % 2) * 8));
        let ready = read(G17P_READY_MASK_LINK_BASE);
        dev_info!(self.resources.dev().as_ref(),
            "G17P B1 scheduler[{}]: masks(requested,sampled,deferred,next)={:x?} runnable={:x?} dm-active(0,1,2)={:x?} ready={:x?}\n",
            label, masks, runnable, active, ready);
        for &qid in qids {
            let qid64 = u64::from(qid);
            let dependency = G17P_DEPENDENCY_RECORD_LINK_BASE + qid64 * G17P_DEPENDENCY_RECORD_STRIDE;
            let park = G17P_KSM_PARK_RECORD_LINK_BASE + qid64 * 0x28;
            let park_record: [Result<u64>; 5] = core::array::from_fn(|word| read(park + 0x3f8 + word as u64 * 8));
            dev_info!(self.resources.dev().as_ref(),
                "G17P B1 queue[{}]: qid={} dep(valid,completed,status2)={:x?} park(+3f8..+418)={:x?} owner={:x?} qstate={:x?} qflag={:x?} packed-completion={:x?}\n",
                label, qid,
                [read(dependency), read(dependency + 8), read(dependency + 16)],
                park_record,
                read(G17P_QID_OWNER_LINK_BASE + qid64 * 8),
                [read(G17P_QUEUE_STATE_LINK_BASE + qid64 * 0x28), read(G17P_QUEUE_STATE_LINK_BASE + qid64 * 0x28 + 8)],
                read(G17P_QUEUE_FLAG_LINK_BASE + qid64 * 0x28 + 0x40),
                read(0xffff_fc00_0012_5df0 + qid64 * 8));
        }
        // DM1 monitor slots: the cached c078 key (qid << 40 | stamp) and the
        // shared 24-byte monitor record. Historical DRAM, printed if populated.
        for slot in 0..16usize {
            let key = read(0xffff_fc00_0017_74b8 + slot as u64 * 8);
            let monitor = self.resources.firmware_dm1_slot_snapshot(slot).map(|sample| sample.raw);
            let populated = key.as_ref().is_ok_and(|key| *key != 0)
                || monitor.as_ref().is_ok_and(|raw| raw.iter().any(|word| *word != 0));
            if populated {
                dev_info!(self.resources.dev().as_ref(),
                    "G17P B1 DM1 slot[{}]: slot={} key={:x?} monitor={:x?}\n",
                    label, slot, key, monitor);
            }
        }
        let Some(qid) = watch_qid else { return (false, false) };
        let word = usize::from(qid / 64);
        let bit = 1u64 << (qid % 64);
        let set = |pair: usize| masks[pair * 2 + word].as_ref().is_ok_and(|value| value & bit != 0);
        (set(0), set(2))
    }

    /// Native `AGXAccelerator::restartWorkQueue` for every AGX3DWorkQueue:
    /// each render pair that carries a graph gets its TA and 3D recovery
    /// targets set to the current producers. Called with firmware halted at
    /// recovery state 1, before the host's 1 -> 2 acknowledge. Graphs parked
    /// in a logical-queue teardown are not on any pair (a successor's graph
    /// owns the QIDs) and are skipped. Returns (pairs published, first failure).
    pub(crate) fn publish_render_recovery_targets(&mut self) -> (u32, Option<(u8, Error)>) {
        let mut published = 0u32;
        let mut failure = None;
        for (slot, state) in self.render_slots.iter_mut() {
            let Some(storage) = state.and_then(|state| state.storage.as_deref_mut()) else {
                continue;
            };
            match storage.publish_recovery_targets() {
                Ok(_) => published += 1,
                Err(error) => {
                    if failure.is_none() { failure = Some((slot, error)); }
                }
            }
        }
        (published, failure)
    }

    pub(crate) fn log_render_recovery_pre_ack(&mut self, label: &'static str) {
        let blamed = self.pending_primary_recovery()
            .and_then(|(_, _, qid)| qid)
            .and_then(|qid| self.render_slot_for_qid(qid).map(|slot| (qid, slot)));
        if let Some((qid, slot)) = blamed {
            if self.render_slots.allocated(slot).is_ok() {
                dev_info!(self.resources.dev().as_ref(),
                    "G17P render fault owner[{}]: blamed-qid={} slot={}\n", label, qid, slot);
                self.log_selected_render_recovery_pre_ack(slot, label, None);
            }
        }
        if module_parameters::g17p_fault_report.value() & 4 != 0 {
            dev_info!(self.resources.dev().as_ref(),
                "G17P gUPM retained request[{}]: words={:x?} (inactive is historical)\n",
                label, diagnostic_gupm_request());
            dev_info!(self.resources.dev().as_ref(),
                "G17P FList worker active pools[{}]: words0..3={:x?}\n",
                label, g17p_read_binding_row::<4>(G17P_FLIST_ACTIVE_BITMAP_LINK, 0, 0x20));
            for slot in 0..g17_resources::G17PRenderQueuePair::SLOT_COUNT {
                if blamed.is_some_and(|(_, primary)| primary == slot) { continue; }
                let live = self.render_slots.allocated(slot).is_ok_and(|state|
                    state.storage.is_some()
                        && (state.tracker.is_some() || state.queued_tracker.is_some()));
                if !live { continue; }
                dev_info!(self.resources.dev().as_ref(),
                    "G17P render fault peer owner: slot={} (not inferred fault source)\n", slot);
                self.log_selected_render_recovery_pre_ack(slot, "peer-live-render", None);
            }
        }
    }

    /// Capture the actual failed owner even when queued successors forbid
    /// retirement. This only selects a host cursor and reads retained state;
    /// it must not acknowledge recovery or release any firmware-owned object.
    pub(crate) fn report_queued_render_failure(&mut self, slot: u8) -> Result {
        self.render_slots.allocated(slot)?;
        self.report_render_wait_timeout(slot, "queued-failure");
        self.log_selected_render_recovery_pre_ack(slot, "queued-failure", None);
        Ok(())
    }

    /// `log_selected_render_recovery_pre_ack`'s snapshot and report half,
    /// in its own cold frame; returns the completion-dump targets.
    #[cold]
    #[inline(never)]
    fn log_selected_render_recovery_report(&mut self, pair_slot: u8, label: &'static str, parent_qid: Option<u8>)
        -> [Option<(u8, u64)>; 2]
    {
        let progress = self.resources.firmware_gpu_progress_snapshot();
        dev_info!(self.resources.dev().as_ref(),
            "G17P global GPU progress[{}]: (recovery-epoch,counter)={:x?}; not per-job completion\n",
            label, progress);
        let mut next_entries = [None; 2];
        // v97d: the completed entry of each engine, the same-queue baseline
        // for the stalled entry after it.
        let mut completed_entries: [Option<(u64, usize)>; 2] = [None; 2];
        let render_queues = self.render_slots.get(pair_slot).sksm.as_ref().map(|queues| {
            [
                (
                    queues.tiling.queue_id as usize,
                    queues.tiling.producer.previous_timestamp,
                    queues.tiling.producer.geometry.entry_stride() as usize,
                    queues.tiling.producer.geometry.stamp_count(),
                ),
                (
                    queues.fragment.queue_id as usize,
                    queues.fragment.producer.previous_timestamp,
                    queues.fragment.producer.geometry.entry_stride() as usize,
                    queues.fragment.producer.geometry.stamp_count(),
                ),
            ]
        });
        if let Some([(tiling_qid, _, _, _), (fragment_qid, _, _, _)]) = render_queues {
            let tiling_last = self.g17p_last_submitted_hw_timestamp(tiling_qid);
            let fragment_last = self.g17p_last_submitted_hw_timestamp(fragment_qid);
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P render recovery pre-ack[{}]: last-submitted TA(qid {})={:?} 3D(qid {})={:?}\n",
                label,
                tiling_qid,
                tiling_last,
                fragment_qid,
                fragment_last,
            );
            // Fault-only scheduler DATA.  The monitor/recovery notification is
            // already present when this runs, so these bounded reads cannot
            // perturb the causal execution window.  In particular, distinguish
            // a dependency that names an incomplete stamp from a queue that was
            // parked despite its dependency record reaching the requested value.
            if module_parameters::g17p_fault_report.value() & 4 != 0 {
                let read = |link: u64| -> Result<u64> {
                    pgtable::LiveFirmwareU64Probe::new(g17p_gfx_link_to_physical(link))
                        .and_then(|probe| probe.observe_mask(u64::MAX, 1))
                        .map(|sample| sample.first)
                };
                dev_err!(self.resources.dev().as_ref(),
                    "G17P render admission[{}]: DM1-active={:x?}\n", label,
                    [read(G17P_DM_ACTIVE_BITMAP_LINK_BASE + 0x10),
                     read(G17P_DM_ACTIVE_BITMAP_LINK_BASE + 0x18)]);
                // v97d: complete B1 scheduler masks and both engines' park
                // records for this pair (the render path previously logged
                // them only for compute timeouts).
                let _ = self.log_b1_scheduler_state(label,
                    &[tiling_qid as u8, fragment_qid as u8], Some(fragment_qid as u8));
                if label == "blocked-compute-parent" {
                    // Native DM1 monitor 6b44..6bcc caches c078 separately
                    // from its shared 24-byte record. These are historical
                    // firmware samples, NOT proof of current GPU residency.
                    // Double-read the key to expose an attribution race.
                    for slot in 0..16usize {
                        let key_address = 0xffff_fc00_0017_74b8 + slot as u64 * 8;
                        let before = read(key_address);
                        let monitor = self.resources.firmware_dm1_slot_snapshot(slot);
                        let calls = read(0xffff_fc00_0017_7538 + (slot as u64 / 2) * 8)
                            .map(|word| (word >> ((slot % 2) * 32)) as u32);
                        let after = read(key_address);
                        dev_err!(self.resources.dev().as_ref(),
                            "G17P timeout DM1 cached sample: slot={} key-before/after={:x?}/{:x?} monitor={:x?} invocations={:?} (historical DRAM, not live residency)\n",
                            slot, before, after, monitor.map(|sample| sample.raw), calls);
                    }
                }
                for (dm, qid) in [tiling_qid as u64, fragment_qid as u64].into_iter().enumerate() {
                    // B1 e288 restores this cached quartet to 21008/21010/
                    // 21020/211e0. These are DRAM snapshots, not live MMIO.
                    let cached: [Result<u64>; 5] = core::array::from_fn(|field|
                        read(G17P_QUEUE_FLAG_LINK_BASE + qid * 0x28 + 0x20 + field as u64 * 8));
                    let installed: [Result<u64>; 5] = core::array::from_fn(|field|
                        read(G17P_QUEUE_STATE_LINK_BASE + qid * 0x28 + field as u64 * 8));
                    dev_err!(self.resources.dev().as_ref(),
                        "G17P render admission[{}]: qid={} installed-qstate={:x?} cached-ksm(registers,flags)={:x?} packed-completion={:x?}\n",
                        label, qid, installed, cached,
                        read(0xffff_fc00_0012_5df0 + qid * 8));
                    let dependency = G17P_DEPENDENCY_RECORD_LINK_BASE
                        + qid * G17P_DEPENDENCY_RECORD_STRIDE;
                    let park = G17P_KSM_PARK_RECORD_LINK_BASE + qid * 0x28;
                    // B1 d194..d1ac stores the status-2 timestamp and sets
                    // this DM/QID bit. cfb0..cfd4 clears only the bit on
                    // matching completion, leaving the old timestamp behind.
                    // +0x10 alone therefore says nothing about current wait.
                    let status2_member = read(G17P_DEPENDENCY_RECORD_LINK_BASE
                        + 0xc00 + dm as u64 * 16 + (qid / 64) * 8)
                        .map(|bits| bits & (1u64 << (qid % 64)) != 0);
                    dev_err!(
                        self.resources.dev().as_ref(),
                        "G17P render fault-only scheduler[{}]: qid={} dep(valid,completed,status2-stamp)={:x?} status2-member={:?} park(hw-watermark,counters)={:x?} owner={:x?} cached-register-mask={:x?} qstate={:x?} qflag={:x?}\n",
                        label,
                        qid,
                        [read(dependency), read(dependency + 8), read(dependency + 16)],
                        status2_member,
                        [read(park + 0x400), read(park + 0x418)],
                        read(G17P_QID_OWNER_LINK_BASE + qid * 8),
                        read(G17P_READY_MASK_LINK_BASE),
                        [
                            read(G17P_QUEUE_STATE_LINK_BASE + qid * 0x28),
                            read(G17P_QUEUE_STATE_LINK_BASE + qid * 0x28 + 8),
                        ],
                        read(G17P_QUEUE_FLAG_LINK_BASE + qid * 0x28 + 0x40),
                    );
                }
                if let Some(queues) = render_queues {
                    for (index, (qid, latest, stride, count)) in queues.into_iter().enumerate() {
                        let dependency = G17P_DEPENDENCY_RECORD_LINK_BASE
                            + qid as u64 * G17P_DEPENDENCY_RECORD_STRIDE;
                        if let (Ok(1), Ok(completed)) = (read(dependency), read(dependency + 8)) {
                            // Use the queue's runtime stamp geometry, including
                            // wrap. This is a candidate after the completed
                            // frontier, not proof that the entry is executing.
                            if completed != latest {
                                if let Ok(previous) = g17_submission::prepare_kick_submission(
                                    completed, qid as u8, count, stride as u32,
                                ) {
                                    let next = previous.next_timestamp;
                                    next_entries[index] = Some((next, stride));
                                    completed_entries[index] = Some((completed, stride));
                                    dev_info!(self.resources.dev().as_ref(),
                                        "G17P render fault frontier[{}]: qid={} completed={:#x} next={:#x} latest={:#x}\n",
                                        label, qid, completed, next, latest);
                                }
                            }
                        }
                    }
                }
            }
        }
        {
            let blamed_qid = parent_qid.or_else(|| self.pending_primary_recovery().and_then(|(_, _, qid)| qid));
            let fault_target = render_queues.and_then(|queues| {
                queues.iter().position(|(qid, _, _, _)| Some(*qid as u8) == blamed_qid)
            }).and_then(|engine| next_entries[engine].map(|(timestamp, _)| (engine, timestamp)));
            // Fault-only DRAM census: identify the pool supplying a gUPM VA
            // without changing queue ownership or touching powered registers.
            // The primary snapshot already captured this global table.
            if module_parameters::g17p_fault_report.value() & 4 != 0
                && label != "peer-live-render" {
                for id in 0..=255u8 {
                    if let Ok(words) = self.resources.snapshot_compute_uma(id) {
                        if words != [0; 4] {
                            dev_err!(self.resources.dev().as_ref(),
                                "G17P fault UMA table[{}]: id={} words={:x?}\n", label, id, words);
                        }
                    }
                }
            }
            let dev = self.resources.dev();
            let slot = self.render_slots.get_mut(pair_slot);
            if let Some(storage) = slot.storage.as_deref_mut() {
                if module_parameters::g17p_fault_report.value() & 4 != 0 {
                    let exact = fault_target.and_then(|(engine, timestamp)| {
                        Iterator::chain(slot.tracker.iter(), slot.queued_tracker.iter()).find(|tracker| {
                            tracker.native_timestamps.is_some_and(|values| values[engine] == timestamp)
                        })
                    });
                    if let Some(tracker) = exact.or(slot.tracker.as_ref()) {
                        dev_err!(dev.as_ref(), "G17P fault work state: source={} ordinal={} full-word={:#x} low-id={}\n",
                            if exact.is_some() { "hardware-frontier" } else { "oldest-host" },
                            tracker.ordinal, tracker.command_buffer_state_word,
                            tracker.command_buffer_state_word & 63);
                        if let Err(error) = storage.log_fault_command(&dev, tracker.ordinal) {
                            dev_warn!(dev.as_ref(),
                                "G17P fault command: ordinal={} snapshot failed ({:?})\n",
                                tracker.ordinal, error);
                        }
                    }
                    // TA may have advanced beyond the oldest pending FRG.
                    // Preserve both exact frontiers; the selected source's
                    // private buffers cannot describe its blocked peer.
                    let primary_ordinal = exact.or(slot.tracker.as_ref()).map(|t| t.ordinal);
                    for (engine, frontier) in next_entries.iter().enumerate() {
                        let Some((timestamp, _)) = frontier else { continue };
                        let peer = Iterator::chain(slot.tracker.iter(), slot.queued_tracker.iter())
                            .find(|tracker| tracker.native_timestamps
                                .is_some_and(|values| values[engine] == *timestamp));
                        if let Some(peer) = peer {
                            if Some(peer.ordinal) == primary_ordinal { continue; }
                            dev_err!(dev.as_ref(),
                                "G17P fault peer frontier: engine={} timestamp={:#x} ordinal={} full-word={:#x}\n",
                                engine, timestamp, peer.ordinal, peer.command_buffer_state_word);
                            if let Err(error) = storage.log_fault_command(&dev, peer.ordinal) {
                                dev_warn!(dev.as_ref(),
                                    "G17P fault peer command: ordinal={} snapshot failed ({:?})\n",
                                    peer.ordinal, error);
                            }
                        }
                    }
                }
                // Capture the actual firmware FList binding, not just the
                // host's UMA descriptor and retained backing. This is the
                // same bounded, explicitly enabled fault-only read as CL.
                if module_parameters::g17p_fault_report.value() & 4 != 0 {
                    if let Ok(header) = storage.tiling_config_update_header() {
                        let pool = u64::from(header.flist_id);
                        let row = g17p_read_binding_row::<29>(
                            G17P_FLIST_ROW_LINK_BASE, header.flist_id.into(), G17P_FLIST_ROW_STRIDE);
                        dev_err!(dev.as_ref(),
                            "G17P fault-only render FList binding[{}]: qid={} ctx={} pool={} expected-pointer={:#x} generation={:#x} qwords={:x?}\n",
                            label, header.queue_id, header.context_id, pool,
                            header.flist_pointer, header.flist_generation, row);
                        let (pointer, token, id) = storage.parameter_buffer_binding();
                        let hwpb = g17p_read_binding_row::<10>(
                            G17P_HWPB_ROW_LINK_BASE, id, G17P_HWPB_ROW_STRIDE);
                        dev_err!(dev.as_ref(),
                            "G17P fault-only render HWPB binding[{}]: ta-qid={} ctx={} id={} expected-pointer={:#x} token={:#x} qwords={:x?}\n",
                            label, header.queue_id, header.context_id, id, pointer, token, hwpb);
                    }
                }
                match (
                    storage.tiling_config_update_header(),
                    storage.fragment_config_update_header(),
                ) {
                    (Ok(tiling), Ok(fragment)) => dev_err!(
                        dev.as_ref(),
                        "G17P render fault identity[{}]: ordinal={} TA(qid={},ctx={},flist={:#x}/id{}/generation{:#x},qos={},pb={:#x}/token{:#x}/id{}) 3D(qid={},ctx={},flist={:#x}/id{}/generation{:#x},qos={})\n",
                        label,
                        slot.submission_ordinal,
                        tiling.queue_id,
                        tiling.context_id,
                        tiling.flist_pointer,
                        tiling.flist_id,
                        tiling.flist_generation,
                        tiling.field_56,
                        tiling.parameter_buffer_object,
                        tiling.parameter_buffer_token,
                        tiling.parameter_buffer_id,
                        fragment.queue_id,
                        fragment.context_id,
                        fragment.flist_pointer,
                        fragment.flist_id,
                        fragment.flist_generation,
                        fragment.field_56,
                    ),
                    (tiling, fragment) => dev_warn!(
                        dev.as_ref(),
                        "G17P render fault identity[{}]: tag15 decode failed TA={:?} 3D={:?}\n",
                        label,
                        tiling.err(),
                        fragment.err(),
                    ),
                }
                match storage.snapshot(slot.submission_ordinal) {
                    Ok(state) => dev_info!(
                        dev.as_ref(),
                        "G17P render recovery pre-ack[{}]: tiling done/read/write {}/{}/{} fragment {}/{}/{} job-list-empty {} tiling-ts [{:#x},{:#x}] fragment-ts [{:#x},{:#x}]\n",
                        label,
                        state.tiling.done,
                        state.tiling.read,
                        state.tiling.write,
                        state.fragment.done,
                        state.fragment.read,
                        state.fragment.write,
                        state.job_list_empty,
                        state.tiling_timestamps[0],
                        state.tiling_timestamps[1],
                        state.fragment_timestamps[0],
                        state.fragment_timestamps[1],
                    ),
                    Err(error) => dev_warn!(
                        dev.as_ref(),
                        "G17P render recovery pre-ack[{}]: queue snapshot failed ({:?})\n",
                        label,
                        error
                    ),
                }
                if module_parameters::g17p_fault_report.value() & 4 != 0 {
                    if let Some([(_, ta_stamp, ta_stride, _), (_, fragment_stamp, fragment_stride, _)]) =
                        render_queues
                    {
                        let ta_offset = usize::try_from(
                            ta_stamp & g17_submission::KICK_STAMP_INDEX_MASK,
                        )
                        .unwrap_or(0)
                        .saturating_mul(ta_stride);
                        let fragment_offset = usize::try_from(
                            fragment_stamp & g17_submission::KICK_STAMP_INDEX_MASK,
                        )
                        .unwrap_or(0)
                        .saturating_mul(fragment_stride);
                        storage.log_sksm_entry(&dev, true, label, ta_offset);
                        storage.log_sksm_entry(
                            &dev,
                            false,
                            label,
                            fragment_offset,
                        );
                    }
                    for (index, entry) in next_entries.into_iter().enumerate() {
                        if let Some((timestamp, stride)) = entry {
                            storage.log_sksm_entry(&dev, index == 0, "after-completed",
                                (timestamp & g17_submission::KICK_STAMP_INDEX_MASK) as usize * stride);
                        }
                    }
                    // v97d: complete entries for the stalled (next) and the
                    // completed (baseline) stamp of both engines.
                    for (index, entry) in next_entries.into_iter().enumerate() {
                        if let Some((timestamp, stride)) = entry {
                            storage.log_sksm_entry_full(&dev, index == 0, "stalled-next", timestamp,
                                (timestamp & g17_submission::KICK_STAMP_INDEX_MASK) as usize * stride);
                        }
                    }
                    for (index, entry) in completed_entries.into_iter().enumerate() {
                        if let Some((timestamp, stride)) = entry {
                            storage.log_sksm_entry_full(&dev, index == 0, "completed-baseline", timestamp,
                                (timestamp & g17_submission::KICK_STAMP_INDEX_MASK) as usize * stride);
                        }
                    }
                }
            }
        }
        core::array::from_fn(|index| render_queues.and_then(|queues|
            next_entries[index].map(|(stamp, _)| (queues[index].0 as u8, stamp))))
    }

    fn log_selected_render_recovery_pre_ack(&mut self, pair_slot: u8, label: &'static str, parent_qid: Option<u8>) {
        // Report half first, in its own frame; the nested page/entry dumps
        // below then run without its diagnostic locals on the stack.
        let targets = self.log_selected_render_recovery_report(pair_slot, label, parent_qid);
        self.log_render_status_pages(pair_slot, label);
        self.log_render_sksm_entries(pair_slot, label);
        if g17p_native_fragment_first_mode(
            crate::g17_config::RENDER_NATIVE_DOORBELLS,
        ) {
            self.log_render_outer_channels(label);
        }
        if let Err(error) = self.dump_completion_records_targets(label, targets) {
            let dev = self.resources.dev();
            dev_warn!(
                dev.as_ref(),
                "G17P render recovery pre-ack[{}]: completion dump failed ({:?})\n",
                label,
                error
            );
        }
    }

    /// DRAM-only TA/3D outer-channel state. The current slot record pointer and
    /// queue-id byte prove whether firmware consumed the Fragment slot after
    /// its dedicated doorbell without touching the SGX MMIO aperture.
    pub(crate) fn log_render_outer_channels(&mut self, label: &'static str) {
        let snapshots = match self.resources.snapshot_all_primary_work_channels() {
            Ok(snapshots) => snapshots,
            Err(error) => {
                dev_warn!(
                    self.resources.dev().as_ref(),
                    "G17P render outer[{}]: snapshot failed ({:?})\n",
                    label,
                    error
                );
                return;
            }
        };
        // Log EVERY work channel, not the first two. The userspace render's
        // tables are TA_2/3D_2 (indices 6/7) and compute publishes on CL_2 (8).
        // Native's distinct system bring-up queues occupy TA_0/3D_0.
        for snapshot in snapshots.into_iter() {
            let name = if snapshot.table_index
                == g17_submission::G17P_PARTIAL_OPENING_TILING_CHANNEL_TABLE_INDEX
            {
                "TA"
            } else if snapshot.table_index
                == g17_submission::G17P_PARTIAL_OPENING_FRAGMENT_CHANNEL_TABLE_INDEX
            {
                "3D"
            } else if snapshot.table_index
                == g17_submission::G17P_COMPUTE_CHANNEL_TABLE_INDEX
            {
                "CL"
            } else {
                "--"
            };
            if snapshot.cursors == [0, 0, 0] && snapshot.current_slot_queue == 0 {
                continue;
            }
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P render outer[{}]: {} table={} cursors(actual,coherency,producer)={:?} current-slot={} record={:#x} kind={} queue-byte={}\n",
                label,
                name,
                snapshot.table_index,
                snapshot.cursors,
                snapshot.current_slot_index,
                snapshot.current_slot_queue,
                snapshot.current_slot_kind,
                snapshot.current_slot_queue_id,
            );
        }
    }

    /// In native pre-ack mode, keep TA wholly unpublished until the primary
    /// firmware has synchronously drained the already-published Fragment outer
    /// group. The outer producer is host-owned and must remain at the expected
    /// value; only the firmware-owned actual consumer gates success. The middle
    /// coherency cursor is diagnostic because firmware can legally update it
    /// independently of the completed synchronous drain.
    pub(crate) fn wait_native_render_fragment_outer_consumed(
        &mut self,
        expected: u32,
    ) -> Result {
        const POLLS: usize = 100;
        const POLL_US: i64 = 100;

        if !g17p_native_fragment_preack_mode(
            crate::g17_config::RENDER_NATIVE_DOORBELLS,
        ) {
            return Err(EINVAL);
        }

        let mut last = [0; 3];
        for poll in 0..=POLLS {
            fence(Ordering::Acquire);
            last = self.resources.partial_opening_outer_counters()?[0];
            if last[2] != expected {
                dev_err!(
                    self.resources.dev().as_ref(),
                    "G17P render native pre-ack: Fragment producer drifted, expected {} cursors(actual,coherency,producer)={:?}\n",
                    expected,
                    last
                );
                return Err(EINVAL);
            }
            if g17p_native_fragment_outer_consumed(last, expected) {
                dev_info!(
                    self.resources.dev().as_ref(),
                    "G17P render native pre-ack: Fragment outer consumed after {} us, cursors(actual,coherency,producer)={:?}\n",
                    poll * POLL_US as usize,
                    last
                );
                return Ok(());
            }
            if poll != POLLS {
                fsleep(Delta::from_micros(POLL_US));
            }
        }

        dev_err!(
            self.resources.dev().as_ref(),
            "G17P render native pre-ack: Fragment outer was not consumed within {} us, expected {} cursors(actual,coherency,producer)={:?}; TA remains unpublished\n",
            POLLS * POLL_US as usize,
            expected,
            last
        );
        Err(ETIMEDOUT)
    }

    /// Observe the TA-owned execution timestamps for the opt-in split render
    /// launch. This is DRAM-only and does not advance either completion
    /// tracker: the ordinary paired wait still owns final retirement after the
    /// deferred fragment producer is published.
    pub(crate) fn split_render_tiling_completion(&mut self, pair_slot: u8) -> Result<Option<[u64; 2]>> {
        if crate::g17_config::RENDER_SPLIT_LAUNCH == 0
            || self.render_slots.get(pair_slot).tracker.is_none()
        {
            return Err(EINVAL);
        }
        fence(Ordering::Acquire);
        let slot = self.render_slots.get_mut(pair_slot);
        let state = slot.storage.as_deref_mut().ok_or(ENODEV)?
            .snapshot(slot.submission_ordinal)?;
        if state.tiling_timestamps.iter().all(|value| *value != 0) {
            Ok(Some(state.tiling_timestamps))
        } else {
            Ok(None)
        }
    }

    /// Read both engine timestamp pairs without imposing the split-launch
    /// policy gate. This is used only by the post-doorbell KSM diagnostic.
    pub(crate) fn render_engine_timestamps(&mut self, pair_slot: u8) -> Result<[[u64; 2]; 2]> {
        if self.render_slots.get(pair_slot).tracker.is_none() {
            return Err(EINVAL);
        }
        fence(Ordering::Acquire);
        let slot = self.render_slots.get_mut(pair_slot);
        let state = slot.storage.as_deref_mut().ok_or(ENODEV)?
            .snapshot(slot.submission_ordinal)?;
        Ok([state.tiling_timestamps, state.fragment_timestamps])
    }

    /// Publish the complete fragment group retained by a split launch after
    /// [`Self::split_render_tiling_completion`] has proved TA execution.
    /// Ordering is strict: SKSM entry, tag-14/AddKicks and inner producer, then
    /// outer slot and producer. The second doorbell is sent by the caller only
    /// after this returns.
    pub(crate) fn publish_split_render_fragment_group(
        &mut self, pair_slot: u8,
        dev: &AsahiDevice,
        _registers: &regs::Resources,
        plan: &g17_submission::G17PPartialOpeningOuterSlotPlan,
    ) -> Result {
        if crate::g17_config::RENDER_SPLIT_LAUNCH == 0
            || self.render_slots.get(pair_slot).tracker.is_none()
            || self.render_slots.get(pair_slot).storage.is_none()
            || self.render_slots.get(pair_slot).bind.is_none()
        {
            return Err(EINVAL);
        }
        let (ta_stamp, ta_queue_id) = {
            let queues = self.render_slots.get(pair_slot).sksm.as_ref().ok_or(ENODEV)?;
            if !queues.tiling.producer.previous_valid {
                return Err(EINVAL);
            }
            (
                queues.tiling.producer.previous_timestamp,
                queues.tiling.queue_id,
            )
        };
        let barrier = [g17_submission::g17p_render_barrier_from_timestamp(
            ta_stamp, ta_queue_id,
        ).map_err(|_| EINVAL)?];
        let event_word = barrier[0].event_word;
        let suppress_barrier =
            crate::g17_config::RENDER_NO_BARRIER != 0;
        let barrier_slice: &[g17_submission::G17PClBarrierDependency] =
            if suppress_barrier { &[] } else { &barrier };
        if suppress_barrier {
            dev_info!(
                dev.as_ref(),
                "G17P render split-launch: DIAGNOSTIC barrier dependency suppressed (event_word {:#x} qid {})\n",
                event_word,
                ta_queue_id
            );
        }
        let fragment_queue = self.render_slots.get(pair_slot)
            .storage
            .as_deref()
            .ok_or(ENODEV)?
            .queue_gpu_vas()
            .0;
        let fragment_header = self.render_slots.get_mut(pair_slot)
            .storage
            .as_deref_mut()
            .ok_or(ENODEV)?
            .fragment_config_update_header()
            .map_err(|error| Self::render_stage_fail("split-decode-fragment-tag15", error))?;
        let fragment_install = self
            .validate_render_firmware_install(pair_slot, false, fragment_header.install)
            .map_err(|error| Self::render_stage_fail("split-validate-fragment-install", error))?;
        let fragment_stamp = self
            .publish_render_sksm_group(pair_slot, dev, false, fragment_queue, barrier_slice, None)
            .map_err(|error| Self::render_stage_fail("split-sksm-publish-fragment", error))?;
        let slot = self.render_slots.get_mut(pair_slot);
        slot.storage
            .as_deref_mut()
            .ok_or(ENODEV)?
            .stage_fragment_kick(slot.submission_ordinal, fragment_stamp)
            .map_err(|error| Self::render_stage_fail("split-fragment-addkicks", error))?;
        fence(Ordering::SeqCst);
        let state = slot.storage.as_deref_mut().ok_or(ENODEV)?
            .snapshot(slot.submission_ordinal)?;
        if state.fragment.write
            != slot.storage.as_deref().ok_or(ENODEV)?
                .current_render_item_windows(slot.submission_ordinal)?[1].target()
        {
            return Err(Self::render_stage_fail("split-fragment-inner-prefix", EINVAL));
        }
        self.resources
            .publish_deferred_partial_opening_fragment(plan)
            .map_err(|error| Self::render_stage_fail("split-fragment-outer", error))?;
        self.commit_render_firmware_install(pair_slot, false, fragment_install);
        Ok(())
    }

    /// Complete the TA group retained by native render chronology modes 2/3.
    /// The caller has already sent the Fragment/priority-2 doorbell. Every
    /// fallible byte construction/VMap acquisition happens before the first TA
    /// inner producer is touched, then publication is strictly:
    ///
    /// tag-15 prefix -> SKSM entry -> tag-14/tag-16 inner producer ->
    /// outer producer -> late Pool-A/shared-inner transition.
    ///
    /// The caller may send the Vertex/priority-2 doorbell only after success.
    pub(crate) fn publish_native_render_tiling_group(
        &mut self, pair_slot: u8,
        dev: &AsahiDevice,
        _registers: &regs::Resources,
        plan: &g17_submission::G17PPartialOpeningOuterSlotPlan,
    ) -> Result {
        if !g17p_native_fragment_first_mode(
            crate::g17_config::RENDER_NATIVE_DOORBELLS,
        )
            || crate::g17_config::RENDER_SPLIT_LAUNCH != 0
            || self.render_slots.get(pair_slot).tracker.is_none()
            || self.render_slots.get(pair_slot).storage.is_none()
            || self.render_slots.get(pair_slot).bind.is_none()
            || self.render_slots.get(pair_slot).sksm.is_none()
        {
            return Err(EINVAL);
        }
        let ordinal = self.render_slots.get(pair_slot).submission_ordinal;
        let (tiling_queue, expected_stamp) = {
            let storage = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?;
            let queues = self.render_slots.get(pair_slot).sksm.as_ref().ok_or(ENODEV)?;
            (
                storage.queue_gpu_vas().1,
                queues.tiling.producer.current_timestamp,
            )
        };

        // Complete every fallible preparation before publishing TA prefix 2.
        let late_tiling = self.render_slots.get_mut(pair_slot)
            .storage
            .as_deref_mut()
            .ok_or(ENODEV)?
            .prepare_late_tiling_publication_with_stamp(
                ordinal,
                expected_stamp,
            )
            .map_err(|error| Self::render_stage_fail("native-prepare-tiling", error))?;
        let mut order = G17PNativeRenderPublishOrder::after_fragment_doorbell();

        let tiling_header = late_tiling
            .config_update_header()
            .ok_or_else(|| Self::render_stage_fail("native-decode-tiling-tag15", EINVAL))?;
        let tiling_install = self
            .validate_render_firmware_install(pair_slot, true, tiling_header.install)
            .map_err(|error| Self::render_stage_fail("native-validate-tiling-install", error))?;
        dev_info!(
            dev.as_ref(),
            "G17P render native TA tag15: selector={:#x} entry=[{:#x},{:#x}] qid={} install={} dm={} graphics={} context={} flist=[{:#x},id={},generation={:#x}] scheduler={:#x} field56={} pb=[{:#x},{:#x},id={},bit={}]\n",
            tiling_header.selector,
            tiling_header.entry_low,
            tiling_header.entry_high,
            tiling_header.queue_id,
            tiling_header.install,
            tiling_header.data_master,
            tiling_header.scratch_seed_valid,
            tiling_header.context_id,
            tiling_header.flist_pointer,
            tiling_header.flist_id,
            tiling_header.flist_generation,
            tiling_header.scheduler_state,
            tiling_header.field_56,
            tiling_header.parameter_buffer_object,
            tiling_header.parameter_buffer_token,
            tiling_header.parameter_buffer_id,
            tiling_header.parameter_buffer_bit,
        );
        let tiling_qos = self
            .resources
            .publish_render_qos_queue_record(tiling_header)
            .map_err(|error| Self::render_stage_fail("native-publish-tiling-qos", error))?;
        self.validate_render_queue_consistency(pair_slot, dev, true, tiling_header, tiling_qos.record, None)
            .map_err(|error| {
                Self::render_stage_fail("native-validate-tiling-qid-consistency", error)
            }).or_else(|error| {
                self.resources.cancel_qos_publication(tiling_qos)?;
                Err(error)
            })?;
        if crate::util::g17p_verbose() {
            dev_info!(
                dev.as_ref(),
                "G17P render QoS: delayed native TA qid {} record {:?}\n",
                tiling_header.queue_id,
                tiling_qos.record,
            );
        }

        // Once the prefix is exposed, retain this count and its lease on
        // every error; recovery must prove firmware has stopped before reuse.
        let flist_submission = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?
            .prepare_flist_submitted_operation(1)?;
        self.render_slots.get(pair_slot).tracker.as_ref().ok_or(EIO)?
            .work_state.as_ref().ok_or(EIO)?.add_submitted_kicks(1)?;
        flist_submission.commit();
        late_tiling.publish_native_prefix();
        order
            .advance(G17PNativeRenderPublishPhase::TilingPrefix)
            .map_err(|_| Self::render_stage_fail("native-order-prefix", EINVAL))?;
        fence(Ordering::SeqCst);

        let tiling_stamp = self
            .publish_render_sksm_group(pair_slot, dev, true, tiling_queue, &[], None)
            .map_err(|error| Self::render_stage_fail("native-sksm-publish-tiling", error))?;
        if tiling_stamp != expected_stamp {
            return Err(Self::render_stage_fail("native-tiling-stamp-mismatch", EINVAL));
        }
        order
            .advance(G17PNativeRenderPublishPhase::TilingSksm)
            .map_err(|_| Self::render_stage_fail("native-order-sksm", EINVAL))?;

        late_tiling.publish_native_kick();
        order
            .advance(G17PNativeRenderPublishPhase::TilingKick)
            .map_err(|_| Self::render_stage_fail("native-order-kick", EINVAL))?;
        fence(Ordering::SeqCst);
        let slot = self.render_slots.get_mut(pair_slot);
        let state = slot.storage.as_deref_mut().ok_or(ENODEV)?
            .snapshot(slot.submission_ordinal)?;
        let published_prefix = slot.storage.as_deref().ok_or(ENODEV)?
            .current_render_item_windows(ordinal)?[0].target();
        if state.tiling.write != published_prefix {
            return Err(Self::render_stage_fail("native-tiling-inner-prefix", EINVAL));
        }

        self.resources
            .publish_deferred_partial_opening_tiling(plan)
            .map_err(|error| Self::render_stage_fail("native-tiling-outer", error))?;
        order
            .advance(G17PNativeRenderPublishPhase::TilingOuter)
            .map_err(|_| Self::render_stage_fail("native-order-outer", EINVAL))?;
        fence(Ordering::Acquire);
        let outer = self.resources.partial_opening_outer_counters()?;
        if !g17p_native_tiling_fully_visible(
            state.tiling.write,
            published_prefix,
            outer[1][2],
            plan.next_producer,
        ) {
            return Err(Self::render_stage_fail("native-complete-ta-visibility", EINVAL));
        }

        late_tiling.publish_native_pool_transition();
        order
            .advance(G17PNativeRenderPublishPhase::PoolTransition)
            .map_err(|_| Self::render_stage_fail("native-order-pool", EINVAL))?;
        fence(Ordering::SeqCst);
        self.commit_render_firmware_install(pair_slot, true, tiling_install);
        Ok(())
    }

    /// One iteration of the render completion poll.
    ///
    /// Split out of `wait_translated_render_completion` so the runtime can
    /// interleave `service_firmware_events_inline` between polls. The compute
    /// submit does exactly that, and its poll loop carries the reason:
    /// "The submit path holds the runtime mutex for the whole wait, which is
    /// why this drains here rather than from an RTKit handler or a workqueue."
    /// Render's wait had no such interleave, so a firmware restart request
    /// raised during the wait stayed unread until the wait gave up.
    pub(crate) fn poll_translated_render_completion(
        &mut self, pair_slot: u8,
    ) -> Result<G17PRenderPollOutcome> {
        self.commit_render_observation(pair_slot, None)
    }

    /// A supplied observation was read with this pair exclusively borrowed.
    /// The caller returns its graph and validates the epoch under Runtime
    /// before calling this commit, without dropping Runtime between them.
    pub(crate) fn commit_render_observation(
        &mut self, pair_slot: u8,
        observed: Option<(g17_resources::G17PUserRenderQueueState,
            g17_completion::PairedRenderCompletion)>,
    ) -> Result<G17PRenderPollOutcome> {
        if self.render_slots.get(pair_slot).preparing { return Ok(G17PRenderPollOutcome::Retry); }
        if self.render_flist_owners[self.render_slots.get(pair_slot).flist_index].as_ref().is_some_and(|owner|
            owner.state == G17PBootstrapUmaRequestState::Submitted) {
            return Ok(G17PRenderPollOutcome::Retry);
        }
        {
            fence(Ordering::Acquire);
            let tracker = self.render_slots.get(pair_slot).tracker.as_ref().ok_or(ENODEV)?;
            let ordinal = tracker.ordinal;
            let (mut state, mut completion) = if let Some(observed) = observed {
                observed
            } else {
                let mut state = self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?
                    .snapshot(ordinal)?;
                // B1 unlinks EventControl only after its completed count reaches
                // its own submitted-count slot (168d0, KTrace 0x10b/0x10d).
                // Queue retirement and timestamps do not authorize host writes
                // to this firmware-owned list. Keep polling until it is empty.
                let tracker = self.render_slots.get(pair_slot).tracker.as_ref().ok_or(ENODEV)?;
                let windows = tracker.windows;
                if windows[0].target() != tracker.tiling_prefix || windows[1].target() != tracker.fragment_prefix {
                    dev_err!(
                        self.resources.dev().as_ref(),
                        "G17P render completion: ordinal {} window targets {:?} disagree with tracker TA/3D {}/{}\n",
                        ordinal,
                        windows.map(|window| window.target()),
                        tracker.tiling_prefix,
                        tracker.fragment_prefix,
                    );
                    return Ok(G17PRenderPollOutcome::Retry);
                }
                let tiling_previous = tracker.tiling_previous;
                let fragment_previous = tracker.fragment_previous;
                let execution = |state: &g17_resources::G17PUserRenderQueueState| {
                    g17_completion::RenderExecutionState {
                        job_list_empty: state.job_list_empty,
                        tiling_timestamps: state.tiling_timestamps,
                        fragment_timestamps: state.fragment_timestamps,
                    }
                };
                let observe = |state: &g17_resources::G17PUserRenderQueueState| {
                    g17_completion::observe_paired_render_windows_depth(
                        windows,
                        g17_resources::G17P_RENDER_MAX_IN_FLIGHT as u32,
                        tiling_previous,
                        state.tiling,
                        fragment_previous,
                        state.fragment,
                        execution(state),
                    )
                };
                let completion = match observe(&state) {
                    Ok(completion) => completion,
                    Err(first_error) => {
                        // The six producer/consumer words are firmware-owned and
                        // sampled separately.  An event edge can therefore catch
                        // one cache line before the paired line becomes visible.
                        // Retry one immediate DRAM snapshot; this is bounded edge
                        // handling, not a completion poll or a steady-state timer.
                        fence(Ordering::Acquire);
                        let retry = self.render_slots.get_mut(pair_slot)
                            .storage
                            .as_deref_mut()
                            .ok_or(ENODEV)?
                            .snapshot(ordinal)?;
                        match observe(&retry) {
                            Ok(completion) => {
                                state = retry;
                                completion
                            }
                            Err(second_error) => {
                                dev_err!(
                                    self.resources.dev().as_ref(),
                                    "G17P render completion: ordinal {} targets {:?} rejected two snapshots ({:?}/{:?}); previous TA {}/{}/{} 3D {}/{}/{} first TA {}/{}/{} 3D {}/{}/{} retry TA {}/{}/{} 3D {}/{}/{}\n",
                                    ordinal,
                                    windows.map(|window| window.target()),
                                    first_error,
                                    second_error,
                                    tiling_previous.done,
                                    tiling_previous.read,
                                    tiling_previous.write,
                                    fragment_previous.done,
                                    fragment_previous.read,
                                    fragment_previous.write,
                                    state.tiling.done,
                                    state.tiling.read,
                                    state.tiling.write,
                                    state.fragment.done,
                                    state.fragment.read,
                                    state.fragment.write,
                                    retry.tiling.done,
                                    retry.tiling.read,
                                    retry.tiling.write,
                                    retry.fragment.done,
                                    retry.fragment.read,
                                    retry.fragment.write,
                                );
                                return Ok(G17PRenderPollOutcome::Retry);
                            }
                        }
                    }
                };
                (state, completion)
            };
            let tracker = self.render_slots.get_mut(pair_slot).tracker.as_mut().ok_or(ENODEV)?;
            if completion.complete {
                let shared_work_state = tracker.work_state.is_some();
                let event_record = tracker.pool_a_record_index;
                let event_submitted = tracker.pool_a_submitted;
                let hardware_buffer_id = tracker.hardware_buffer_id;
                let command_buffer_state_word = tracker.command_buffer_state_word;
                let context_update = tracker.context_update;
                // Bootstrap still owns its private record. Generic submits
                // share one TSQ node across engines and commands; its aggregate
                // count/unlink state must never gate an individual member.
                if !shared_work_state {
                    let event_control = self.render_slots.get_mut(pair_slot)
                        .storage
                        .as_deref_mut()
                        .ok_or(ENODEV)?
                        .render_event_control_state(event_record, event_submitted)?;
                    if !event_control.unlinked() {
                        if crate::util::g17p_verbose() {
                            dev_info!(
                                self.resources.dev().as_ref(),
                                "G17P render completion: EventControl {} pending submitted={} completed={} membership={:#x}\n",
                                event_control.record_index,
                                event_control.submitted,
                                event_control.completed,
                                event_control.membership,
                            );
                        }
                        return Ok(G17PRenderPollOutcome::Pending);
                    }
                }
                // B1 clears EventControl membership BEFORE GPU cache
                // maintenance. Its fragment stamp A write follows that
                // maintenance (25G83 B1 0x16ea0); queue/list retirement alone
                // therefore cannot authorize a userspace-visible fence.
                // The physical slot retains its ordinal across VM switches.
                // Both engine prefixes must cover this member, including when
                // several pairs are in flight. The event value wraps as on M1/M2;
                // the previous completed value differs even across zero.
                // Require this exact generation, never a stale nonzero stamp.
                let pair = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?.queue_pair();
                let expected_stamp = ordinal
                    .checked_add(1)
                    .map(|value| (value as u32).wrapping_shl(8))
                    .ok_or(EOVERFLOW)?;
                let stamps = self.resources.render_completion_stamps(pair)?;
                let fragment_stamp = stamps[1];
                let visible = |stamp: u32| {
                    let delta = stamp.wrapping_sub(expected_stamp);
                    delta & 0xff == 0 && delta < 0x8000_0000
                };
                if !visible(fragment_stamp) || (shared_work_state && !visible(stamps[0])) {
                    if crate::util::g17p_verbose() {
                        dev_info!(
                            self.resources.dev().as_ref(),
                            "G17P render completion: fragment stamp pending qid={} observed={:#x} expected={:#x}\n",
                            pair.fragment,
                            fragment_stamp,
                            expected_stamp,
                        );
                    }
                    // The post-stamp type-1 notification wakes this job.
                    // Keep its ownership and fence pending without polling.
                    return Ok(G17PRenderPollOutcome::Pending);
                }
                // The earlier queue snapshot can precede the post-maintenance
                // release stores. Refresh only this owned generation's pass
                // pair after render_completion_stamps' acquire barrier, before
                // telemetry, caller/UAPI export, or releasing its backing.
                let pass = self.render_slots.get_mut(pair_slot).storage.as_deref_mut()
                    .ok_or(ENODEV)?.render_pass_timestamps(ordinal)?;
                state.tiling_timestamps = [pass[0], pass[0]];
                state.fragment_timestamps = pass;
                completion.execution.tiling_timestamps = state.tiling_timestamps;
                completion.execution.fragment_timestamps = state.fragment_timestamps;
                // v81 e2e diagnostic (default off): identity of the retired pass.
                if crate::g17_e2e::armed() {
                    let identity = u64::from(pair_slot) << 32 | (ordinal & 0xffff_ffff);
                    crate::g17_e2e::record(crate::g17_e2e::K_OBS_DEVICE, u32::from(pair_slot), identity,
                        [u64::from(pair.tiling) | u64::from(pair.fragment) << 32,
                         u64::from(stamps[0]) | u64::from(stamps[1]) << 32,
                         u64::from(expected_stamp), pass[0], pass[1]]);
                    if crate::g17_e2e::option(crate::g17_e2e::OPT_SEGMENTS) {
                        let segments = self.render_slots.get_mut(pair_slot).storage.as_deref_mut()
                            .and_then(|storage| storage.render_device_segments(ordinal).ok());
                        let values = segments.unwrap_or([0; 6]);
                        crate::g17_e2e::record(crate::g17_e2e::K_SEGMENTS, u32::from(pair_slot), identity,
                            [values[2], values[3], values[4], values[5], u64::from(segments.is_some())]);
                    }
                }
                // Bounded opt-in observation after exact maintenance witnesses,
                // before this generation can be retired or reused. A failed
                // diagnostic read never changes completion or error handling.
                if crate::g17_timing_buffer::enabled() {
                    let native = self.render_slots.get(pair_slot).tracker.as_ref()
                        .and_then(|tracker| tracker.native_timestamps).unwrap_or([0; 2]);
                    let segments = self.render_slots.get_mut(pair_slot).storage.as_deref_mut()
                        .and_then(|storage| storage.render_device_segments(ordinal).ok());
                    let mut words = [0; crate::g17_timing_buffer::WORDS];
                    words[0] = crate::g17_timing_buffer::KIND_RENDER_DEVICE_SEGMENTS;
                    words[1] = u64::from(pair_slot);
                    words[2] = ordinal;
                    if let Some(values) = segments {
                        words[3..5].copy_from_slice(&values[..2]);
                        words[10..14].copy_from_slice(&values[2..]);
                        words[16] = 1;
                    }
                    words[5..7].copy_from_slice(&native);
                    words[7] = u64::from(pair.tiling) | (u64::from(pair.fragment) << 32);
                    words[8] = state.tiling_timestamps[0];
                    words[9] = state.fragment_timestamps[1];
                    words[14] = u64::from(stamps[0]) | (u64::from(stamps[1]) << 32);
                    words[15] = crate::g17_timing_buffer::arch_counter();
                    words[17] = crate::g17_timing_buffer::arch_frequency();
                    crate::g17_timing_buffer::record(words);
                }
                // Publish the exact GPU-written pass envelope while the completed job's
                // VM binding, ordinal and QIDs are still owned.  This is
                // observation only: it neither gates completion nor changes
                // queue state.  Invalid retirement-only pairs are omitted.
                if let Some(interval) =
                    g17_completion::render_gpu_interval(
                        g17_completion::RenderExecutionState {
                            job_list_empty: state.job_list_empty,
                            tiling_timestamps: state.tiling_timestamps,
                            fragment_timestamps: state.fragment_timestamps,
                        },
                    )
                {
                    let (vm_id, root) = self.render_slots.get(pair_slot).bind.as_ref()
                        .map_or((0, 0), |binding| (binding.vm_id(), binding.root()));
                    let fallback_pair = self.render_slots.get(pair_slot).storage
                        .as_deref()
                        .map(g17_resources::G17PUserRenderStorage::queue_pair)
                        .ok_or(EINVAL)?;
                    let (tiling_qid, fragment_qid) = self.render_slots.get(pair_slot).sksm.as_ref()
                        .map_or((fallback_pair.tiling, fallback_pair.fragment), |queues| (
                            u16::from(queues.tiling.queue_id),
                            u16::from(queues.fragment.queue_id),
                        ));
                    if crate::util::g17p_verbose() {
                        dev_info!(
                            self.resources.dev().as_ref(),
                            "G17P render: GPU-INTERVAL ordinal={} vm={} root={:#x} ta-qid={} fragment-qid={} start={} end={}\n",
                            ordinal,
                            vm_id,
                            root,
                            tiling_qid,
                            fragment_qid,
                            interval[0],
                            interval[1],
                        );
                    }
                }
                // Validate and commit the context-retirement epoch before the
                // ordinary occupancy transaction starts mutating allocator
                // state. Every fallible retirement precondition is therefore
                // checked before an HWPB lease or queue producer is changed.
                if context_update {
                    self.complete_render_context_update_retirement(pair_slot, true)?;
                }
                // Native TA/3D complete subtract completed kicks from queue
                // +44. Prepare both results before mutating either queue;
                // timestamps and previous-stamp history must never reset.
                let retired_sksm = self.render_slots.get(pair_slot).sksm.as_ref().map(|queues| {
                    let ta = g17_submission::complete_g17p_cl_kicks(queues.tiling.producer, 1)?;
                    let fragment = g17_submission::complete_g17p_cl_kicks(queues.fragment.producer, 1)?;
                    Ok::<_, g17_submission::SubmissionError>((ta, fragment))
                }).transpose().map_err(|_| EIO)?;
                self.retire_tracked_render_qos(pair_slot)?;
                self.render_hardware_buffer_ids
                    .release(hardware_buffer_id)
                    .map_err(|_| EIO)?;
                self.release_render_command_buffer_state(pair_slot, command_buffer_state_word)?;
                self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?
                    .retire_command_generation(ordinal)?;
                if let (Some(queues), Some((ta, fragment))) = (self.render_slots.get_mut(pair_slot).sksm.as_mut(), retired_sksm) {
                    queues.tiling.producer = ta;
                    queues.fragment.producer = fragment;
                }
                if crate::g17_config::RENDER_USC_FREELIST != 0 {
            let state = self.render_slots.get_mut(pair_slot);
            if state.flist_retired_ordinal.is_some()
                || state.flist_leases.front().map(|lease| lease.0) != Some(ordinal) { return Err(EIO); }
            state.flist_retired_ordinal = Some(ordinal);
        }
        self.render_slots.get_mut(pair_slot).tracker = self.render_slots.get_mut(pair_slot).queued_tracker.take();
                if crate::util::g17p_verbose() {
                    dev_info!(
                        self.resources.dev().as_ref(),
                        "G17P render: released TA HardwareBufferID {} at paired completion\n",
                        hardware_buffer_id,
                    );
                }
                // One control sample per boot, from a render that DID complete.
                //
                // Every field of the `context/qos` line captured at a wedge --
                // slot=255 ctx=255 prio=0 class=0 state=2 bound-slot=255 and an
                // all-zero qid bitmap -- is byte-for-byte what
                // `apply_g17p_cold_opening_channel_control` writes at cold
                // opening (`0x0000_0100_0000_ffff` at +0x00, `0x0002_..` at
                // +0x20, `0x0000_0000_ff00_0000` at +0x30, `fill(0)` elsewhere).
                // So that line proves only that the record is UNTOUCHED, which
                // is a different claim from "the context was lost", and without
                // a healthy sample the two are indistinguishable. If this
                // control prints the same constants, the firmware never writes
                // this record and the diagnostic -- and the comment above it
                // claiming it is "the exact host-safe witness" -- is wrong.
                if !RENDER_CONTEXT_WITNESS_LOGGED.swap(true, Ordering::Relaxed) {
                    self.log_render_context_witness(pair_slot, "healthy-paired-completion");
                }
                return Ok(G17PRenderPollOutcome::Complete(completion));
            }
            tracker.tiling_previous = state.tiling;
            tracker.fragment_previous = state.fragment;
        }
        Ok(G17PRenderPollOutcome::Pending)
    }

    /// Completion masks of any retained render slot, without selecting it.
    /// Event service inspects every pending slot on every firmware event and
    /// must not move the caller's selection to do so.
    pub(crate) fn render_completion_masks_for_slot(&self, slot: u8) -> Result<[u64; 2]> {
        // Queue identity is manager-owned even while a preparer owns storage.
        self.render_slots.allocated(slot)?;
        self.retained_render_queue_pair(slot)?.completion_masks().ok_or(EINVAL)
    }

    pub(crate) fn render_completion_masks(&self, pair_slot: u8) -> Result<[u64; 2]> {
        self.render_slots.get(pair_slot).storage
            .as_deref()
            .ok_or(ENODEV)?
            .queue_pair()
            .completion_masks()
            .ok_or(EINVAL)
    }

    /// Nominal render wait bound, expressed the way it was originally written:
    /// 2000 polls of one millisecond each.
    ///
    /// The waits are now deadline-driven (`g17p_render_wait_ms`, which
    /// defaults to exactly this many milliseconds) because the poll interval
    /// varies. The constant stays so the two cannot drift apart and so the
    /// timeout report still names a poll budget.
    pub(crate) const RENDER_COMPLETION_POLLS: usize = 2000;

    pub(crate) fn wait_translated_render_completion(
        &mut self, pair_slot: u8,
    ) -> Result<g17_completion::PairedRenderCompletion> {
        let mut schedule = crate::g17_perf::PollSchedule::new(
            module_parameters::g17p_render_poll_us.value(),
        );
        let deadline_ns =
            i64::from(module_parameters::g17p_render_wait_ms.value()).saturating_mul(1_000_000);
        let started = kernel::time::Instant::<kernel::time::Monotonic>::now();
        loop {
            match self.poll_translated_render_completion(pair_slot)? {
                G17PRenderPollOutcome::Complete(completion) => return Ok(completion),
                // Re-poll immediately, do not sleep -- but still inside the
                // deadline. Without the old fixed poll count, an outcome that
                // never advances would spin this loop forever.
                G17PRenderPollOutcome::Retry => {
                    if started.elapsed().as_nanos() >= deadline_ns {
                        break;
                    }
                }
                G17PRenderPollOutcome::Pending => {
                    if started.elapsed().as_nanos() >= deadline_ns {
                        break;
                    }
                    fsleep(Delta::from_micros(i64::from(schedule.next_delay_us())));
                }
            }
        }
        self.report_render_wait_timeout(pair_slot, "inline-wait-expired");
        Err(ETIMEDOUT)
    }

    /// Call only after this tracker's paired firmware retirement is proved.
    /// complete_qos_queue_records validates both exact QID/HBID/DM owners before
    /// changing either row. A successful call belongs to this tracker once,
    /// even if a later HWPB/command-generation cleanup step fails and retries.
    fn retire_tracked_render_qos(&mut self, pair_slot: u8) -> Result {
        if !self.render_slots.get(pair_slot).tracker.as_ref().ok_or(ENODEV)?.qos_pending {
            return Ok(());
        }
        let storage = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?;
        let pair = storage.queue_pair();
        let qos_id = u8::try_from(storage.qos_hardware_buffer_id()?).map_err(|_| EINVAL)?;
        self.resources.complete_qos_queue_records(&[
            (pair.tiling as u8, qos_id, 0),
            (pair.fragment as u8, qos_id, 1),
        ])?;
        // The runtime lock keeps this exact tracker selected through the
        // transaction; only the caller may promote its queued successor.
        self.render_slots.get_mut(pair_slot).tracker.as_mut().ok_or(ENODEV)?.qos_pending = false;
        Ok(())
    }

    /// After a render wait has expired: if the firmware has provably retired
    /// this group without executing it, perform the host bookkeeping a paired
    /// completion performs and report `true`; otherwise touch nothing and
    /// report `false`.
    ///
    /// "Provably retired" is the firmware's own word on it: both queue cursors
    /// at this group's published target, the job unlinked from the firmware's
    /// completion list, and no pass timestamp. The tiler stamps the pass start
    /// before any fragment work, so a retired, unlinked group with a zero start
    /// is a group the GPU never ran -- the shape `g17p_zero_duration` names for
    /// compute. The firmware owns nothing of such a group any more, so
    /// quarantining the slot for the rest of the session and pinning its
    /// HardwareBufferID bought nothing; the failing client is still told its
    /// work did not run.
    ///
    /// A group the firmware still holds -- cursors short of the target, or the
    /// job still linked -- is left exactly as the timeout found it.
    ///
    /// `accept_executed` drops ONLY the `observed()` term -- see
    /// `g17p_render_recovery_retire`. Both ownership proofs are retained in
    /// every mode: `completion.retired` (cursors at the published target) and
    /// `job_list_empty` (unlinked from the firmware completion list). Those two
    /// are what make the bookkeeping below safe to perform; `observed()` only
    /// ever described whether the GPU did any work, which does not bear on
    /// whether the firmware still owns the group.
    /// v93r `g17p_recovery_path` bit0: firmware evidence that every render
    /// ever published on `pair_slot` has been consumed. `None` when drained:
    /// both engines' post-maintenance completion stamps cover the pair's last
    /// published ordinal (`submission_ordinal - 1`, i.e. kick timestamp
    /// `submission_ordinal`) and the pair FList's completed-kick counter equals
    /// its submitted counter. A pair nothing was ever published on is drained.
    pub(crate) fn render_slot_firmware_drained(
        &mut self, pair_slot: u8,
    ) -> Result<Option<crate::g17_recovery_path::ReclaimBlock>> {
        use crate::g17_recovery_path::ReclaimBlock;
        let (published, pair, [submitted, completed]) = {
            let slot = self.render_slots.allocated(pair_slot)?;
            if slot.preparing { return Ok(Some(ReclaimBlock::Preparing)); }
            if slot.submission_ordinal == 0 { return Ok(None); }
            let storage = slot.storage.as_deref().ok_or(ENODEV)?;
            (slot.submission_ordinal, storage.queue_pair(), storage.render_flist_completion_counters()?)
        };
        let stamps = self.resources.render_completion_stamps(pair)?;
        if !stamps.into_iter().all(|stamp| g17_compute::completion_stamp_covers(stamp, published)) {
            return Ok(Some(ReclaimBlock::Stamps));
        }
        if submitted != completed {
            return Ok(Some(ReclaimBlock::Flist));
        }
        if self.firmware_recovery_handshake_snapshot()?.state != 0 {
            return Ok(Some(ReclaimBlock::Recovery));
        }
        Ok(None)
    }

    /// v93r: whether `pair_slot` still tracks a published render group.
    pub(crate) fn render_slot_tracked(&self, pair_slot: u8) -> Result<bool> {
        Ok(self.render_slots.allocated(pair_slot)?.tracker.is_some())
    }

    pub(crate) fn retire_abandoned_render_if_firmware_retired(
        &mut self, pair_slot: u8,
        accept_executed: bool,
    ) -> Result<bool> {
        if self.render_slots.get(pair_slot).preparing { return Ok(false); }
        if crate::g17_config::RENDER_NOP_RETIRE == 0 {
            return Ok(false);
        }
        fence(Ordering::Acquire);
        let slot = self.render_slots.get_mut(pair_slot);
        let tracker = slot.tracker.as_ref().ok_or(ENODEV)?;
        let ordinal = tracker.ordinal;
        let hardware_buffer_id = tracker.hardware_buffer_id;
        let command_buffer_state_word = tracker.command_buffer_state_word;
        let state = slot.storage.as_deref_mut().ok_or(ENODEV)?
            .snapshot(ordinal)?;
        let windows = tracker.windows;
        if windows[0].target() != tracker.tiling_prefix || windows[1].target() != tracker.fragment_prefix {
            return Err(EINVAL);
        }
        let execution = g17_completion::RenderExecutionState {
            job_list_empty: state.job_list_empty,
            tiling_timestamps: state.tiling_timestamps,
            fragment_timestamps: state.fragment_timestamps,
        };
        let completion = g17_completion::observe_paired_render_windows_depth(
            windows,
            g17_resources::G17P_RENDER_MAX_IN_FLIGHT as u32,
            tracker.tiling_previous,
            state.tiling,
            tracker.fragment_previous,
            state.fragment,
            execution,
        )
        .map_err(|_| EINVAL)?;
        if !completion.retired
            || !state.job_list_empty
            || (execution.observed() && !accept_executed)
        {
            return Ok(false);
        }
        if self.render_slots.get(pair_slot).tracker.as_ref().ok_or(ENODEV)?.work_state.is_some() {
            // A shared node may be unlinked before maintenance finishes.
            // Even an error cleanup must retain its allocation until both
            // engines publish the exact final stamps and recovery is closed.
            let pair = self.render_slots.get(pair_slot).storage.as_deref().ok_or(ENODEV)?.queue_pair();
            let timestamp = ordinal.checked_add(1).ok_or(EOVERFLOW)?;
            let stamps = self.resources.render_completion_stamps(pair)?;
            if !stamps.into_iter().all(|stamp|
                g17_compute::completion_stamp_covers(stamp, timestamp))
                || self.firmware_recovery_handshake_snapshot()?.state != 0
            {
                return Ok(false);
            }
        }
        // Twin of the paired-completion transaction in
        // `poll_translated_render_completion`; keep the two in step.
        let retired_sksm = self.render_slots.get(pair_slot).sksm.as_ref().map(|queues| {
            let ta = g17_submission::complete_g17p_cl_kicks(queues.tiling.producer, 1)?;
            let fragment = g17_submission::complete_g17p_cl_kicks(queues.fragment.producer, 1)?;
            Ok::<_, g17_submission::SubmissionError>((ta, fragment))
        }).transpose().map_err(|_| EIO)?;
        self.retire_tracked_render_qos(pair_slot)?;
        self.render_hardware_buffer_ids
            .release(hardware_buffer_id)
            .map_err(|_| EIO)?;
        self.release_render_command_buffer_state(pair_slot, command_buffer_state_word)?;
        self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?
            .retire_command_generation(ordinal)?;
        if let (Some(queues), Some((ta, fragment))) = (self.render_slots.get_mut(pair_slot).sksm.as_mut(), retired_sksm) {
            queues.tiling.producer = ta;
            queues.fragment.producer = fragment;
        }
        if crate::g17_config::RENDER_USC_FREELIST != 0 {
            let state = self.render_slots.get_mut(pair_slot);
            if state.flist_retired_ordinal.is_some()
                || state.flist_leases.front().map(|lease| lease.0) != Some(ordinal) { return Err(EIO); }
            state.flist_retired_ordinal = Some(ordinal);
        }
        self.render_slots.get_mut(pair_slot).tracker = self.render_slots.get_mut(pair_slot).queued_tracker.take();
        let pair = self.render_slots.get(pair_slot)
            .storage
            .as_deref()
            .map(g17_resources::G17PUserRenderStorage::queue_pair);
        dev_warn!(
            self.resources.dev().as_ref(),
            "G17P render: firmware retired ordinal {} on slot {} (qids {:?}) [executed={}] -- tiling-ts [{:#x},{:#x}] fragment-ts [{:#x},{:#x}]; failing the submission and keeping the slot\n",
            ordinal,
            pair_slot,
            pair.map(|pair| (pair.tiling, pair.fragment)),
            execution.observed(),
            state.tiling_timestamps[0],
            state.tiling_timestamps[1],
            state.fragment_timestamps[0],
            state.fragment_timestamps[1],
        );
        Ok(true)
    }

    /// Explain a render wait timeout. Split out with the poll body so both the
    /// plain and the event-servicing loops report identically.
    /// Fault-only snapshot before serial-console formatting can let a delayed
    /// command progress. Fixed DATA ranges, one bounded read per word, no MMIO
    /// or callback-pointer dereference. It is not an atomic firmware snapshot.
    fn log_render_callback_frontier(&self, pair_slot: u8, reason: &'static str) {
        if module_parameters::g17p_fault_report.value() & 4 == 0 { return; }
        let Ok(slot) = self.render_slots.allocated(pair_slot) else { return; };
        let Some(tracker) = slot.tracker.as_ref() else { return; };
        let Ok(pair) = self.retained_render_queue_pair(pair_slot) else { return; };
        let Some(addresses) = g17p_render_callback_addresses(tracker.hardware_buffer_id,
            pair.fragment) else { return; };
        let read = |link: u64| -> Result<u64> {
            if link < G17P_GFX_LINK_DATA_BASE || link & 7 != 0
                || link.checked_add(8).is_none_or(|end| end > G17P_GFX_LINK_DATA_END) {
                return Err(EINVAL);
            }
            pgtable::LiveFirmwareU64Probe::new(g17p_gfx_link_to_physical(link))
                .and_then(|probe| probe.observe_mask(u64::MAX, 1))
                .map(|sample| sample.first)
        };
        let started = crate::g17_timing_buffer::arch_counter();
        let callback: [Result<u64>; 7] = core::array::from_fn(|i| read(addresses[0] + i as u64 * 8));
        let waiter = [read(addresses[1]), read(addresses[1] + 8)];
        let pointer = read(addresses[2]);
        let completed = read(addresses[3]);
        let packed = read(addresses[4]);
        let taskword = read(addresses[5]);
        // Fixed bounded DATA fields only. Native hardware walks slots 0..16;
        // the maps are observations, never addresses to dereference here.
        let readiness = read(0xffff_fc00_0012_f668).map(|word| word as u32);
        let slot_maps: [Result<u64>; 16] = core::array::from_fn(|index|
            read(0xffff_fc00_0012_f670 + index as u64 * 8));
        let irq_masks = [read(0xffff_fc00_0017_7128), read(0xffff_fc00_0017_7130)];
        let worker_masks = [read(0xffff_fc00_0017_7228), read(0xffff_fc00_0017_7230)];
        // Completion summary bit21 has a separate IRQ/worker handoff from
        // the paired-callback masks above (B1 218b4, 1db54, 23674..236b4).
        // Snapshot fixed DATA only; do not read interrupt-status MMIO or poke
        // firmware. Each u64 sample is bounded, not a multiword atomic view.
        let completion_irq = [read(0xffff_fc00_0017_7100), read(0xffff_fc00_0017_7188),
            read(0xffff_fc00_0017_7200), read(0xffff_fc00_0017_7288)];
        let walk_masks: [Result<u64>; 10] = core::array::from_fn(|index|
            read(0xffff_fc00_0005_83b8 + index as u64 * 8));
        let paired_exclusions = [read(0xffff_fc00_0005_8158), read(0xffff_fc00_0005_8160)];
        let counter_side_effects = [read(0xffff_fc00_0005_9938), read(0xffff_fc00_0005_9940)];
        let ended = crate::g17_timing_buffer::arch_counter();
        let descriptors = slot.storage.as_deref().map(|storage| (
            storage.descriptor_high_va_at(true, tracker.ordinal),
            storage.descriptor_high_va(true),
        ));
        dev_err!(self.resources.dev().as_ref(),
            "G17P fault callback[{}]: slot={} ordinal={} hwpb={} qid={} counter=[{:#x},{:#x}] cb-va={:#x} cb={:x?} waiter={:x?} pointer={:x?} completed={:x?} packed={:x?} taskword={:x?} TA(oldest,latest)={:x?} readiness={:x?} slot-maps={:x?} irq-masks={:x?} worker-masks={:x?} completion-irq(summary,pending,staged-summary,staged-pending)={:x?} walk-masks(B/P/S/E/D)={:x?} paired-exclusions={:x?} counter-side-effects={:x?} (bounded non-atomic DATA snapshot)\n",
            reason, pair_slot, tracker.ordinal, tracker.hardware_buffer_id, pair.fragment,
            started, ended, addresses[0], callback, waiter, pointer, completed, packed,
            taskword, descriptors, readiness, slot_maps, irq_masks, worker_masks, completion_irq,
            walk_masks, paired_exclusions, counter_side_effects);
    }

    pub(crate) fn report_render_wait_timeout(&mut self, pair_slot: u8, reason: &'static str) {
        self.log_render_callback_frontier(pair_slot, reason);
        let polls = Self::RENDER_COMPLETION_POLLS;
        // The graph remains pinned because firmware may still own it. Runtime
        // teardown stops both provider CPUs before manager resources drop.
        //
        // Say WHY the wait failed. Without this a render timeout is anonymous,
        // and the three stages it can stall in are indistinguishable:
        //
        //   read  < target       the firmware has not consumed this group
        //   done  < target       consumed but not retired
        //   timestamps still 0   retired while the GPU wrote nothing -- the
        //                        exact shape the compute tag-16 bug had
        //
        // Every field is a plain read of an object the driver itself allocated:
        // DRAM only, no sgx MMIO, which would take an asynchronous SError with
        // the GPU cores gated -- and by the time a submit has timed out they
        // are.
        let ordinal = self.render_slots.get(pair_slot).tracker.as_ref()
            .map_or(self.render_slots.get(pair_slot).submission_ordinal, |tracker| tracker.ordinal);
        let final_state = self.render_slots.get_mut(pair_slot)
            .storage
            .as_deref_mut()
            .and_then(|storage| storage.snapshot(ordinal).ok());
        let target = self.render_slots.get(pair_slot).tracker.as_ref()
            .map(|tracker| tracker.windows.map(|window| window.target()));
        if let Some(state) = final_state {
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P render: settle-reason {} (wait budget {} polls); ordinal {} target {:?} tiling done/read/write {}/{}/{} fragment {}/{}/{} job-list-empty {} job-list [first={:#x} last={:#x} head-va={:#x}] tiling-ts [{:#x},{:#x}] fragment-ts [{:#x},{:#x}]\n",
                reason,
                polls,
                ordinal,
                target,
                state.tiling.done,
                state.tiling.read,
                state.tiling.write,
                state.fragment.done,
                state.fragment.read,
                state.fragment.write,
                state.job_list_empty,
                state.job_list[0],
                state.job_list[1],
                state.job_list_head_va,
                state.tiling_timestamps[0],
                state.tiling_timestamps[1],
                state.fragment_timestamps[0],
                state.fragment_timestamps[1],
            );
        }
        self.log_render_context_witness(pair_slot, reason);
    }

    /// The firmware-side view of this render's context binding, labelled.
    ///
    /// Split out of `report_render_wait_timeout` so it can also be taken once
    /// from a render that COMPLETED. Every field the `context/qos` line prints
    /// has a driver-written cold-opening default, so a wedge sample on its own
    /// cannot distinguish "the firmware cleared this" from "the firmware never
    /// wrote it" -- only a healthy sample can.
    pub(crate) fn log_render_context_witness(&mut self, pair_slot: u8, reason: &'static str) {
        // The counter pair that decides whether the job gets unlinked at all.
        //
        // `poll_translated_render_completion` states the rule: "B1 unlinks
        // EventControl only after its completed count reaches its own
        // submitted-count slot". `unlinked()` is `completed == submitted &&
        // membership == 0`, and an unlinked EventControl is what leaves the
        // graph's job list empty -- so this pair is upstream of
        // `job_list_empty`, which is in turn the only term that decides
        // `firmware-retired`, which decides whether one client loses a frame or
        // the whole session is marked crashed.
        //
        // It was sampled nowhere on the settle path and only under
        // `g17p_verbose` on the Pending path, so the one number that explains
        // the split was never in a wedge capture.
        let reusable = self.render_slots.get_mut(pair_slot)
            .storage
            .as_deref_mut()
            .ok_or(ENODEV)
            .and_then(|storage| storage.pool_a_reusable_count());
        let event_ticket = self.render_slots.get(pair_slot).tracker.as_ref()
            .map(|tracker| (tracker.pool_a_record_index, tracker.pool_a_submitted));
        match event_ticket.ok_or(ENODEV).and_then(|(record, submitted)| {
            self.render_slots.get_mut(pair_slot).storage.as_deref_mut().ok_or(ENODEV)?.render_event_control_state(
                record,
                submitted,
            )
        })
        {
            Ok(event_control) => dev_err!(
                self.resources.dev().as_ref(),
                "G17P render event-control [{}]: record={} submitted={} slot-word={} completed={} lag={} membership={:#x} unlinked={} pool-a-reusable={:?}/35\n",
                reason,
                event_control.record_index,
                event_control.submitted,
                event_control.slot_word,
                event_control.completed,
                event_control.submitted.wrapping_sub(event_control.completed),
                event_control.membership,
                event_control.unlinked(),
                reusable,
            ),
            Err(error) => dev_err!(
                self.resources.dev().as_ref(),
                "G17P render event-control [{}]: read failed ({:?})\n",
                reason,
                error,
            ),
        }
        // What the firmware keeps in the per-queue stamp lanes both pairs'
        // records point at. A second pair whose lanes still read as the first
        // pair's is the measured retire-without-executing shape.
        if let Ok(lanes) = self.resources.render_stamp_lanes() {
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P render: stamp lanes [{}] A(qid0..3)={:#x?} B(qid0..3)={:#x?}\n",
                reason,
                lanes[0],
                lanes[1],
            );
        }
        match self.render_slots.get_mut(pair_slot)
            .storage
            .as_deref_mut()
            .ok_or(ENODEV)
            .and_then(|storage| storage.fragment_word_20f0())
        {
            // Demoted to verbose: the healthy control proved it reads 0x0
            // identically on a paired completion and on a recovery-blamed
            // wedge, so it discriminates nothing and was pure noise on the
            // settle path.
            Ok(_) if !crate::util::g17p_verbose() => {}
            Ok(value) => dev_err!(
                self.resources.dev().as_ref(),
                "G17P render descriptor diagnostic [{}]: +0x20f0={:#010x} (DEAD WITNESS -- reads 0x0 identically on a healthy paired completion and on a recovery-blamed wedge, so it discriminates nothing; kept only so nobody re-derives it, do not reason from it)\n",
                reason,
                value,
            ),
            Err(error) => dev_err!(
                self.resources.dev().as_ref(),
                "G17P render descriptor diagnostic: +0x20f0 read failed ({:?})\n",
                error,
            ),
        }
        // ConfigUpdate +0x56 is the AGXQOSQueue HardwareBufferID, not a
        // submission counter. The firmware writes that byte to ctxdesc[0x33]
        // and maintains private ctx<->slot maps before programming MMIO
        // 0x103a0. The private maps are not AP-readable; this shared descriptor
        // is the exact host-safe witness for the same transaction.
        let context_qos = self.render_slots.get_mut(pair_slot).storage.as_deref_mut().and_then(|storage| {
            let header = storage.fragment_config_update_header().ok()?;
            let tiling_header = storage.tiling_config_update_header().ok()?;
            let prefix = storage.render_channel_control_prefix().ok()?;
            Some((header, tiling_header, prefix))
        });
        if let Some((header, tiling_header, prefix)) = context_qos {
            let slot = header.field_56 as u8;
            let fragment_qos = self
                .resources
                .qos_queue_record(header.queue_id as u8)
                .ok();
            let tiling_qos = self
                .resources
                .qos_queue_record(tiling_header.queue_id as u8)
                .ok();
            let bitmap_lo = u64::from_le_bytes([
                prefix[0x12], prefix[0x13], prefix[0x14], prefix[0x15],
                prefix[0x16], prefix[0x17], prefix[0x18], prefix[0x19],
            ]);
            let bitmap_hi = u64::from_le_bytes([
                prefix[0x1a], prefix[0x1b], prefix[0x1c], prefix[0x1d],
                prefix[0x1e], prefix[0x1f], prefix[0x20], prefix[0x21],
            ]);
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P render context/qos [{}]: requested-slot={} ctxdesc(slot={},ctx={},prio={},class={},state={},bound-slot={}) qid-bitmap={:#018x}/{:#018x} qos-records TA(qid {})={:?} 3D(qid {})={:?}\n",
                reason,
                slot,
                prefix[0],
                prefix[1],
                prefix[3],
                prefix[4],
                prefix[0x26],
                prefix[0x33],
                bitmap_lo,
                bitmap_hi,
                tiling_header.queue_id,
                tiling_qos,
                header.queue_id,
                fragment_qos,
            );
        }
    }

    pub(crate) fn sksm_scratch(&self) -> G17SksmScratchGeometry {
        self.platform.sksm_scratch
    }

    pub(crate) fn sksm_queue_geometry(&self) -> g17_submission::G17SksmQueueGeometry {
        self.platform.sksm_queue_geometry
    }

    /// Return the hardware QID of a queue owned by this manager.
    pub(crate) fn compute_queue_id(&self, queue: &G17PSksmQueue) -> Result<u8> {
        if queue.scratch != self.sksm_scratch() || queue.uat_owner != self.uat().ttb_base() {
            return Err(EINVAL);
        }
        Ok(queue.queue_id)
    }

    /// Prove that a prepared compute queue is still host-only and has never
    /// been registered, published, kicked, or assigned a command timestamp.
    /// Submission ordinal of the retained queue, for trace labelling.
    pub(crate) fn compute_queue_submission_ordinal(&self, queue: &G17PSksmQueue) -> u32 {
        queue.submission_ordinal
    }

    /// Whether the retained queue is past its first bind and can take a
    /// steady-state submission.
    pub(crate) fn compute_queue_is_installed(&self, queue: &G17PSksmQueue) -> bool {
        queue.hardware_state == G17PSksmQueueHardwareState::HardwareRegistered
            && queue.submission_ordinal != 0
            && queue.scratch == self.sksm_scratch()
            && queue.uat_owner == self.uat().ttb_base()
    }

    pub(crate) fn validate_prepublication_compute_queue(
        &self,
        queue: &G17PSksmQueue,
    ) -> Result {
        if queue.scratch != self.sksm_scratch()
            || queue.uat_owner != self.uat().ttb_base()
            || queue.hardware_state != G17PSksmQueueHardwareState::PreparedMemory
            || queue.submission_ordinal != 0
            || queue.producer.submitted != 0
            || queue.producer.current_timestamp != queue.producer.geometry.timestamp_seed()
            || queue.producer.last_add_kicks_timestamp
                != queue.producer.geometry.timestamp_seed()
            || queue.producer.base_timestamp != queue.producer.geometry.timestamp_seed()
            || queue.producer.previous_valid
            || queue.producer.previous_timestamp != 0
            || queue.producer.queue_id != queue.queue_id
            || !queue.direct_lifecycle.queue_config_dirty
            || queue.direct_lifecycle.cached_channel_identity != 0
            || queue.direct_lifecycle.next_correlation_token != 1
        {
            return Err(EBUSY);
        }
        Ok(())
    }

    pub(crate) fn bind_compute_user_vm(
        &mut self,
        queue: &mut G17PSksmQueue,
        vm: &mmu::Vm,
    ) -> Result<G17PComputeUserBinding> {
        if queue.scratch != self.sksm_scratch()
            || queue.uat_owner != self.uat().ttb_base()
        {
            return Err(EINVAL);
        }
        self.resources
            .cache_compute_vm_mappings(vm)
            .inspect_err(|error| {
            pr_info!("G17P compute VM: FList aliases failed {:?}\n", error);
        })?;
        let sksm_entries = queue.storage.map_user_entry_alias(vm).inspect_err(|error| {
            pr_info!("G17P compute VM: canonical SKSM entry alias {:#x} failed {:?}\n",
                queue.storage.entry_gpu_va(), error);
        })?;
        let flist = queue.storage.map_user_flist_alias(vm)?;
        let mappings = queue.compute.map_user_execution_state(vm).inspect_err(|error| {
            pr_info!("G17P compute VM: execution aliases failed {:?}\n", error);
        })?;
        // Bracketed to localise the ordering hang: after a failed render the
        // next compute goes silent for tens of seconds here. slotalloc's
        // get_inner is documented "Blocks if no slots are free", and T8140
        // limits the allocator to ONE user slot, so a retained binding from a
        // previous client blocks every later one.
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P compute: acquiring UAT bind for vm\n"
        );
        let vm_bind = self.resources.uat().bind(vm)?;
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P compute: UAT bind acquired, slot {}\n",
            vm_bind.slot()
        );
        // Slot 1 is where the VM is *bound*: the first-partial opening pins
        // the render/descriptor/optional-identity context there and the
        // root-phase machine reads `ttbs[0]`/`ttbs[1]` back verbatim. It is
        // not the context the kick names -- the compute submission declares
        // `context_id = 2`, which needs its own translation root.
        if vm_bind.slot() != 1 || !mappings.all_reachable_from(vm) {
            return Err(EFAULT);
        }
        // Publish that root before anything can be kicked. Without it GPU
        // context 2 has no lower and no upper root at all, and the first
        // firmware-half fetch the front end makes takes a Page Invalid read
        // fault with vm_slot = 2 (unit 0xd9, gUPM).
        self.resources
            .uat()
            .install_t8140_compute_context_alias(&vm_bind)?;
        if crate::g17_config::CTX2_ROOT != 0
            && !self
                .resources
                .uat()
                .t8140_compute_context_alias_ready(&vm_bind)
        {
            let roots = self.resources.uat().t8140_compute_context_roots();
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P compute: context alias did not take (root={:#x}): ctx1 ttb0={:#x}, ctx2 ttb0={:#x} ttb1={:#x}, ctx3 ttb0={:#x} ttb1={:#x}\n",
                vm_bind.root(),
                roots[1].0,
                roots[2].0,
                roots[2].1,
                roots[3].0,
                roots[3].1,
            );
            return Err(EFAULT);
        }
        let (descriptor_high, descriptor_low) = queue.compute.descriptor_gpu_vas(0);
        let encoded_low = mappings.descriptor_gpu_va(0);
        if encoded_low != descriptor_low {
            return Err(EFAULT);
        }
        let high_pa = self.uat().kernel_vm().translate_iova(descriptor_high)?;
        let context0_pa = self
            .uat()
            .kernel_lower_vm()
            .translate_iova(descriptor_low)?;
        let context1_pa = vm.translate_iova(encoded_low)?;
        if high_pa != context0_pa || high_pa != context1_pa {
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P descriptor aliases disagree high={:#x}->{:#x} context0-low={:#x}->{:#x} context1-low={:#x}->{:#x}\n",
                descriptor_high,
                high_pa,
                descriptor_low,
                context0_pa,
                encoded_low,
                context1_pa
            );
            return Err(EFAULT);
        }
        let entry_aliases = queue.storage.entry_aliases()?;
        if sksm_entries.iova() != entry_aliases.low()
            || sksm_entries.size() != entry_aliases.extent()
        {
            return Err(EFAULT);
        }
        let entry_high_pa = self
            .uat()
            .kernel_vm()
            .translate_iova(entry_aliases.high())?;
        let entry_context0_pa = self
            .uat()
            .kernel_lower_vm()
            .translate_iova(entry_aliases.low())?;
        let entry_context1_pa = vm.translate_iova(sksm_entries.iova())?;
        if entry_high_pa != entry_context0_pa || entry_high_pa != entry_context1_pa {
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P SKSM entry aliases disagree high={:#x}->{:#x} context0-low={:#x}->{:#x} context1-low={:#x}->{:#x}\n",
                entry_aliases.high(),
                entry_high_pa,
                entry_aliases.low(),
                entry_context0_pa,
                sksm_entries.iova(),
                entry_context1_pa
            );
            return Err(EFAULT);
        }
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P SKSM entry aliases: high={:#x} context0-low={:#x} context1-low={:#x} backing-pa={:#x} size={:#x}\n",
            entry_aliases.high(),
            entry_aliases.low(),
            sksm_entries.iova(),
            entry_high_pa,
            entry_aliases.extent()
        );
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P descriptor aliases: high={:#x} context0-low={:#x} encoded/context1-low={:#x} backing-pa={:#x}\n",
            descriptor_high,
            descriptor_low,
            encoded_low,
            high_pa
        );
        Ok(G17PComputeUserBinding {
            mappings,
            _flist: flist,
            _sksm_entries: sksm_entries,
            _vm_bind: Some(vm_bind),
            _execution_context: None,
        })
    }

    pub(crate) fn bind_independent_compute_user_vm(
        &mut self, queue: &mut G17PSksmQueue, vm: &mmu::Vm,
        context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
    ) -> Result<G17PComputeUserBinding> {
        if !context.matches(vm) || !context.is_current() { return Err(EFAULT); }
        // Independent compute roots execute the same firmware context-local
        // low-view accesses as render roots. Render binding already installs
        // this native SecureGart closure; omitting it for compute leaves
        // writes such as VA 0x1080 unmapped when CL_2 switches between QIDs.
        // The helper is idempotent for a VM shared by multiple queues and
        // retains the mapping for the VM lifetime.
        self.resources.uat().install_t8140_job_context_aliases(vm)?;
        self.resources.cache_compute_vm_mappings(vm)?;
        let sksm_entries = queue.storage.map_user_entry_alias(vm)?;
        let flist = queue.storage.map_user_flist_alias(vm)?;
        let mappings = queue.compute.map_user_execution_state(vm)?;
        let (high, low) = queue.compute.descriptor_gpu_vas(0);
        if !mappings.all_reachable_from(vm) || mappings.descriptor_gpu_va(0) != low
            || vm.translate_iova(low)? != self.uat().kernel_vm().translate_iova(high)?
        { return Err(EFAULT); }
        queue.independent_context = true;
        Ok(G17PComputeUserBinding {
            mappings,
            _flist: flist,
            _sksm_entries: sksm_entries, _vm_bind: None,
            _execution_context: Some(context.clone()),
        })
    }

    /// Start one shared-ring transaction. All target queues are staged before
    /// the ring is read, so a coalesced event incurs one DRAM walk regardless
    /// of how many clients completed in the batch.
    pub(crate) fn begin_independent_compute_completion_batch(&mut self) {
        self.compute_completion_batch.clear();
    }

    /// Request one published, unretired kick by its exact descriptor. Call
    /// once per in-flight kick of a queue, oldest first, consecutively.
    pub(crate) fn stage_independent_compute_completion(
        &mut self,
        queue: &G17PSksmQueue,
        ticket: &G17PComputeRetirementTicket,
    ) -> Result<bool> {
        if !queue.independent_context || queue.completion_descriptor == 0 {
            return Ok(false);
        }
        if ticket.queue_id != queue.queue_id || ticket.descriptor == 0 {
            return Err(EIO);
        }
        self.compute_completion_batch.request(
            queue.queue_id,
            ticket.descriptor,
            ticket.kick_timestamp,
        )?;
        Ok(true)
    }

    /// The record the current batch captured for one requested kick, if any.
    /// The caller latches it: the ring slot may be reused before the kick can
    /// retire (see `G17PComputeCompletionBatch`).
    pub(crate) fn take_staged_independent_compute_observation(
        &mut self,
        queue: &G17PSksmQueue,
        ticket: &G17PComputeRetirementTicket,
    ) -> Result<Option<g17_resources::G17PComputeCompletionCandidate>> {
        if ticket.queue_id != queue.queue_id {
            return Err(EIO);
        }
        self.compute_completion_batch.take(queue.queue_id, ticket.descriptor, ticket.kick_timestamp)
    }

    /// Firmware has consumed this kick's item records. This alone is NOT
    /// completion: the caller separately requires the exact visibility stamp
    /// and an execution record, or the existing recovery-free recordless proof.
    /// Mixed-engine TSQ counters cannot identify a physical QID's completed kick.
    pub(crate) fn independent_compute_kick_finished(
        &mut self,
        queue: &mut G17PSksmQueue,
        ticket: &G17PComputeRetirementTicket,
    ) -> Result<bool> {
        let ring = queue.compute.read_compute_ring_state()?;
        if ring.count != ticket.item_count || ring.count == 0 {
            return Err(EIO);
        }
        let count = ring.count;
        let producer = ring.producer % count;
        let consumer = ring.consumer % count;
        let own_end = ticket.item_producer % count;
        let unconsumed = (producer + count - consumer) % count;
        let published_after = (producer + count - own_end) % count;
        Ok(unconsumed <= published_after)
    }

    pub(crate) fn scan_independent_compute_completion_batch(&mut self) -> Result {
        self.resources
            .scan_compute_completion_batch(&mut self.compute_completion_batch)
    }

    /// Snapshot the terminal values published for the active compute command.
    /// This runs after the outer channel publication and before its RTKit
    /// notifications are sent, while the runtime mutex excludes completion and
    /// recovery handling.
    pub(crate) fn capture_independent_compute_retirement_ticket(
        &mut self,
        queue: &mut G17PSksmQueue,
        work_state: &g17_resources::G17PWorkStateLease,
    ) -> Result<G17PComputeRetirementTicket> {
        if !queue.independent_context
            || queue.completion_descriptor == 0
            || queue.producer.submitted == 0
        {
            return Err(EIO);
        }
        let target = queue.submission_ordinal;
        let ring = queue.compute.read_compute_ring_state()?;
        if ring.count == 0 || ring.producer >= ring.count
            || !queue.producer.previous_valid || queue.producer.previous_timestamp == 0
        {
            return Err(EIO);
        }
        Ok(G17PComputeRetirementTicket {
            queue_id: queue.queue_id,
            descriptor: queue.completion_descriptor,
            tsq_node: work_state.tsq_gpu_va()?,
            // `prepare_g17p_cl_kick_entry` leaves the published kick's own
            // timestamp here; the next publication has not happened yet.
            kick_timestamp: queue.producer.previous_timestamp,
            submitted_target: target,
            item_producer: ring.producer,
            item_count: ring.count,
        })
    }

    /// Retire the oldest kick from its exact channel visibility prefix and
    /// consumed item records, with a matching KSM execution record whenever
    /// available. A mixed-engine TSQ node's completed count is not a QID
    /// completion sequence and is read only for diagnostics.
    ///
    /// Native TSQ nodes carry the same stable work-state identity as execution
    /// and are ordered by that full identity. One channel's stable stamp cell
    /// advances in its kick sequence. A completed later kick covers the earlier
    /// channel prefix; another engine's TSQ counter cannot provide that proof.
    ///
    /// Completion-ring records are transient. The existing recordless grace
    /// remains restricted to work which did not cross a recovery epoch. Across
    /// recovery only a captured record can reject the zero-duration shape.
    /// The caller checks the closed recovery handshake before entering here.
    pub(crate) fn poll_staged_independent_compute_completion(
        &mut self,
        queue: &mut G17PSksmQueue,
        ticket: &G17PComputeRetirementTicket,
        work_state: &g17_resources::G17PWorkStateLease,
        observed: Option<g17_resources::G17PComputeCompletionCandidate>,
        recordless: G17PComputeRecordless,
        fallback_end: Option<u64>,
    ) -> Result<Option<G17PComputeRetired>> {
        let (recovery_quiet, crossed_recovery) = match recordless {
            G17PComputeRecordless::Allowed => (true, false),
            G17PComputeRecordless::NotYet => (false, false),
            G17PComputeRecordless::CrossedRecovery => (false, true),
        };
        if !queue.independent_context || queue.completion_descriptor == 0 {
            return Ok(None);
        }
        if ticket.queue_id != queue.queue_id
            || ticket.descriptor == 0
            || ticket.tsq_node != work_state.tsq_gpu_va()?
            || ticket.item_count == 0
            || queue.producer.submitted == 0
        {
            dev_err!(self.resources.dev().as_ref(),
                "G17P async compute: retirement-invariant qid={} cause=poll-ticket qid={}/{} descriptor={:#x} newest={:#x} item-count={} in-flight={}\n",
                queue.queue_id, ticket.queue_id, queue.queue_id,
                ticket.descriptor, queue.completion_descriptor, ticket.item_count,
                queue.producer.submitted);
            return Err(EIO);
        }
        if let Some(candidate) = observed.as_ref() {
            let timestamps = candidate.timestamps;
            if timestamps[0] == timestamps[1] {
                // Retired without executing. This is the DOCUMENTED post-recovery
                // outcome -- `g17p_zero_duration` says so in as many words: "every
                // submission after a fault-blamed firmware recovery retires with
                // duration 0" -- and that parameter's default 1 is "fail the
                // submission ... but do NOT rebuild the firmware session".
                G17P_MARK_ZERO_DURATION.fetch_add(1, Ordering::Relaxed);
                self.log_compute_usage_slot(queue, "zero-duration");
                dev_err!(self.resources.dev().as_ref(),
                    "G17P async compute: zero-duration qid={} descriptor={:#x} kick-ts={:#x} timestamps=[{},{}] -- retired without executing, the expected shape after a firmware recovery\n",
                    queue.queue_id, ticket.descriptor, ticket.kick_timestamp,
                    timestamps[0], timestamps[1]);
                // ENODATA, not EIO, when the parameter is enabled: the caller
                // treats it as submission-scoped. Setting the parameter to 0
                // restores the old device-wide behaviour byte for byte.
                if crate::g17_config::ZERO_DURATION != 0 {
                    return Err(ENODATA);
                }
                return Err(EIO);
            }
        }
        let stamp = queue.compute.read_compute_stamp()?;
        if queue.completion_stamp_armed
            && !g17_compute::completion_stamp_covers(stamp, ticket.kick_timestamp)
        {
            return Ok(None);
        }
        if !self.independent_compute_kick_finished(queue, ticket)? {
            return Ok(None);
        }
        let [target, completed] = work_state.tsq_counters()?;
        let floor = queue.completion_last_end;
        let (timestamps, pairs) = match observed {
            Some(candidate) => (candidate.timestamps, candidate.pairs),
            None if recovery_quiet => {
                let recordless = G17P_COMPUTE_RECORDLESS_RETIRES.fetch_add(1, Ordering::Relaxed) + 1;
                if recordless == 1 || recordless % 256 == 0 {
                    dev_info!(self.resources.dev().as_ref(),
                        "G17P async compute: qid={} kick-ts={:#x} retired on firmware counters and stamp {:#x}; its completion record was overwritten before a scan saw it ({} so far)\n",
                        queue.queue_id, ticket.kick_timestamp, stamp, recordless);
                }
                ([floor, fallback_end.unwrap_or(floor).max(floor)], [0; 4])
            }
            // Not yet allowed to go recordless: wait for the record or for the
            // caller's next pass.
            None if !crossed_recovery => return Ok(None),
            None => {
                dev_err!(self.resources.dev().as_ref(),
                    "G17P async compute: qid={} kick-ts={:#x} finished across a firmware recovery with no completion record; cannot exclude a zero-duration retire\n",
                    queue.queue_id, ticket.kick_timestamp);
                if crate::g17_config::ZERO_DURATION != 0 {
                    return Err(ENODATA);
                }
                return Err(EIO);
            }
        };
        if let Some(pair) = observed.and_then(|candidate| candidate.interval) {
            if crate::util::g17p_verbose() {
                dev_info!(self.resources.dev().as_ref(),
                    "G17P async compute: GPU-INTERVAL qid={} descriptor={:#x} start={} end={}\n",
                    queue.queue_id, ticket.descriptor, pair[0], pair[1]);
            }
        }
        if crate::g17_config::COMPUTE_RESOURCE_SNAPSHOT != 0
            && queue.producer.previous_valid
        {
            let last = g17_submission::prepare_kick_submission(
                queue.producer.previous_timestamp,
                queue.queue_id,
                queue.producer.geometry.stamp_count(),
                queue.producer.geometry.entry_stride(),
            ).map_err(|_| EINVAL)?;
            let entry = queue.storage.snapshot_compute_entry(last.entry_offset as usize)?;
            dev_info!(self.resources.dev().as_ref(),
                "G17P B1 entry retirement: qid={} stamp={:#x} offset={:#x} header={:#x} word08={:#x} rce-kind-word={:#x} rce0={:#x} rce1={:#x} rce2={:#x} rce3={:#x}\n",
                queue.queue_id, queue.producer.previous_timestamp,
                last.entry_offset, entry[0], entry[1], entry[41], entry[42],
                entry[43], entry[44], entry[45]);
        }
        self.log_compute_usage_slot(queue, "retired");
        self.resources.complete_qos_queue_records(&[
            (queue.queue_id, queue.qos_hardware_buffer_id(), 2),
        ])?;
        let retired = g17_submission::complete_g17p_cl_kicks(queue.producer, 1)
            .map_err(|_| EINVAL)?;
        queue.producer = retired;
        // Only a real record advances the floor: a bracketing fallback end is
        // a later kick's end and must stay above this floor for that kick.
        if observed.is_some() {
            queue.completion_last_end = queue.completion_last_end.max(timestamps[1]);
        }
        if retired.submitted == 0 {
            queue.completion_descriptor = 0;
            queue.completion_stamp_armed = false;
        }
        Ok(Some(G17PComputeRetired {
            timestamps,
            pairs,
            counters: [target, completed],
            stamp,
            in_flight_after: retired.submitted,
        }))
    }

    /// Queue-local terminal witness for a quarantined set of commands. The
    /// caller supplies its newest kick and checks the closed recovery handshake.
    /// Its exact final visibility prefix and fully consumed item ring cover the
    /// preceding ordered CL kicks. This releases command pins, not the graph;
    /// the graph remains quarantined, and aggregate owners retain shared TSQ
    /// backing for any other engines still live. Mixed-engine node counters
    /// cannot be compared with this physical queue's publication ordinal.
    pub(crate) fn independent_compute_retired_by_firmware(
        &mut self,
        queue: &mut G17PSksmQueue,
        ticket: &G17PComputeRetirementTicket,
        in_flight: u32,
    ) -> Result<bool> {
        if !queue.independent_context
            || ticket.queue_id != queue.queue_id
            || ticket.descriptor == 0
            || ticket.descriptor != queue.completion_descriptor
            || ticket.item_count == 0
            || in_flight == 0
        {
            return Err(EINVAL);
        }
        // The flush prefix must cover this exact kick, alongside the complete
        // counter and consumed item records. A previous nonzero store is not
        // evidence of a later kick's cache maintenance.
        if queue.completion_stamp_armed
            && !g17_compute::completion_stamp_covers(
                queue.compute.read_compute_stamp()?, ticket.kick_timestamp)
        {
            return Ok(false);
        }
        let ring = queue.compute.read_compute_ring_state()?;
        if ring.count != ticket.item_count || ring.count == 0 {
            return Err(EIO);
        }
        let consumer = ring.consumer % ring.count;
        let producer = ring.producer % ring.count;
        Ok(producer == ticket.item_producer % ring.count
            && consumer == ticket.item_producer % ring.count
            && queue.producer.submitted == in_flight)
    }

    /// `g17p_recovery_isolation`: return a queue whose quarantined kicks were
    /// all cancelled by another client's recovery and are now proven retired
    /// by `independent_compute_retired_by_firmware` (the caller has just
    /// observed it true for the newest of exactly `in_flight` kicks).
    ///
    /// Settles the host accounting an ordinary retirement of those kicks
    /// would have settled -- QoS in-flight records, the producer's submitted
    /// count, the armed completion descriptor/stamp -- and, as the retained
    /// queue does after every recovery (`RECOVERY_RESYNC` bit0), makes the
    /// next kick carry the no-predecessor implicit dependency: every earlier
    /// kick of this QID is retired, so there is nothing left to order after,
    /// and a recovery-cancelled predecessor stamp is not relied upon.
    /// Forward-only state (stamp sequence, ordinal, item ring) is untouched.
    pub(crate) fn return_witnessed_compute_queue(
        &mut self,
        queue: &mut G17PSksmQueue,
        in_flight: u32,
    ) -> Result {
        if !queue.independent_context || in_flight == 0
            || queue.producer.submitted != in_flight
            || self.firmware_recovery_handshake_snapshot()?.state != 0
        {
            return Err(EINVAL);
        }
        let ring = queue.compute.read_compute_ring_state()?;
        if ring.count == 0 || ring.consumer % ring.count != ring.producer % ring.count {
            return Err(EBUSY);
        }
        for _ in 0..in_flight {
            self.resources.complete_qos_queue_records(&[
                (queue.queue_id, queue.qos_hardware_buffer_id(), 2),
            ])?;
        }
        queue.producer = g17_submission::complete_g17p_cl_kicks(queue.producer, in_flight)
            .map_err(|_| EINVAL)?;
        queue.completion_descriptor = 0;
        queue.completion_stamp_armed = false;
        queue.producer.previous_valid = false;
        queue.producer.previous_timestamp = 0;
        Ok(())
    }

    /// Allocate a stable graph identity and lease an empty slot in the common
    /// table. Constructor failure returns the slot, but never reuses identity.
    fn allocate_compute_queue_id(&mut self) -> Result<crate::g17_channel::Lease> {
        use crate::g17_channel::{ChannelId, Engine};
        let identity = self.next_channel_identity;
        let next = identity.checked_add(1).ok_or(EOVERFLOW)?;
        // Keep render pairs servable when compute faults have used up the rest.
        let lease = self.channel_ids.acquire_reserving(
            ChannelId(identity), Engine::Compute, Some(G17P_COMPUTE_QUEUE_ID_BASE),
            crate::g17_channel::COMPUTE_RENDER_RESERVE,
        ).ok_or(ENOSPC)?;
        self.next_channel_identity = next;
        Ok(lease)
    }

    /// Allocate and encode the native compute queue without touching the SKSM
    /// hardware bridge. The dirty first direct submitBuffer asks firmware to
    /// install the QID through tag 15 once startup is complete.
    pub(crate) fn prepare_logical_queue_context(
        &mut self, dev: &AsahiDevice, vm: &mmu::Vm,
    ) -> Result<Arc<g17_resources::G17PLogicalQueueContext>> {
        // QUEUE_CREATE runs in the owning process, before asynchronous dispatch.
        let owner_pid = u32::try_from(kernel::current!().group_leader().pid())
            .map_err(|_| EOVERFLOW)?;
        if self.logical_qos_ids.is_none() {
            // Keep bootstrap QoS IDs 0/1 out of the logical namespace.
            // Native AGXQOSManager::init supplies 128 slots to its allocator;
            // unlike the PB namespace, QoS table index 127 is usable.
            self.logical_qos_ids = Some(Arc::pin_init(kernel::new_mutex!(
                3u128, "G17 logical QoS IDs"
            ), GFP_KERNEL)?);
        }
        let qos = g17_resources::G17PLogicalQosLease::new(
            self.logical_qos_ids.as_ref().ok_or(EIO)?.clone(),
        )?;
        let execution = self.uat().allocate_t8140_compute_execution_context(vm)?;
        let control = Arc::new(g17_resources::G17PClChannelControl::new(dev, self.uat())?, GFP_KERNEL)?;
        let tsq = g17_resources::G17PWorkStateStorage::new(dev, self.uat())?;
        let context = Arc::new(g17_resources::G17PLogicalQueueContext::new(
            execution, control, qos, owner_pid, tsq,
        )?, GFP_KERNEL)?;
        Ok(context)
    }

    /// Diagnostic isolation of the execution ID from cross-engine firmware
    /// caches. Scheduler/QoS/process ownership stays with the logical queue.
    /// This consumes another root-table slot; it is not a production fix.
    pub(crate) fn prepare_compute_execution_context(
        &mut self,
        context: &Arc<g17_resources::G17PLogicalQueueContext>,
        split: bool,
    ) -> Result<Arc<g17_resources::G17PLogicalQueueContext>> {
        if !split { return Ok(context.clone()); }
        if !context.is_current() { return Err(EFAULT); }
        let execution = self.uat().allocate_t8140_compute_execution_context(context.vm())?;
        if !execution.matches(context.vm()) || execution.context_id() == context.context_id() {
            return Err(EIO);
        }
        let tsq = g17_resources::G17PWorkStateStorage::new(self.resources.dev(), self.uat())?;
        let compute = Arc::new(g17_resources::G17PLogicalQueueContext::new(
            execution, context.channel_control.clone(), context.qos.clone(), context.owner_pid,
            tsq,
        )?, GFP_KERNEL)?;
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P split-execution-ID probe: pid={} vm={} root={:#x} render-context={} compute-context={} scheduler={:#x} qos={} -- diagnostic only; extra root slot, no execution gating\n",
            context.owner_pid, context.vm_id(), context.root(), context.context_id(),
            compute.context_id(), compute.channel_control_gpu_va(), compute.qos.id(),
        );
        Ok(compute)
    }

    /// A prepared graph may be dropped on close only while its exact lease
    /// remains unpublished. Hardware-state alone is insufficient: publication
    /// can fail after the lease was sealed but before that state advances.
    pub(crate) fn cancel_unpublished_compute_queue(&mut self, queue: &G17PSksmQueue) -> bool {
        queue.requires_first_install()
            && self.channel_ids.cancel_unpublished(queue.channel_lease)
    }

    pub(crate) fn prepare_g17p_compute_queue(
        &mut self,
        dev: &AsahiDevice,
    ) -> Result<G17PSksmQueue> {
        let channel_lease = self.allocate_compute_queue_id()
            .inspect_err(|error| crate::g17_queue_slots::note_error(
                crate::g17_queue_slots::Pool::QidCompute, *error))?;
        let queue_id = channel_lease.qid;
        let hardware_buffer_id = 1;
        let result = (|| {
        const COMPLETION_SELECTOR: u8 = 2;

        let geometry = self.sksm_queue_geometry();
        let flist = Arc::new(
            g17_resources::G17PComputeFlist::new(dev, self.uat(), queue_id)?, GFP_KERNEL,
        )?;
        self.resources.initialize_compute_pool_uma(&flist)?;
        let storage = g17_resources::G17PClRuntimeStorage::new(
            dev,
            self.uat(),
            geometry,
            flist,
        )?;
        if self.compute_channel_control.is_none() {
            self.compute_channel_control =
                Some(g17_resources::G17PClChannelControl::new(dev, self.uat())?);
        }
        let channel_control_gpu_va = self
            .compute_channel_control
            .as_ref()
            .ok_or(EIO)?
            .gpu_va();
        let compute = g17_resources::G17PComputeDescriptorStorage::new(
            dev,
            self.uat(),
            channel_control_gpu_va,
        )?;
        let routing = g17_submission::G17PComputeQueueRouting {
            queue_id,
            completion_selector: COMPLETION_SELECTOR as u32,
            entry_gpu_va: storage.entry_gpu_va(),
            geometry,
        };
        let config =
            g17_submission::prepare_g17p_compute_sksm_queue_config(routing, self.sksm_scratch())
                .map_err(|_| EINVAL)?;

        Ok(G17PSksmQueue {
            queue_id: routing.queue_id,
            channel_lease,
            hardware_buffer_id,
            scratch: self.sksm_scratch(),
            uat_owner: self.uat().ttb_base(),
            configure_pair: config.write_pair(),
            enable_pair: config.enable_pair,
            disable_pair: config.disable_pair,
            hardware_state: G17PSksmQueueHardwareState::PreparedMemory,
            storage,
            channel_control_gpu_va,
            logical_channel_control: None,
            logical_qos: None,
            compute,
            producer: geometry
                .initial_producer_state(
                    queue_id,
                    COMPLETION_SELECTOR,
                    g17_submission::G17PClQosConfig {
                        queue_byte_32: 1,
                        class_40: 8,
                        word_48: 0xffff,
                    },
                )
                .map_err(|_| EINVAL)?,
            direct_lifecycle: g17_submission::G17PDirectClLifecycleState {
                queue_config_dirty: true,
                cached_channel_identity: 0,
                next_correlation_token: 1,
            },
            configure_guard_accelerator_stamp: 0,
            configure_guard_event_stamp: 0,
            published_config: None,
            outer_channel_started: false,
            submission_ordinal: 0,
            independent_context: false,
            completion_descriptor: 0,
            completion_last_end: 0,
            completion_stamp_armed: false,
            publication_resource_snapshot: None,
        })
        })();
        // No firmware publication occurs in this constructor. A failed host
        // allocation must not consume the finite QID namespace permanently.
        if result.is_err() {
            if !self.channel_ids.cancel_unpublished(channel_lease) { return Err(EIO); }
        }
        result
    }

    /// Prepare a queue for its firmware-mediated tag-15 installation.
    ///
    /// Native performs no host MMIO during steady-state rendering, and the
    /// working QID4 path also succeeds with the AP SKSM aperture suppressed.
    /// The queue therefore becomes installed only when its tag-15 group is
    /// published to the firmware channel below, never from an AP port write.
    fn prepare_compute_firmware_install(
        &self,
        queue: &mut G17PSksmQueue,
    ) -> Result {
        if !self.channel_ids.matches(queue.channel_lease) { return Err(EIO); }
        if queue.scratch != self.sksm_scratch() || queue.uat_owner != self.uat().ttb_base() {
            return Err(EINVAL);
        }
        if queue.hardware_state != G17PSksmQueueHardwareState::PreparedMemory {
            return Err(EBUSY);
        }
        if queue.configure_guard_accelerator_stamp != queue.configure_guard_event_stamp {
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P QID {} configure guard mismatch accelerator={} event-machine={}\n",
                queue.queue_id,
                queue.configure_guard_accelerator_stamp,
                queue.configure_guard_event_stamp
            );
            return Err(EBUSY);
        }
        queue.storage.prepare_live_mappings(self.uat())?;
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P submitBuffer/prepareMappings: low/high QID4 aliases published to live UAT\n"
        );

        fence(Ordering::SeqCst);
        Ok(())
    }

    /// Remove a retained queue from the hardware bridge before releasing its
    /// backing storage. Repeated teardown is harmless.
    pub(crate) fn unregister_compute_sksm_queue(
        &self,
        registers: &regs::Resources,
        queue: &mut G17PSksmQueue,
    ) -> Result {
        if queue.scratch != self.sksm_scratch() || queue.uat_owner != self.uat().ttb_base() {
            return Err(EINVAL);
        }
        if queue.hardware_state == G17PSksmQueueHardwareState::PreparedMemory {
            return Ok(());
        }

        if registers.g17p_cancel_deferred_sksm_registration(queue.configure_pair) {
            queue.hardware_state = G17PSksmQueueHardwareState::PreparedMemory;
            return Ok(());
        }
        registers.g17p_sksm_publish_write_pair(queue.disable_pair)?;
        queue.hardware_state = G17PSksmQueueHardwareState::PreparedMemory;
        Ok(())
    }

    /// Publish one complete CL entry, then perform the queue's ordered SKSM
    /// notification. The first post-registration entry publishes the retained
    /// B2 state; later entries keep those provider-owned pages unchanged.
    pub(crate) fn submit_compute_entry(
        &mut self,
        registers: &regs::Resources,
        queue: &mut G17PSksmQueue,
        operands: g17_submission::G17PClKickEntryOperands<'_>,
    ) -> Result<u64> {
        if queue.hardware_state != G17PSksmQueueHardwareState::HardwareRegistered
            || queue.scratch != self.sksm_scratch()
            || queue.uat_owner != self.uat().ttb_base()
        {
            return Err(EINVAL);
        }
        let prepared = g17_submission::prepare_g17p_cl_kick_entry(queue.producer, operands)
            .map_err(|_| EINVAL)?;
        self.publish_compute_entry(registers, queue, prepared)
    }

    fn publish_compute_entry(
        &mut self,
        registers: &regs::Resources,
        queue: &mut G17PSksmQueue,
        prepared: g17_submission::PreparedG17PClKickEntry,
    ) -> Result<u64> {
        if g17p_compute_sksm_entry_enabled() {
            queue.storage.write_entry(
                prepared.entry_offset as usize,
                prepared.zero_length as usize,
                &prepared.entry,
            )?;
        }

        self.publish_compute_b2_once(queue)?;

        fence(Ordering::SeqCst);
        queue.producer = prepared.producer_after;
        let timestamp = prepared.current_timestamp;
        queue.storage.reserve_flist_submission()?;
        self.add_sksm_kicks(registers, queue, 1)?;
        queue.producer.last_add_kicks_timestamp = timestamp;
        fence(Ordering::SeqCst);
        Ok(timestamp)
    }

    fn publish_compute_b2_once(&mut self, _queue: &mut G17PSksmQueue) -> Result {
        self.resources.stage_compute_region_views()?;
        if self.compute_b2_activation == g17_submission::G17PClB2ActivationState::Pending {
            let bootstrap_control = self.compute_channel_control.as_ref().ok_or(EIO)?.gpu_va();
            let activation = self.compute_b2_activation;
            let published = self.resources.with_b2_computed_mut(|computed| {
                g17_submission::apply_g17p_cl_b2_computed_state(
                    activation,
                    computed,
                    bootstrap_control,
                )
                .map_err(|_| EINVAL)
            })?;
            self.compute_b2_activation = published;
        }
        Ok(())
    }

    /// The binary-verified first-bind chronology: tag 3 -> 15 -> QID
    /// configure -> SKSM entry -> 14 -> 16 -> CL_2, ending with the queue
    /// registered on the SKSM bridge. It is shape-independent -- the command
    /// only supplies the CDM range, USC window, sampler state and timestamps
    /// that `build_compute_descriptor` consumes -- so any well-formed compute
    /// command may drive it, not just the add3 probe.
    fn submit_translated_first_bind_channel(
        &mut self,
        _registers: &regs::Resources,
        queue: &mut G17PSksmQueue,
        submission_phase: &AtomicU32,
        binding: &G17PComputeUserBinding,
        command: &g17_uapi::TranslatedComputeCommand,
        context: G17PComputeSubmissionContext<'_>,
        add3_buffers: Option<[u64; 3]>,
    ) -> Result<g17_submission::PreparedG17PDirectClNotifications> {
        if queue.submission_ordinal != 0 {
            return Err(EINVAL);
        }
        // This release gate is deliberately first: any future contract
        // regression must fail before descriptor, ring, SKSM, MMIO, or mailbox
        // state changes.
        g17_submission::require_g17p_first_bind_direct_hardware_release(
            g17_submission::G17P_FIRST_BIND_COMMAND_PLAN,
        )
        .map_err(|_| ENOTSUPP)?;

        let descriptor_slot = 0;
        let (descriptor_high, _) = queue.compute.descriptor_gpu_vas(descriptor_slot);
        let descriptor_low = binding.mappings.descriptor_gpu_va(descriptor_slot);
        // The +0x14a0 operand table belongs to the add3 *shader*, not to the
        // firmware. Register 0x1a510 names the retained preempt object and the
        // hand-written USC binary loads input A, input B and output out of it;
        // the constant 0x14a0 is not an offset anywhere in the b1 firmware
        // image, so no firmware path reads the structure. A Honeykrisp
        // dispatch carries its bindings in the command stream instead (root
        // descriptor pointer plus its Ioto uniform ranges), so it neither
        // needs nor reads the table. Write it only for the flagged probe. The
        // object itself is allocated, mapped and named by 0x1a510 either way,
        // and 0x1a4d0..0x1a4e8 keep pointing at preempt+0x1480..+0x1498, whose
        // contents the proven add3 run also leaves entirely zero.
        match add3_buffers {
            Some(buffers) => queue.compute.write_g17p_add3_resource_table(buffers)?,
            None => dev_info!(
                self.resources.dev().as_ref(),
                "G17P compute: generic first-bind, no add3 operand table; 0x1a510 still names the retained preempt object\n"
            ),
        }

        let user_layout = binding.mappings.layout();
        let shared_control = queue.storage.shared_support_gpu_va();
        let objects = g17_compute::ComputeDescriptorObjects {
            scheduler_record: queue.compute.scheduler_record_gpu_va(),
            descriptor_low_alias: descriptor_low,
            dispatch_a: queue.compute.dispatch_a_gpu_va(),
            dispatch_b: queue.compute.dispatch_b_gpu_va(),
            status_a: queue.compute.status_a_gpu_va(),
            status_b: queue.compute.status_b_gpu_va(),
            shared_control,
            usage_record: queue.compute.compute_usage_record_gpu_va(),
        };
        queue.compute.clear_compute_usage_slot()?;
        // The timestamp this kick is about to be published under.
        let kick_timestamp = queue.producer.current_timestamp;
        let resume_checkpoints = queue.compute.with_descriptor_mut(descriptor_slot, |raw| {
            g17_uapi::build_compute_descriptor(
                command,
                g17_uapi::ComputeInternalState {
                    preempt_base: user_layout.preempt,
                    dispatch_identity: context.dispatch_identity,
                    context_id: context.context_id,
                    work_ordinal: context.work_state_word,
                    robustness: user_layout.robustness,
                    operand_state_base: user_layout.operand_state,
                    usc_flist_id: u32::from(queue.storage.flist_id()),
                },
                objects,
                g17_compute::ComputeDescriptorMetadata {
                    submit_sequence: 0,
                    context_id: context.context_id,
                    grid_index: u32::from(queue.queue_id),
                    work_ordinal: 0,
                    queue_submission: 1,
                    queue_ordinal: 0,
                    submission_index: 1,
                    support_control: 0xe0a0_0001,
                    support_flags: 0,
                },
                context.timestamps,
                raw,
            )
            .map_err(|_| EINVAL)
            .and_then(|checkpoints| {
                if let Some(work_state) = context.work_state {
                    work_state.bind_compute_descriptor(raw)?;
                }
                g17_compute::set_context_generation(raw, binding.context_generation())
                    .map_err(|_| EINVAL)?;
                // Stamp progression is independent of TSQ slot selection.
                g17_compute::set_completion_stamp_value(raw, kick_timestamp)
                    .map_err(|_| EINVAL)?;
                g17_compute::set_hardware_timestamp(raw, kick_timestamp)
                    .map_err(|_| EINVAL)?;
                Ok(checkpoints)
            })
        })?;
        queue
            .compute
            .with_descriptor_mut(descriptor_slot, |raw| {
                self.log_record("descriptor", &raw[..raw.len().min(320)]);
                Ok(())
            })?;

        let primary_checkpoint = u8::try_from(resume_checkpoints[0]).map_err(|_| EINVAL)?;
        let command_state = g17_submission::prepare_g17p_compute_cl_command_state(
            descriptor_low,
            u64::from(queue.submission_ordinal),
            primary_checkpoint,
        )
        .map_err(|_| EINVAL)?;
        let prepared_entry = g17_submission::prepare_g17p_cl_kick_entry(
            queue.producer,
            g17_submission::G17PClKickEntryOperands {
                descriptor_flag_4c: false,
                descriptor_flag_5c8: command.scs_enabled(),
                converted_command_timestamp: 1,
                // Registration changes the publication chronology, not the
                // command's execution dependencies. Dropping these on the
                // first kick allowed a render readback to execute before its
                // producer; subsequent kicks already preserve this slice.
                barriers: context.barriers,
                mcache: None,
                payload: [descriptor_high, queue.compute.queue_record_gpu_va()],
                event_mask: command_state.event_mask,
                rce_kind: 0,
                rce_bindings: command_state.rce_bindings,
                auxiliary: context.auxiliary,
            },
        )
        .map_err(|_| EINVAL)?;
        if prepared_entry.entry_index != 1
            || prepared_entry.entry_offset != queue.producer.geometry.entry_stride()
            || prepared_entry.current_timestamp != queue.producer.geometry.timestamp_seed()
        {
            return Err(EINVAL);
        }
        let backend = g17_submission::T8140_G17P_CL_BACKEND;
        if !g17_submission::g17p_first_bind_tag15_required(true)
            || !g17_submission::g17p_first_bind_tag16_required(backend, true)
        {
            return Err(EINVAL);
        }
        let mut entry_signal =
            [0u8; g17_submission::G17P_COMPUTE_OPTIONAL_EVENT_SIZE];
        // Tag 16 names the last retired entry. A newly installed queue has no
        // predecessor, so the protocol value is zero.
        let first_bind_old_timestamp = prepared_entry.current_timestamp.saturating_sub(1);
        g17_submission::encode_g17p_compute_optional_event(
            g17_submission::G17PComputeOptionalEvent {
                queue_id: u16::from(queue.queue_id),
                old_timestamp: first_bind_old_timestamp,
            },
            &mut entry_signal,
        )
        .map_err(|_| EINVAL)?;
        if let Ok(window) = queue.compute.g17p_preempt_argument_window() {
            // +0x1480 is where 0x1a4d0..0x1a4e8 point; +0x14a0 (this dump's
            // +0x20) is the add3 operand table. Populated for the proof path,
            // untouched for a Honeykrisp dispatch.
            self.log_record("preempt+0x1480", &window);
        }
        self.log_record("cl-kick-entry", &prepared_entry.entry);
        self.log_record("tag16-entry-signal", &entry_signal);

        let sksm_entry_aliases = queue.storage.entry_aliases()?;
        // Tag 15 installs a queue exactly once. This path is reachable only for
        // the prepared first bind; retained submissions use the repeat encoder
        // with the install flag clear.
        let install_queue = matches!(
            queue.hardware_state,
            G17PSksmQueueHardwareState::PreparedMemory
        );
        // The single most diagnostic line for a client handoff. The tag-15
        // queue-install flag must be published exactly once for each fresh QID.
        // Reusing an installed QID with another context would ask firmware to
        // reprogram a live KSMFE queue and strand one of the two routes.
        dev_info!(
            self.resources.dev().as_ref(),
            "G17PMARK tag15-install i={} z={} r={} h={} qid={} flag={} (first-bind chronology; a second install on the SAME qid in one boot is the second-client bug)\n",
            {
                let n = G17P_MARK_TAG15_INSTALLS.fetch_add(1, Ordering::Relaxed) + 1;
                n
            },
            G17P_MARK_ZERO_DURATION.load(Ordering::Relaxed),
            G17P_MARK_GATE_REBUILDS.load(Ordering::Relaxed),
            G17P_MARK_RETAINED_HANDOFFS.load(Ordering::Relaxed),
            queue.queue_id,
            install_queue as u32,
        );
        let mut group = queue
            .compute
            .stage_initial_channel_group_without_fallback_event(
                descriptor_high,
                sksm_entry_aliases,
                shared_control,
                queue.channel_control_gpu_va,
                queue.queue_id,
                queue.qos_hardware_buffer_id(),
                install_queue,
                binding.declared_context_id(),
                queue.flist_generation(),
            )?;
        // Purely precompute the host-only sequence token and notification
        // predicates. Retained state is not committed until after the SKSM
        // producer transition, matching the native correlation chronology.
        let lifecycle = g17_submission::prepare_g17p_direct_cl_lifecycle(
            queue.direct_lifecycle,
            group.queue,
            true,
            backend,
        )
        .map_err(|_| EINVAL)?;
        if !lifecycle.publish_config_update
            || !lifecycle.publish_entry_signal
            || lifecycle.correlation_token == 0
        {
            return Err(EINVAL);
        }
        if module_parameters::g17p_fault_report.value() & 1 != 0 {
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P rec tag15: install_queue={} descriptor_high={:#x} shared_control={:#x} channel_control={:#x} group(queue={:#x} optional={:#x} event={:#x} write_index={})\n",
                install_queue,
                descriptor_high,
                shared_control,
                queue.channel_control_gpu_va,
                group.queue,
                group.optional,
                group.event,
                group.write_index,
            );
        }
        self.arm_compute_completion(queue, descriptor_high)?;
        self.publish_compute_b2_once(queue)?;
        submission_phase.store(G17P_SUBMIT_PHASE_B2, Ordering::Release);
        let qos_publication = self.resources.publish_compute_qos_queue_record(
            queue.queue_id,
            queue.qos_hardware_buffer_id(),
            queue.channel_control_gpu_va,
        )?;
        let flist_target = queue.storage.reserve_flist_submission().or_else(|error| {
            self.resources.cancel_qos_publication(qos_publication)?;
            Err(error)
        })?;
        if let Err(error) = queue.compute.publish_initial_config_update_pointer(group) {
            queue.storage.cancel_unpublished_flist_submission(flist_target)?;
            self.resources.cancel_qos_publication(qos_publication)?;
            return Err(error);
        }
        submission_phase.store(G17P_SUBMIT_PHASE_TAG15, Ordering::Release);
        // Tag 15 carries the firmware-visible queue-install request. Prepare
        // every mapping before the remainder of that group is published; no
        // AP-side SKSM MMIO is valid on J700.
        self.prepare_compute_firmware_install(queue)?;
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P submitBuffer: tag15 firmware install published; writing first SKSM entry\n"
        );
        if g17p_compute_sksm_entry_enabled() {
            queue.storage.write_entry(
                prepared_entry.entry_offset as usize,
                prepared_entry.zero_length as usize,
                &prepared_entry.entry,
            )?;
        }
        fence(Ordering::SeqCst);
        if crate::g17_config::COMPUTE_RESOURCE_SNAPSHOT != 0 {
            let entry = queue.storage.snapshot_compute_entry(
                prepared_entry.entry_offset as usize,
            )?;
            dev_info!(self.resources.dev().as_ref(),
                "G17P B1 entry publication: qid={} stamp={:#x} offset={:#x} header={:#x} word08={:#x} rce-kind-word={:#x} rce0={:#x} rce1={:#x} rce2={:#x} rce3={:#x}\n",
                queue.queue_id, prepared_entry.current_timestamp,
                prepared_entry.entry_offset, entry[0], entry[1], entry[41],
                entry[42], entry[43], entry[44], entry[45]);
        }
        queue.producer = prepared_entry.producer_after;
        submission_phase.store(G17P_SUBMIT_PHASE_SKSM_ENTRY, Ordering::Release);
        let descriptor_generation =
            g17_submission::advance_g17p_compute_descriptor_generation(0, 0)
                .map_err(|_| EINVAL)?;
        queue.direct_lifecycle = lifecycle.next;
        // Native's publication order is tag 15 -> 14 -> 16; ours has been
        // 15 -> 16 -> 14. Selectable so both can be tested from one build.
        let native_order = crate::g17_config::NATIVE_ORDER != 0;
        let add_kicks = g17_submission::prepare_g17p_ksm_add_kicks_command(
            g17_submission::G17PKsmAddKicksCommand {
                queue_id: queue.queue_id,
                add_count: descriptor_generation,
                queue_priority: queue.producer.completion_selector,
                entry_stamp: prepared_entry.current_timestamp,
            },
        )
        .map_err(|_| EINVAL)?;
        self.log_record("tag14-add-kicks", &add_kicks.bytes);

        let skip = crate::g17_config::SKIP;
        if native_order {
            // Native's firmware-transport order is tag 3 -> 15 -> 14 -> 16.
            // Tag 16's handler ends with a broadcast MMIO[0x210e0]/[0x210d8]
            // = -1 "go" poke, so it must follow AddKicks, not precede it.
            // publish_initial_config_update_pointer leaves the ring producer
            // at 2. group.write_index is staged as 3 for the default order,
            // where tag 16 advances 2 -> 3 first; in native order tag 14 takes
            // slot 2 itself.
            let defer_kicks = crate::g17_config::DEFER_ADD_KICKS != 0;
            let after_kicks = if skip & 1 != 0 {
                dev_info!(self.resources.dev().as_ref(), "G17P bisect: tag-14 AddKicks SKIPPED\n");
                2
            } else if defer_kicks {
                // Native defers the AddKicks poke (host flag accel[0x662]) and
                // replays it from an IOP power-state callback. It must not be
                // published while a recovery is open: the unhalt's 128-queue
                // restore rewrites 0x21010/0x21018/0x21020 for every queue from
                // the halt-time snapshot and erases it.
                self.pending_add_kicks = Some(add_kicks.bytes);
                dev_info!(
                    self.resources.dev().as_ref(),
                    "G17P compute: tag-14 AddKicks deferred until recovery closes\n"
                );
                2
            } else {
                queue
                    .compute
                    .publish_add_kicks_command(2, &add_kicks.bytes)?
            };
            queue.producer.last_add_kicks_timestamp = prepared_entry.current_timestamp;
            fence(Ordering::SeqCst);
            submission_phase.store(G17P_SUBMIT_PHASE_ADD_KICKS, Ordering::Release);
            group.write_index = if skip & 2 != 0 {
                dev_info!(self.resources.dev().as_ref(), "G17P bisect: tag-16 EntrySignal SKIPPED\n");
                after_kicks
            } else {
                queue
                    .compute
                    .publish_initial_entry_signal_at(after_kicks, &entry_signal)?
            };
            submission_phase.store(G17P_SUBMIT_PHASE_TAG16, Ordering::Release);
        } else {
            queue
                .compute
                .publish_initial_entry_signal(&entry_signal)?;
            submission_phase.store(G17P_SUBMIT_PHASE_TAG16, Ordering::Release);
            group.write_index = queue
                .compute
                .publish_add_kicks_command(group.write_index, &add_kicks.bytes)?;
            queue.producer.last_add_kicks_timestamp = prepared_entry.current_timestamp;
            fence(Ordering::SeqCst);
            submission_phase.store(G17P_SUBMIT_PHASE_ADD_KICKS, Ordering::Release);
        }
        // Check exact graph ownership before firmware can observe this route.
        // Retain on any ambiguous publication failure; do not return its QID.
        if !self.channel_ids.publish(queue.channel_lease) { return Err(EIO); }
        let prepared = self
            .resources
            .publish_compute_channel(
                group.queue,
                group.write_index,
                queue.queue_id,
                queue.outer_channel_started,
            )?;
        queue.outer_channel_started = true;
        submission_phase.store(G17P_SUBMIT_PHASE_CL2, Ordering::Release);
        // The complete install group is now visible to firmware. From this
        // point a retry must use the retained route; publishing another
        // install for the same QID could reprogram a live queue.
        queue.hardware_state = G17PSksmQueueHardwareState::HardwareRegistered;
        queue.published_config = Some(g17_compute::ComputeConfigIdentity {
            channel_control: queue.channel_control_gpu_va,
            qos_id: queue.qos_hardware_buffer_id(),
            context_id: binding.declared_context_id(),
        });
        if lifecycle.notifications.activation != Some(prepared.doorbell) {
            return Err(EINVAL);
        }
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P compute: native-direct-first-v2 qid {} entry_offset {:#x} entry_low {:#x} entry_high {:#x} timestamp {:#x} descriptor_generation {} tag15_producer 2 tag16_producer 3 tag14_producer {} addkicks-via-channel-command=true correlation_token {}\n",
            queue.queue_id,
            prepared_entry.entry_offset,
            sksm_entry_aliases.low(),
            sksm_entry_aliases.high(),
            prepared_entry.current_timestamp,
            descriptor_generation,
            group.write_index,
            lifecycle.correlation_token
        );
        queue.submission_ordinal = 1;
        self.log_compute_ring_state(queue, "first-post-cl2");
        self.log_compute_outer_channel("first-post-cl2");
        Ok(lifecycle.notifications)
    }

    /// The steady-state (second and later) submission on an already-installed
    /// queue.
    ///
    /// Everything the first bind does exactly once is skipped here: the QID
    /// configure transaction, the B2 activation, the queue-install flag in
    /// tag 15, and above all the first-bind graph builder, which rewrites the
    /// queue header and RESETS the item-ring producer to 1. Re-running that
    /// builder for a repeat is what made the firmware's consumer cursor and
    /// our producer disagree, so the record was published and never consumed.
    ///
    /// What DOES repeat, in native's transport order:
    ///
    ///   tag 3 (a new descriptor slot) -> tag 15 -> tag 14 -> tag 16 -> CL_2
    ///
    /// appended at the live ring producer rather than at slots 0..3. macOS
    /// republishes tag 15 on every submission -- both live captures are of
    /// already-installed queues and both carry one -- with `+0x1a` clear.
    fn submit_translated_repeat_channel(
        &mut self,
        registers: &regs::Resources,
        queue: &mut G17PSksmQueue,
        submission_phase: &AtomicU32,
        binding: &G17PComputeUserBinding,
        command: &g17_uapi::TranslatedComputeCommand,
        context: G17PComputeSubmissionContext<'_>,
        add3_buffers: Option<[u64; 3]>,
    ) -> Result<g17_submission::PreparedG17PDirectClNotifications> {
        if !self.channel_ids.matches(queue.channel_lease) { return Err(EIO); }
        if queue.hardware_state != G17PSksmQueueHardwareState::HardwareRegistered
            || queue.submission_ordinal == 0
        {
            return Err(EINVAL);
        }
        let _ = registers;
        // The add3 operand table lives in the retained preempt object and is
        // unchanged by a completed submission, but the probe may have staged
        // new buffers, so honour the flag exactly as the first bind does.
        if let Some(buffers) = add3_buffers {
            queue.compute.write_g17p_add3_resource_table(buffers)?;
        }

        let ordinal = queue.submission_ordinal;
        let next_ordinal = ordinal.checked_add(1).ok_or(EOVERFLOW)?;
        // These fields have distinct meanings; do not advance them as one
        // sequence. In particular +0xf58 (queue_ordinal) indexes a TSQ pointer
        // slot and stays zero. Only +0xf50 carries the monotonic flush prefix,
        // bound from current_timestamp below. work_ordinal enters the register
        // context key and is independent of the completion sequence as well.
        let sequence_ordinal: u32 = 0;
        let work_ordinal = ordinal;
        let queue_submission = sequence_ordinal.checked_add(1).ok_or(EINVAL)?;
        let stamp = g17_submission::decode_kick_timestamp(queue.producer.current_timestamp)
            .map_err(|_| EINVAL)?;
        let descriptor_slot = stamp.stamp_index;
        let (descriptor_high, _) = queue.compute.descriptor_gpu_vas(descriptor_slot);
        let descriptor_low = binding.mappings.descriptor_gpu_va(descriptor_slot);
        let user_layout = binding.mappings.layout();
        let shared_control = queue.storage.shared_support_gpu_va();
        let objects = g17_compute::ComputeDescriptorObjects {
            scheduler_record: queue.compute.scheduler_record_gpu_va(),
            descriptor_low_alias: descriptor_low,
            dispatch_a: queue.compute.dispatch_a_gpu_va(),
            dispatch_b: queue.compute.dispatch_b_gpu_va(),
            status_a: queue.compute.status_a_gpu_va(),
            status_b: queue.compute.status_b_gpu_va(),
            shared_control,
            usage_record: queue.compute.compute_usage_record_gpu_va(),
        };
        // The usage record is one queue-owned granule the GPU writes. Earlier
        // kicks of this queue may still be executing and writing it; clear it
        // only when this publication starts an idle queue.
        if queue.producer.submitted == 0 {
            queue.compute.clear_compute_usage_slot()?;
        }
        // The timestamp this kick is about to be published under.
        let kick_timestamp = queue.producer.current_timestamp;
        let resume_checkpoints = queue.compute.with_descriptor_mut(descriptor_slot, |raw| {
            g17_uapi::build_compute_descriptor(
                command,
                g17_uapi::ComputeInternalState {
                    preempt_base: user_layout.preempt,
                    dispatch_identity: context.dispatch_identity,
                    context_id: context.context_id,
                    work_ordinal: context.work_state_word,
                    robustness: user_layout.robustness,
                    operand_state_base: user_layout.operand_state,
                    usc_flist_id: u32::from(queue.storage.flist_id()),
                },
                objects,
                g17_compute::ComputeDescriptorMetadata {
                    submit_sequence: sequence_ordinal as u64,
                    context_id: context.context_id,
                    grid_index: u32::from(queue.queue_id),
                    work_ordinal,
                    queue_submission,
                    queue_ordinal: sequence_ordinal,
                    submission_index: queue_submission,
                    support_control: 0xe0a0_0001,
                    support_flags: 0,
                },
                context.timestamps,
                raw,
            )
            .map_err(|_| EINVAL)
            .and_then(|checkpoints| {
                if let Some(work_state) = context.work_state {
                    work_state.bind_compute_descriptor(raw)?;
                }
                g17_compute::set_context_generation(raw, binding.context_generation())
                    .map_err(|_| EINVAL)?;
                // Stamp progression is independent of TSQ slot selection.
                g17_compute::set_completion_stamp_value(raw, kick_timestamp)
                    .map_err(|_| EINVAL)?;
                g17_compute::set_hardware_timestamp(raw, kick_timestamp)
                    .map_err(|_| EINVAL)?;
                Ok(checkpoints)
            })
        })?;

        // Ordinal 0 deliberately: the entry's event mask is a set of event
        // SLOT indices, not a counter. macOS's captured kick entries carry the
        // same constant mask value on every submission of a queue (0x22 for
        // CL), and the CL command record's own event quartet is likewise
        // identical across all twelve captured submissions. Only the RCE
        // bindings, which this call also derives, legitimately move -- they
        // follow the descriptor slot.
        let primary_checkpoint = u8::try_from(resume_checkpoints[0]).map_err(|_| EINVAL)?;
        let command_state = g17_submission::prepare_g17p_compute_cl_command_state(
            descriptor_low,
            0,
            primary_checkpoint,
        )
        .map_err(|_| EINVAL)?;
        let prepared_entry = g17_submission::prepare_g17p_cl_kick_entry(
            queue.producer,
            g17_submission::G17PClKickEntryOperands {
                descriptor_flag_4c: context.descriptor_flag_4c,
                descriptor_flag_5c8: command.scs_enabled(),
                converted_command_timestamp: context.converted_command_timestamp,
                barriers: context.barriers,
                mcache: context.mcache,
                payload: [descriptor_high, queue.compute.queue_record_gpu_va()],
                event_mask: command_state.event_mask,
                rce_kind: context.rce_kind,
                rce_bindings: command_state.rce_bindings,
                auxiliary: context.auxiliary,
            },
        )
        .map_err(|_| EINVAL)?;
        self.log_record("repeat-cl-kick-entry", &prepared_entry.entry);
        if prepared_entry.entry_index != descriptor_slot {
            return Err(EINVAL);
        }
        let backend = g17_submission::T8140_G17P_CL_BACKEND;
        let mut entry_signal = [0u8; g17_submission::G17P_COMPUTE_OPTIONAL_EVENT_SIZE];
        // Tag 16 names the last retired entry, never the kick being published.
        let entry_old_timestamp = queue.producer.previous_timestamp;
        g17_submission::encode_g17p_compute_optional_event(
            g17_submission::G17PComputeOptionalEvent {
                queue_id: u16::from(queue.queue_id),
                old_timestamp: entry_old_timestamp,
            },
            &mut entry_signal,
        )
        .map_err(|_| EINVAL)?;
        let sksm_entry_aliases = queue.storage.entry_aliases()?;
        // Reclaim everything the firmware has finished with, so slots come back
        // round instead of the ring being single-use.
        let trace = Self::compute_trace_enabled(ordinal);
        match queue.compute.recycle_consumed_ring_slots() {
            Ok(_) if !trace => {}
            Ok(0) => {}
            Ok(reclaimed) => dev_info!(
                self.resources.dev().as_ref(),
                "G17P compute: reclaimed {} consumed item-ring slots\n",
                reclaimed
            ),
            Err(error) => dev_err!(
                self.resources.dev().as_ref(),
                "G17P compute: item-ring reclaim failed ({:?}); publishing without it\n",
                error
            ),
        }
        match crate::g17_config::REPEAT_CONTEXT {
            0 => {}
            2 => queue
                .compute
                .write_repeat_queue_context_item(
                    descriptor_slot,
                    descriptor_high,
                    queue.queue_id,
                )?,
            _ => queue.compute.refresh_repeat_queue_context(descriptor_high)?,
        }
        let rewind = crate::g17_config::REPEAT_RING == 0;
        let config_identity = g17_compute::ComputeConfigIdentity {
            channel_control: queue.channel_control_gpu_va,
            qos_id: queue.qos_hardware_buffer_id(),
            context_id: binding.declared_context_id(),
        };
        let updates = config_identity.updates_from(queue.published_config);
        // Tags 15, 14 and 16 are channel commands firmware reads through the
        // item-ring pointer when it consumes them, which may be after the host
        // has published further kicks. Give every publication its own copy
        // (native allocates AddKicks per command from a pool) instead of
        // rewriting one window per queue graph under an unconsumed pointer --
        // including the first bind's window, which the queue graph still is.
        let record_slot = (queue.submission_ordinal as usize)
            % crate::g17_queue_limits::COMPUTE_RECORD_SLOTS;
        queue.compute.ensure_record_slot_consumed(record_slot)?;
        let group = queue.compute.stage_repeat_config_update(
            record_slot,
            sksm_entry_aliases,
            shared_control,
            queue.channel_control_gpu_va,
            queue.queue_id,
            queue.qos_hardware_buffer_id(),
            rewind,
            updates,
            binding.declared_context_id(),
            queue.flist_generation(),
        )?;
        // `queue_config_dirty` is already false here, so the lifecycle reports
        // no first-bind publications and -- because the channel identity is
        // unchanged -- no priority-2 activation either. The repeat notifies
        // with the direct EP 0x21 kick alone.
        let mut lifecycle = g17_submission::prepare_g17p_direct_cl_lifecycle(
            queue.direct_lifecycle,
            group.queue,
            true,
            backend,
        )
        .map_err(|_| EINVAL)?;
        if crate::g17_config::REPEAT_ACTIVATION != 0
            && lifecycle.notifications.activation.is_none()
        {
            lifecycle.notifications.activation =
                Some(g17_submission::encode_g17p_cl_activation(2).map_err(|_| EINVAL)?);
        }

        // The ring is genuinely circular now, so depth is unbounded rather than
        // capped at 0x500/4 = 320 submissions. Free space is measured against
        // the firmware's consumer, which hardware has proven it owns.
        let free = queue.compute.compute_ring_free_slots()?;
        if free < G17P_COMPUTE_REPEAT_RING_RECORDS {
            dev_err!(
                self.resources.dev().as_ref(),
                "G17P compute: item ring full, {} slots free but {} needed; the firmware consumer has stopped advancing\n",
                free,
                G17P_COMPUTE_REPEAT_RING_RECORDS
            );
            return Err(ENOSPC);
        }
        let producer_before = group.write_index;
        self.arm_compute_completion(queue, descriptor_high)?;
        // Order matters and is native's: the command record's pointer first,
        // then ConfigUpdate, then AddKicks, then the EntrySignal whose handler
        // ends with the broadcast "go" poke.
        let flist_target = queue.storage.reserve_flist_submission()?;
        let after_command = self.publish_repeat_record(
            queue,
            "the tag-3 command pointer",
            producer_before,
            G17PRepeatRecord::Pointer(descriptor_high),
        ).or_else(|error| {
            queue.storage.cancel_unpublished_flist_submission(flist_target)?;
            Err(error)
        })?;
        submission_phase.store(G17P_SUBMIT_PHASE_B2, Ordering::Release);
        let qos_publication = self.resources.publish_compute_qos_queue_record(
            queue.queue_id,
            queue.qos_hardware_buffer_id(),
            queue.channel_control_gpu_va,
        )?;
        let after_config = self.publish_repeat_record(
            queue,
            "the tag-15 ConfigUpdate pointer",
            after_command,
            G17PRepeatRecord::Pointer(group.optional),
        ).or_else(|error| {
            self.resources.cancel_qos_publication(qos_publication)?;
            Err(error)
        })?;
        submission_phase.store(G17P_SUBMIT_PHASE_TAG15, Ordering::Release);

        if g17p_compute_sksm_entry_enabled() {
            queue.storage.write_entry(
                prepared_entry.entry_offset as usize,
                prepared_entry.zero_length as usize,
                &prepared_entry.entry,
            )?;
        }
        fence(Ordering::SeqCst);
        if crate::g17_config::COMPUTE_RESOURCE_SNAPSHOT != 0 {
            let entry = queue.storage.snapshot_compute_entry(
                prepared_entry.entry_offset as usize,
            )?;
            dev_info!(self.resources.dev().as_ref(),
                "G17P B1 entry publication: qid={} stamp={:#x} offset={:#x} header={:#x} word08={:#x} rce-kind-word={:#x} rce0={:#x} rce1={:#x} rce2={:#x} rce3={:#x}\n",
                queue.queue_id, prepared_entry.current_timestamp,
                prepared_entry.entry_offset, entry[0], entry[1], entry[41],
                entry[42], entry[43], entry[44], entry[45]);
        }
        queue.producer = prepared_entry.producer_after;
        submission_phase.store(G17P_SUBMIT_PHASE_SKSM_ENTRY, Ordering::Release);

        let descriptor_generation =
            g17_submission::advance_g17p_compute_descriptor_generation(0, 0)
                .map_err(|_| EINVAL)?;
        let add_kicks = g17_submission::prepare_g17p_ksm_add_kicks_command(
            g17_submission::G17PKsmAddKicksCommand {
                queue_id: queue.queue_id,
                add_count: descriptor_generation,
                queue_priority: queue.producer.completion_selector,
                entry_stamp: prepared_entry.current_timestamp,
            },
        )
        .map_err(|_| EINVAL)?;
        let add_kicks_record = queue.compute.stage_record_add_kicks(record_slot, &add_kicks.bytes)?;
        let after_kicks = self.publish_repeat_record(
            queue,
            "the tag-14 AddKicks record",
            after_config,
            G17PRepeatRecord::Pointer(add_kicks_record),
        )?;
        queue.producer.last_add_kicks_timestamp = prepared_entry.current_timestamp;
        fence(Ordering::SeqCst);
        submission_phase.store(G17P_SUBMIT_PHASE_ADD_KICKS, Ordering::Release);
        // Native AGXCLCommandDescriptor (kext 0xfffffe0008862208..86227c)
        // requests a KSMKickQueueEntrySignal only for a command whose barrier
        // list names another context (or whose per-queue mode changed), and
        // the CL submit (0x88606f8, 0x8860a1c) then sets that kick's own hold
        // bit 254 and names the kick itself. An ordinary next kick on the same
        // queue -- the second CL kick of one submission -- carries no hold bit
        // and gets no EntrySignal: AddKicks alone admits it. B1's tag-16
        // handler (0x11828) is a full KSM pause handshake (0x211c8/0x211d0),
        // an entry RMW and an all-queue rescan (0x210d8/0x210e0 = -1), so
        // publishing it for every kick adds a global scheduler pause per
        // compute kick that native never issues. Keep it only where a barrier
        // crosses to another hardware queue; knob bit 25 restores the old
        // every-kick publication for same-boot A/B.
        let cross_queue_barrier = context.barriers.iter()
            .any(|barrier| barrier.queue_id != queue.queue_id);
        let publish_entry_signal = cross_queue_barrier
            || crate::g17_timing_buffer::diag_knob(
                crate::g17_timing_buffer::DIAG_CL_ENTRY_SIGNAL_EVERY_KICK);
        let write_index = if publish_entry_signal {
            let entry_signal_record =
                queue.compute.stage_record_entry_signal(record_slot, &entry_signal)?;
            self.publish_repeat_record(
                queue,
                "the tag-16 EntrySignal record",
                after_kicks,
                G17PRepeatRecord::Pointer(entry_signal_record),
            )?
        } else {
            after_kicks
        };
        queue.compute.note_record_slot_published(record_slot, write_index);
        submission_phase.store(G17P_SUBMIT_PHASE_TAG16, Ordering::Release);

        // Depth 320 used to fail BEFORE reaching this point: the host-side
        // publisher compared the ring transition's `(producer + 1) % count`
        // against a locally computed `producer + 1`, so the fourth record of
        // submission 320 -- the one that lands on the ring's last slot -- was
        // refused with EBUSY. That is fixed in `publish_command_pointer_at` and
        // its two siblings, so a wrapped write index now actually reaches the
        // outer channel. Whether the firmware reads a producer of 0 as
        // "wrapped" or as "empty" is a separate, still-open question, and
        // `g17p_ring_wrap=1` reports the head as `count` instead so one boot
        // decides it. With `g17p_ring_limit=32` the wrap arrives on submission
        // 8, so that boot costs 20 iterations rather than 400.
        let reported_write_index = if write_index == 0
            && crate::g17_config::RING_WRAP != 0
        {
            let entries = queue.compute.compute_ring_entries()?;
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P compute: item-ring producer wrapped to 0; reporting write index {} to the outer channel\n",
                entries
            );
            entries
        } else {
            write_index
        };
        let prepared = self
            .resources
            .publish_compute_channel(group.queue, reported_write_index, queue.queue_id, true)?;
        submission_phase.store(G17P_SUBMIT_PHASE_CL2, Ordering::Release);
        let _ = prepared;
        queue.direct_lifecycle = lifecycle.next;
        queue.submission_ordinal = next_ordinal;
        queue.published_config = Some(config_identity);
        self.log_compute_ring_state(queue, "repeat-post-cl2");
        if !trace {
            return Ok(lifecycle.notifications);
        }
        self.log_compute_outer_channel("repeat-post-cl2");
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P compute: repeat submission {} qid {} descriptor-slot {} descriptor {:#x} stamp {:#x} ring {} -> {} activation {:?} kick {:?}\n",
            ordinal,
            queue.queue_id,
            descriptor_slot,
            descriptor_high,
            prepared_entry.current_timestamp,
            producer_before,
            write_index,
            lifecycle.notifications.activation,
            lifecycle.notifications.direct_kick
        );
        Ok(lifecycle.notifications)
    }

    /// Build and publish one complete userspace compute command.
    ///
    /// Descriptor and CL-entry slots share the configured stamp lifetime. The
    /// descriptor is written first, then its upper address and the retained
    /// queue record are placed in the CL payload before the ordered SKSM kick.
    pub(crate) fn submit_translated_compute(
        &mut self,
        registers: &regs::Resources,
        queue: &mut G17PSksmQueue,
        submission_phase: &AtomicU32,
        binding: &G17PComputeUserBinding,
        command: &g17_uapi::TranslatedComputeCommand,
        context: G17PComputeSubmissionContext<'_>,
    ) -> Result<Option<g17_submission::PreparedG17PDirectClNotifications>> {
        if queue.independent_context && context.work_state.is_none() { return Err(EINVAL); }
        if context.work_state.is_some_and(|state| state.word() != context.work_state_word) {
            return Err(EINVAL);
        }
        self.log_mixed_resource_census(queue, false);
        if queue.scratch != self.sksm_scratch()
            || queue.uat_owner != self.uat().ttb_base()
        {
            return Err(EINVAL);
        }
        // Prove the kick's translation regime before anything is published.
        //
        // The kick declares hardware context 2, and every later workload
        // context 3 (`T8140_COMPUTE_CTXS`). If those entries do not name this
        // binding's VM the GPU has no root to walk at all: the work is
        // accepted and silently never runs -- no fault, no blamed queue, a
        // zero-duration retire, and a destination buffer that comes back
        // untouched. That is indistinguishable from "the shader did nothing",
        // which is exactly how it stayed hidden.
        //
        // `install_t8140_compute_context_alias` runs only on the bind that
        // creates a client's retained binding. Every repeat submission on that
        // binding, and any submission after another VM was created or
        // destroyed in between, previously reached the kick with nobody having
        // re-checked those two entries.
        if let Some(execution) = binding._execution_context.as_ref() {
            if context.context_id != execution.context_id() || !execution.is_current() {
                return Err(EFAULT);
            }
            if crate::util::g17p_verbose() {
                dev_info!(self.resources.dev().as_ref(),
                    "G17P independent compute root: qid={} context={} vm={} root={:#x}\n",
                    queue.queue_id, execution.context_id(), execution.vm_id(), execution.root());
            }
        } else {
        self.resources
            .uat()
            .ensure_t8140_compute_context_alias(binding.vm_bind())?;
        {
            let roots = self.resources.uat().t8140_compute_context_roots();
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P vm-audit(pre-kick): vm {} root={:#x} ctx1 ttb0={:#x} ctx2 ttb0={:#x} ttb1={:#x} ctx3 ttb0={:#x} ttb1={:#x}\n",
                binding.vm_bind().vm_id(),
                binding.vm_bind().root(),
                roots[1].0,
                roots[2].0,
                roots[2].1,
                roots[3].0,
                roots[3].1,
            );
        }
        }
        // Which chronology a command needs is a property of the *queue*, not
        // of the command's shape. A queue that has never been configured on
        // the SKSM bridge needs the first-bind sequence above; one that has
        // needs the post-registration sequence below. `g17p_add3_buffers` only
        // says whether the probe wants its private operand table written into
        // the preempt object, so it must not be what decides the route -- that
        // coupling is exactly why a Honeykrisp dispatch, which carries its
        // bindings in the command stream and therefore sets no flag and
        // declares no attachments, fell through to the ENOTSUPP gate below
        // while the add3 probe reached the firmware.
        let first_bind = queue.hardware_state == G17PSksmQueueHardwareState::PreparedMemory
            && queue.submission_ordinal == 0;
        // Which of the three chronologies this submit takes. A submit that
        // produces no record dump at all took a path with no logging, so name
        // the route before branching -- otherwise "no output" is ambiguous
        // between "different path" and "never got here".
        if module_parameters::g17p_fault_report.value() & 1 != 0 {
            let attachments = command.attachments.as_slice();
            let route = if first_bind {
                "first-bind"
            } else if queue.hardware_state
                != G17PSksmQueueHardwareState::HardwareRegistered
            {
                "REFUSED-not-registered"
            } else if queue.submission_ordinal != 0 {
                "repeat"
            } else {
                "steady-state"
            };
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P rec route={} hardware_state={:?} submission_ordinal={} first_bind={} add3_proof={}\n",
                route,
                queue.hardware_state,
                queue.submission_ordinal,
                first_bind,
                command.g17p_add3_buffers.is_some(),
            );
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P rec uapi: usc_exec_base={:#x} cs=[{:#x},{:#x}) term={:#x} sampler={:#x}/{} attachments={}\n",
                command.usc_exec_base,
                command.control_stream_base,
                command.control_stream_end,
                command.control_stream_terminator,
                command.sampler_heap,
                command.sampler_count,
                attachments.len(),
            );
            for (index, entry) in attachments.iter().enumerate() {
                dev_info!(
                    self.resources.dev().as_ref(),
                    "G17P rec uapi: attachment[{}] address={:#x} size={:#x}\n",
                    index,
                    entry.address,
                    entry.size,
                );
            }
        }
        // Which chronology to run is decided by the queue, never by the
        // command's flags. The add3 probe used to force the first-bind route
        // unconditionally, which meant its second submission either hit the
        // `submission_ordinal != 0` gate or re-ran the first-bind builder on a
        // live queue and reset the item-ring producer under the firmware.
        if first_bind {
            return self
                .submit_translated_first_bind_channel(
                    registers,
                    queue,
                    submission_phase,
                    binding,
                    command,
                    context,
                    command.g17p_add3_buffers,
                )
                .map(Some);
        }

        // Post-registration path. It assumes the tag/configure chronology has
        // already run, so a still-prepared queue is refused here exactly as
        // before the queue has been registered.
        if queue.hardware_state != G17PSksmQueueHardwareState::HardwareRegistered {
            return Err(ENOTSUPP);
        }

        if queue.submission_ordinal != 0 {
            return self
                .submit_translated_repeat_channel(
                    registers,
                    queue,
                    submission_phase,
                    binding,
                    command,
                    context,
                    command.g17p_add3_buffers,
                )
                .map(Some);
        }

        let timestamp = g17_submission::decode_kick_timestamp(queue.producer.current_timestamp)
            .map_err(|_| EINVAL)?;
        let (descriptor_high, _) = queue.compute.descriptor_gpu_vas(timestamp.stamp_index);
        let descriptor_low = binding
            .mappings
            .descriptor_gpu_va(timestamp.stamp_index);
        let user_layout = binding.mappings.layout();
        let checkpoint_internal = g17_uapi::ComputeInternalState {
            preempt_base: user_layout.preempt,
            dispatch_identity: context.dispatch_identity,
            context_id: context.context_id,
            work_ordinal: queue.submission_ordinal,
            robustness: user_layout.robustness,
            operand_state_base: user_layout.operand_state,
            usc_flist_id: u32::from(queue.storage.flist_id()),
        };
        let resume_checkpoints =
            g17_uapi::compute_resume_checkpoints(command, checkpoint_internal)
                .map_err(|_| EINVAL)?;
        let primary_checkpoint = u8::try_from(resume_checkpoints[0]).map_err(|_| EINVAL)?;
        let command_state = g17_submission::prepare_g17p_compute_cl_command_state(
            descriptor_low,
            u64::from(queue.submission_ordinal),
            primary_checkpoint,
        )
        .map_err(|_| EINVAL)?;
        let payload = [descriptor_high, queue.compute.queue_record_gpu_va()];
        let operands = command.entry_operands(g17_uapi::ComputeEntryContext {
            descriptor_flag_4c: context.descriptor_flag_4c,
            converted_command_timestamp: context.converted_command_timestamp,
            barriers: context.barriers,
            mcache: context.mcache,
            payload,
            event_mask: command_state.event_mask,
            rce_kind: context.rce_kind,
            rce_bindings: command_state.rce_bindings,
            auxiliary: context.auxiliary,
        });
        let prepared = g17_submission::prepare_g17p_cl_kick_entry(queue.producer, operands)
            .map_err(|_| EINVAL)?;
        if prepared.entry_index != timestamp.stamp_index {
            return Err(EINVAL);
        }
        // The fall-through path. It publishes a kick entry WITHOUT ever having
        // run the first-bind chronology (tag 3 -> 15 -> QID configure -> SKSM
        // entry -> 14 -> 16 -> CL_2) for this binding, so name it loudly.
        self.log_record("fallthrough-cl-kick-entry", &prepared.entry);

        let ordinal = queue.submission_ordinal;
        let next_ordinal = ordinal.checked_add(1).ok_or(EOVERFLOW)?;
        let queue_submission = ordinal.checked_add(1).ok_or(EINVAL)?;
        let preempt_base = user_layout.preempt;
        let robustness = user_layout.robustness;
        let operand_state_base = user_layout.operand_state;
        let shared_control = queue.storage.shared_support_gpu_va();
        let objects = g17_compute::ComputeDescriptorObjects {
            scheduler_record: queue.compute.scheduler_record_gpu_va(),
            descriptor_low_alias: descriptor_low,
            dispatch_a: queue.compute.dispatch_a_gpu_va(),
            dispatch_b: queue.compute.dispatch_b_gpu_va(),
            status_a: queue.compute.status_a_gpu_va(),
            status_b: queue.compute.status_b_gpu_va(),
            shared_control,
            usage_record: queue.compute.compute_usage_record_gpu_va(),
        };
        queue.compute.clear_compute_usage_slot()?;
        let kick_timestamp = queue.producer.current_timestamp;
        queue
            .compute
            .with_descriptor_mut(prepared.entry_index, |raw| {
                g17_uapi::build_compute_descriptor(
                    command,
                    g17_uapi::ComputeInternalState {
                        preempt_base,
                        dispatch_identity: context.dispatch_identity,
                        context_id: context.context_id,
                        work_ordinal: context.work_state_word,
                        robustness,
                        operand_state_base,
                        usc_flist_id: u32::from(queue.storage.flist_id()),
                    },
                    objects,
                    g17_compute::ComputeDescriptorMetadata {
                        submit_sequence: ordinal as u64,
                        context_id: context.context_id,
                        grid_index: u32::from(queue.queue_id),
                        work_ordinal: ordinal,
                        queue_submission,
                        queue_ordinal: 0,
                        submission_index: queue_submission,
                        support_control: 0xe0a0_0001,
                        support_flags: 0,
                    },
                    context.timestamps,
                    raw,
                )
                .map_err(|_| EINVAL)
                .and_then(|checkpoints| {
                    if let Some(work_state) = context.work_state {
                    work_state.bind_compute_descriptor(raw)?;
                }
                g17_compute::set_context_generation(raw, binding.context_generation())
                        .map_err(|_| EINVAL)?;
                    g17_compute::set_completion_stamp_value(raw, kick_timestamp)
                        .map_err(|_| EINVAL)?;
                    g17_compute::set_hardware_timestamp(raw, kick_timestamp)
                        .map_err(|_| EINVAL)?;
                    Ok(checkpoints)
                })
            })?;

        fence(Ordering::SeqCst);
        // The alternate direct-entry route can expose an SKSM entry and pool
        // reservation before add_sksm_kicks returns. Treat every subsequent
        // error as partial publication, never as an unpublished retry.
        submission_phase.store(G17P_SUBMIT_PHASE_B2, Ordering::Release);
        let timestamp = self.publish_compute_entry(registers, queue, prepared)?;
        queue.submission_ordinal = next_ordinal;
        let _ = timestamp;
        Ok(None)
    }

    /// Report the record register 0x14070 names.
    ///
    /// Pure observation of driver-owned DRAM. `clear_compute_usage_slot` zeroed
    /// it before publication, so a record that is still all zeroes here means
    /// the GPU wrote nothing through the shared 0x14070/+0xfcb record. Words 0
    /// and 1 are the two the kext's `AGXUSCPrivMemPool::updateStatistics`
    /// consumes (mac17p `0xfffffe0008aa6bf8` and `0xfffffe0008aa6c40`).
    fn log_compute_usage_slot(&mut self, queue: &mut G17PSksmQueue, phase: &str) {
        if crate::module_parameters::g17p_compute_usage_slot.value() == 0 {
            return;
        }
        let address = queue.compute.compute_usage_slot_gpu_va();
        match queue.compute.read_compute_usage_slot() {
            Ok(words) => dev_info!(
                self.resources.dev().as_ref(),
                "G17P compute usage[{}]: qid={} slot={:#x} word0={:#x} word1={:#x} rest={:#x?}\n",
                phase,
                queue.queue_id,
                address,
                words[0],
                words[1],
                &words[2..]
            ),
            Err(error) => dev_err!(
                self.resources.dev().as_ref(),
                "G17P compute usage[{}]: qid={} slot={:#x} unreadable ({:?})\n",
                phase,
                queue.queue_id,
                address,
                error
            ),
        }
    }

    /// Log the queue-graph ring cursors. DRAM only -- safe on a timeout path,
    /// unlike anything that reads the sgx register file once the cores have
    /// powered back down.
    pub(crate) fn log_compute_ring_state(
        &mut self,
        queue: &mut G17PSksmQueue,
        label: &'static str,
    ) {
        // Never gate the failure samples: they are the whole point.
        if !Self::compute_trace_enabled(queue.submission_ordinal)
            && label != "timeout"
            && label != "publish-failed"
        {
            return;
        }
        let state = match queue.compute.read_compute_ring_state() {
            Ok(state) => state,
            Err(error) => {
                dev_err!(
                    self.resources.dev().as_ref(),
                    "G17P ring[{}]: unreadable ({:?})\n",
                    label,
                    error
                );
                return;
            }
        };
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P ring[{}]: queue {:#x} consumer {} producer {} mirror {:#x} count {:#x} submitted {} ordinal {} reclaim-cursor {} reclaimed-total {} items {:#x?}\n",
            label,
            state.queue,
            state.consumer,
            state.producer,
            state.mirror,
            state.count,
            queue.producer.submitted,
            queue.submission_ordinal,
            state.reclaimed,
            state.reclaimed_total,
            state.items
        );
    }

    /// Read retained and in-flight ownership without selecting another slot.
    /// The timestamp arrays are pass-wide views, NOT separate TA/3D intervals.
    fn log_mixed_render_owner(
        checkpoint: &str,
        compute_qid: u8,
        slot: u8,
        host: (u64, bool, Option<u32>, bool, Option<u64>),
        storage: Option<&mut g17_resources::G17PUserRenderStorage>,
    ) {
        let retained = storage.map(|storage| {
            (
                storage.descriptor_context_id(),
                storage.retained_hardware_buffer_id(),
                storage.render_pool_id(),
                storage.parameter_manager_generation(),
                storage.tvb_capacity(),
                storage.context_update_pending(),
                storage.snapshot(host.0),
            )
        });
        pr_info!("G17P mixed-resource render[{}]: compute-qid={} slot={} host(ordinal,tracker,active-hbid,published,vm)={:?} retained(context,hbid,pool,pm-generation,tvb-blocks,context-pending,queues-job-list-pass-stamps)={:x?}\n",
            checkpoint, compute_qid, slot, host, retained);
    }

    /// Diagnostic DRAM census; no register reads, queue selection, retirement,
    /// scheduling changes or error propagation. Firmware may update memory
    /// between reads; report raw witnesses rather than claiming active engines.
    fn log_mixed_resource_census(&mut self, queue: &mut G17PSksmQueue, fault: bool) {
        let pair_slot = 0;
        if !g17p_mixed_resource_sample(
            module_parameters::g17p_mixed_resource_probe.value(), queue.submission_ordinal, fault,
        ) { return; }
        let started = kernel::time::Instant::<kernel::time::Monotonic>::now();
        let checkpoint = if fault { "compute-fault" } else { "compute-before-publish" };
        pr_info!("G17P mixed-resource owners[{}]: qid={} compute-ordinal={} qos-hbid={} selected-render={} hb-free={:x?} hb-reserved={:x?} teardowns={} displaced-graphs={} flist={:?}\n",
            checkpoint, queue.queue_id, queue.submission_ordinal, queue.qos_hardware_buffer_id(),
            pair_slot, self.render_hardware_buffer_ids.free,
            self.render_hardware_buffer_ids.reserved,
            self.logical_queue_teardowns.len(),
            self.logical_queue_teardowns.iter().map(|teardown| teardown.graphs.len()).sum::<usize>(),
            self.render_flist_owners[self.render_slots.get(pair_slot).flist_index]);
        let active_slot = pair_slot;
        let slot = self.render_slots.get_mut(pair_slot);
        Self::log_mixed_render_owner(
            checkpoint, queue.queue_id, active_slot,
            (slot.submission_ordinal, slot.tracker.is_some(),
                slot.hardware_buffer_id, slot.hardware_buffer_published,
                slot.bind.as_ref().map(|binding| binding.vm_id())),
            slot.storage.as_deref_mut(),
        );
        for (index, slot) in self.render_slots.iter_mut() {
            if index == active_slot { continue; }
            if let Some(slot) = slot {
                Self::log_mixed_render_owner(
                    checkpoint, queue.queue_id, index,
                    (slot.submission_ordinal, slot.tracker.is_some(), slot.hardware_buffer_id,
                        slot.hardware_buffer_published, slot.bind.as_ref().map(|binding| binding.vm_id())),
                    slot.storage.as_deref_mut(),
                );
            } else {
                Self::log_mixed_render_owner(
                    checkpoint, queue.queue_id, index, (0, false, None, false, None), None,
                );
            }
        }
        let uma = [0u8, queue.storage.flist_id() as u8]
            .map(|slot| (slot, self.resources.snapshot_compute_uma(slot)));
        let flist = queue.storage.snapshot_compute_flist();
        let qos = self.resources.snapshot_compute_qos_owner(queue.queue_id, queue.qos_hardware_buffer_id());
        pr_info!("G17P mixed-resource pools[{}]: qid={} uma-slots={:x?} compute-flist={:x?} compute-qos={:x?}\n",
            checkpoint, queue.queue_id, uma, flist, qos);
        self.resources.log_render_uma_growth(checkpoint);
        pr_info!("G17P mixed-resource sample[{}]: qid={} census-ns={} (sequential DRAM and logging, not an atomic execution snapshot)\n",
            checkpoint, queue.queue_id, started.elapsed().as_nanos().max(0));
    }

    /// Bounded, read-only timeout evidence for the retained asynchronous owner.
    /// The shared outer slot is the most recent publication, not necessarily
    /// this queue's slot. Samples are sequential DRAM observations, not an
    /// atomic hardware snapshot; no firmware pause or MMIO is attempted.
    pub(crate) fn log_timeout_halt_masks(&self, label: &'static str) {
        let read = |link| {
            pgtable::LiveFirmwareU64Probe::new(g17p_gfx_link_to_physical(link))
                .and_then(|probe| probe.observe_mask(u64::MAX, 1))
                .map(|sample| sample.first)
        };
        // Native EnabledQueuesResume excludes both 583b8 and 58158.
        // 583c8 is requested halt; 583d8 is sampled hardware stopped status;
        // 583e8 is the candidate mask used by the resume selection path.
        let masks: [Result<u64>; 8] = core::array::from_fn(|index|
            read(0xffff_fc00_0005_83b8 + index as u64 * 8));
        dev_err!(self.resources.dev().as_ref(),
            "G17P timeout halt-masks[{}]: aggregate,requested,sampled,resume-candidate={:x?} additional-exclusion={:x?} pause-reasons={:x?}\n",
            label, masks,
            [read(0xffff_fc00_0005_8158), read(0xffff_fc00_0005_8160)],
            read(0xffff_fc00_0012_8a68));
    }

    /// `log_async_compute_timeout`'s snapshot and report half: returns the
    /// observed QIDs and the incomplete render-parent QIDs.
    #[cold]
    #[inline(never)]
    fn log_async_compute_timeout_report(&mut self, queue: &mut G17PSksmQueue) -> (u128, u128) {
        let progress = self.resources.firmware_gpu_progress_snapshot();
        dev_info!(self.resources.dev().as_ref(),
            "G17P global GPU progress[{}]: (recovery-epoch,counter)={:x?}; not per-job completion\n",
            "async-compute", progress);
        let mut observed_qids = 1u128 << queue.queue_id;
        self.log_mixed_resource_census(queue, true);
        let pool_id = queue.storage.flist_id() as u8;
        let uma = self.resources.snapshot_compute_uma(pool_id);
        let uma_slots = [0u8, 1, 2, 3]
            .map(|slot| (slot, self.resources.snapshot_compute_uma(slot)));
        let flist = queue.storage.snapshot_compute_flist();
        let qos_owner = self.resources.snapshot_compute_qos_owner(
            queue.queue_id,
            queue.qos_hardware_buffer_id(),
        );
        if module_parameters::g17p_fault_report.value() & 4 != 0 {
            let read = |link: u64| -> Result<u64> {
                let phys = g17p_gfx_link_to_physical(link);
                pgtable::LiveFirmwareU64Probe::new(phys)
                    .and_then(|probe| probe.observe_mask(u64::MAX, 1))
                    .map(|sample| sample.first)
            };
            let pool_id = u64::from(queue.storage.flist_id());
            let qid = u64::from(queue.queue_id);
            let row_base = G17P_FLIST_ROW_LINK_BASE + pool_id * G17P_FLIST_ROW_STRIDE;
            let row: [Result<u64>; 29] =
                core::array::from_fn(|index| read(row_base + index as u64 * 8));
            let active = [
                read(G17P_FLIST_ACTIVE_BITMAP_LINK),
                read(G17P_FLIST_ACTIVE_BITMAP_LINK + 8),
                read(G17P_FLIST_ACTIVE_BITMAP_LINK + 16),
                read(G17P_FLIST_ACTIVE_BITMAP_LINK + 24),
            ];
            let dm2 = [
                read(G17P_DM_ACTIVE_BITMAP_LINK_BASE + 2 * 0x10),
                read(G17P_DM_ACTIVE_BITMAP_LINK_BASE + 2 * 0x10 + 8),
            ];
            let park = G17P_KSM_PARK_RECORD_LINK_BASE + qid * 0x28;
            let scheduler = [
                read(G17P_QID_OWNER_LINK_BASE + qid * 8),
                read(G17P_READY_MASK_LINK_BASE),
                read(park + 0x418),
                read(park + 0x400),
                read(G17P_QUEUE_STATE_LINK_BASE + qid * 0x28),
                read(G17P_QUEUE_STATE_LINK_BASE + qid * 0x28 + 8),
                read(G17P_QUEUE_FLAG_LINK_BASE + qid * 0x28 + 0x40),
            ];
            let scheduler_masks: [Result<u64>; 8] = core::array::from_fn(|index| {
                read(G17P_KSM_SCHEDULER_MASKS_LINK + index as u64 * 8)
            });
            let runnable = [
                read(G17P_KSM_RUNNABLE_MASK_LINK),
                read(G17P_KSM_RUNNABLE_MASK_LINK + 8),
            ];
            let park_record: [Result<u64>; 5] =
                core::array::from_fn(|word| read(park + 0x3f8 + word as u64 * 8));
            let irq_handoff: [Result<u64>; 5] =
                core::array::from_fn(|word| read(G17P_KSM_IRQ_HANDOFF_LINKS[word]));
            let worker_busy = read(G17P_FLIST_WORKER_BUSY_LINK);
            let worker_rings: [[Result<u64>; 2]; 4] =
                core::array::from_fn(|ring| {
                    core::array::from_fn(|word| {
                        read(G17P_FLIST_WORKER_RING_LINKS[ring] + word as u64 * 8)
                    })
                });
            dev_err!(self.resources.dev().as_ref(),
                "G17P fault-only B1 scheduler: qid={} pool={} qid-owner/ready/park-counters/park-deadline/qstate0/qstate8/qflag={:x?} dm2-active={:x?} pool-active={:x?} masks(requested,sampled,deferred,next)={:x?} runnable={:x?} park-record={:x?} irq-handoff={:x?} flist-worker-busy={:x?} worker-rings={:x?}\n",
                queue.queue_id, pool_id, scheduler, dm2, active,
                scheduler_masks, runnable, park_record, irq_handoff,
                worker_busy, worker_rings);
            dev_err!(self.resources.dev().as_ref(),
                "G17P fault-only B1 FList row: qid={} pool={} link={:#x} qwords={:x?}\n",
                queue.queue_id, pool_id, row_base, row);
        }
        let usage = queue.compute.read_compute_usage_slot();
        let descriptor_usage = queue.compute.read_compute_descriptor_usage_slot();
        dev_err!(self.resources.dev().as_ref(),
            "G17P async-timeout spill: qid={} qos-hbid={} uma-slot={} uma={:x?} uma-slots={:x?} flist-index={} flist-pool={} flist-va={:#x} flist={:x?} reg-14070-usage={:x?} descriptor-fcb-usage={:x?}\n",
            queue.queue_id, queue.qos_hardware_buffer_id(), pool_id, uma, uma_slots, pool_id, pool_id,
            queue.storage.shared_support_gpu_va(), flist,
            usage, descriptor_usage);
        let ring = queue.compute.read_compute_ring_state();
        let outer = self.resources.snapshot_compute_channel();
        let submission = queue.compute.snapshot_compute_submission(queue.completion_descriptor);
        if crate::g17_config::COMPUTE_RESOURCE_SNAPSHOT != 0 {
            match queue.publication_resource_snapshot.as_ref() {
                Some((sample, sample_flist)) => dev_err!(self.resources.dev().as_ref(),
                    "G17P async-timeout B1 publication: qid={} descriptor={:#x} context={} grid={} state={:#x} count={:#x} usage={:#x} index={:#x} register={:x?} flist-pointer={:#x} flist+08={:#x} flist+48={:#x}\n",
                    queue.queue_id, sample.descriptor, sample.context, sample.grid,
                    sample.resource_state, sample.primary_register_count,
                    sample.resource_usage, sample.resource_index,
                    sample.resource_register, sample.flist_pointer,
                    sample_flist[2], sample_flist[18]),
                None => dev_err!(self.resources.dev().as_ref(),
                    "G17P async-timeout B1 publication: qid={} unavailable\n",
                    queue.queue_id),
            }
            match submission.as_ref() {
                Ok(sample) => dev_err!(self.resources.dev().as_ref(),
                    "G17P async-timeout B1 halt: qid={} descriptor={:#x} context={} grid={} state={:#x} count={:#x} usage={:#x} index={:#x} register={:x?} flist-pointer={:#x}\n",
                    queue.queue_id, sample.descriptor, sample.context, sample.grid,
                    sample.resource_state, sample.primary_register_count,
                    sample.resource_usage, sample.resource_index,
                    sample.resource_register, sample.flist_pointer),
                Err(error) => dev_err!(self.resources.dev().as_ref(),
                    "G17P async-timeout B1 halt: qid={} descriptor unavailable ({:?})\n",
                    queue.queue_id, error),
            }
            match flist.as_ref() {
                Ok(sample_flist) => dev_err!(self.resources.dev().as_ref(),
                    "G17P async-timeout B1 halt FList: qid={} flist+08={:#x} flist+48={:#x}\n",
                    queue.queue_id, sample_flist[2], sample_flist[18]),
                Err(error) => dev_err!(self.resources.dev().as_ref(),
                    "G17P async-timeout B1 halt FList: qid={} unavailable ({:?})\n",
                    queue.queue_id, error),
            }
        }
        self.log_compute_usage_slot(queue, "timeout");
        let roots = (|| -> Result<(u64, u64)> {
            let decoded = submission.as_ref().map_err(|error| *error)?;
            let context = u8::try_from(decoded.context).map_err(|_| EINVAL)?;
            Ok(self.uat().t8140_selected_context_roots([context; 4])?[0])
        })();
        let control = queue.logical_channel_control.as_deref()
            .or(self.compute_channel_control.as_ref())
            .ok_or(EINVAL)
            .and_then(g17_resources::G17PClChannelControl::snapshot_prefix);
        let b2_channel_control = self.resources.compute_b2_channel_control();
        let expected_channel_control = self.compute_channel_control.as_ref().map(|c| c.gpu_va());
        let qos_rows = [queue.queue_id, 4u8, 5, 6, 7]
            .map(|qid| (qid, self.resources.qos_queue_record(qid)));
        let entry = (|| -> Result<_> {
            if !queue.producer.previous_valid { return Err(EINVAL); }
            let last = g17_submission::prepare_kick_submission(
                queue.producer.previous_timestamp, queue.queue_id,
                queue.producer.geometry.stamp_count(), queue.producer.geometry.entry_stride(),
            ).map_err(|_| EINVAL)?;
            Ok((last.entry_offset,
                queue.storage.snapshot_compute_entry(last.entry_offset as usize)?))
        })();
        // The fork this whole class of wedge turns on, sampled at the moment
        // it fails rather than inferred afterwards. Three outcomes, three
        // different bugs, and until now the log could not tell them apart:
        //
        //   completed < ordinal   the firmware never retired the work -- a
        //                         scheduling/dispatch failure, and the
        //                         publication-time TSQ check should already
        //                         have said so;
        //   completed == ordinal, stamp == 0
        //                         retired, but the completion stamp was never
        //                         made visible -- the poller has been returning
        //                         EAGAIN, which is swallowed silently;
        //   completed == ordinal, stamp != 0
        //                         retired and stamped, so the loss is in the
        //                         completion RECORD: the poller has been
        //                         returning Ok(None) because no record folded.
        let counters = queue.compute.read_compute_submitted_counters();
        let stamp_word = queue.compute.read_compute_stamp();
        // The firmware's own dependency record for THIS queue.
        //
        // Every kick declares an implicit in-order self-dependency on its own
        // previous stamp -- the encoder writes {own qid, converted_command
        // _timestamp - 1} -- and the firmware's rule is: satisfied iff the
        // record is valid (byte 0) and dep_stamp <= completed_stamp. So a queue
        // reading `completed = target - 1` is exactly the arithmetic of one kick
        // parked on a self-dependency that never resolved, which is also the
        // render event-control class's signature (submitted=386 completed=385).
        // Nothing has ever sampled this record at an async-compute timeout.
        //
        // Deliberately NOT gated on `g17p_dep_records`: that parameter is
        // overloaded and also enables `log_render_sksm_entries`, a ~19 KB
        // hexdump per call at pre-doorbell that has itself caused
        // VK_ERROR_DEVICE_LOST. This is three words on a path that only runs
        // when a submission has already timed out.
        let dependency = {
            let phys = G17P_GFX_PHYSICAL_DATA_BASE
                + (G17P_DEPENDENCY_RECORD_LINK_BASE - G17P_GFX_LINK_DATA_BASE)
                + u64::from(queue.queue_id) * G17P_DEPENDENCY_RECORD_STRIDE;
            let valid = pgtable::LiveFirmwareU64Probe::new(phys)
                .and_then(|p| p.observe_mask(u64::MAX, 1))
                .map(|o| o.first);
            let completed = pgtable::LiveFirmwareU64Probe::new(phys + 8)
                .and_then(|p| p.observe_mask(u64::MAX, 1))
                .map(|o| o.first);
            (valid, completed)
        };
        dev_err!(self.resources.dev().as_ref(),
            "G17P async-timeout dependency: qid={} record={:#x} valid-word={:x?} completed-stamp={:x?} -- own-QID record only; enabled parent records use signed 40-bit timestamp comparison\n",
            queue.queue_id,
            G17P_DEPENDENCY_RECORD_LINK_BASE
                + u64::from(queue.queue_id) * G17P_DEPENDENCY_RECORD_STRIDE,
            dependency.0,
            dependency.1);
        // `consumed-end-floor` is `G17PComputeCompletionBatchSlot::
        // last_consumed_end`, the value `fold_record` compares every candidate
        // against with `pair_b_end <= floor -> reject`. That rejection is
        // PERMANENT for the record, and nothing has ever printed the floor, so
        // a record the decoded dump reports as `matched-rec Some(n)` could be
        // one the batch reader had already refused on this comparison and the
        // two readers' disagreement was invisible. Printed next to the record
        // dump's per-record `b-end` so the two can be compared directly.
        dev_err!(self.resources.dev().as_ref(),
            "G17P async-timeout retirement: qid={} ordinal={} legacy-private-counters(target,completed)={:x?} stamp-word={:x?} stamp-armed={} awaited-descriptor={:#x} consumed-end-floor={:#x} independent-context={}\n",
            queue.queue_id, queue.submission_ordinal, counters, stamp_word,
            queue.completion_stamp_armed, queue.completion_descriptor,
            queue.completion_last_end, queue.independent_context);
        dev_err!(self.resources.dev().as_ref(),
            "G17P async-timeout owner: qid={} hbid={} descriptor={:#x} ordinal={} producer={:?} direct={:?} hardware={:?} qos-owner={:x?}\n",
            queue.queue_id, queue.qos_hardware_buffer_id(), queue.completion_descriptor,
            queue.submission_ordinal, queue.producer, queue.direct_lifecycle,
            queue.hardware_state, qos_owner);
        dev_err!(self.resources.dev().as_ref(),
            "G17P async-timeout ring: {:x?}\n", ring);
        dev_err!(self.resources.dev().as_ref(),
            "G17P async-timeout outer-latest-not-owner: {:x?}\n", outer);
        dev_err!(self.resources.dev().as_ref(),
            "G17P async-timeout submission: {:x?} descriptor-context-ttbat={:x?}\n", submission, roots);
        dev_err!(self.resources.dev().as_ref(),
            "G17P async-timeout channel-control: {:x?}\n", control);
        dev_err!(self.resources.dev().as_ref(),
            "G17P async-timeout B2 bootstrap-control(actual,expected): ({:x?},{:x?}) QoS rows(qid,result)={:x?}\n",
            b2_channel_control, expected_channel_control, qos_rows);
        dev_err!(self.resources.dev().as_ref(),
            "G17P async-timeout last-host-entry(offset,words): {:x?}\n", entry);
        let mut incomplete_render_parents = 0u128;
        if let (Ok(valid), Ok(completed)) = dependency {
            if valid != 0 && completed != queue.producer.previous_timestamp {
                let next = (|| -> Result<_> {
                    let geometry = queue.producer.geometry;
                    let prior = g17_submission::prepare_kick_submission(
                        completed, queue.queue_id, geometry.stamp_count(), geometry.entry_stride(),
                    ).map_err(|_| EINVAL)?;
                    let first = g17_submission::prepare_kick_submission(
                        prior.next_timestamp, queue.queue_id, geometry.stamp_count(), geometry.entry_stride(),
                    ).map_err(|_| EINVAL)?;
                    Ok((first.timestamp, first.entry_offset,
                        queue.storage.snapshot_compute_entry(first.entry_offset as usize)?))
                })();
                // Read only parents actually enabled in this exact pending
                // entry. These fixed-size firmware records distinguish an
                // unsatisfied hardware parent from a final TSQ stamp delay.
                if let Ok((_, _, words)) = &next {
                    let mask = words[4] as u32;
                    for index in 0..32usize {
                        if mask & (1u32 << index) == 0 { continue; }
                        let raw = words[5 + index];
                        let Ok(parent) = g17_submission::decode_kick_word(raw) else { continue; };
                        observed_qids |= 1u128 << parent.queue_id;
                        let phys = G17P_GFX_PHYSICAL_DATA_BASE
                            + (G17P_DEPENDENCY_RECORD_LINK_BASE - G17P_GFX_LINK_DATA_BASE)
                            + u64::from(parent.queue_id) * G17P_DEPENDENCY_RECORD_STRIDE;
                        let record: [Result<u64>; 3] = core::array::from_fn(|field| {
                            pgtable::LiveFirmwareU64Probe::new(phys + field as u64 * 8)
                                .and_then(|probe| probe.observe_mask(u64::MAX, 1))
                                .map(|sample| sample.first)
                        });
                        // Firmware compares required-completed modulo 40 bits;
                        // record[0]'s low byte is validity. Keep raw fields so
                        // a changing snapshot cannot masquerade as a proof.
                        let delta = record[1].map(|done| {
                            let difference = parent.timestamp.wrapping_sub(done) & ((1u64 << 40) - 1);
                            ((difference << 24) as i64) >> 24
                        });
                        if parent.queue_id < 128
                            && (record[0].is_ok_and(|valid| valid & 0xff == 0)
                                || delta.is_ok_and(|difference| difference > 0))
                        {
                            incomplete_render_parents |= 1u128 << parent.queue_id;
                        }
                        dev_err!(self.resources.dev().as_ref(),
                            "G17P async-timeout parent: child-qid={} index={} raw={:#x} parent-qid={} required={:#x} record={:x?} required-minus-completed-s40={:?}\n",
                            queue.queue_id, index, raw, parent.queue_id,
                            parent.timestamp, record, delta);
                    }
                }
                dev_err!(self.resources.dev().as_ref(),
                    "G17P async-timeout after-completed: qid={} completed={:#x} next(timestamp,offset,words)={:x?}\n",
                    queue.queue_id, completed, next);
            }
        }
        (observed_qids, incomplete_render_parents)
    }

    pub(crate) fn log_async_compute_timeout(&mut self, queue: &mut G17PSksmQueue) -> u128 {
        // The snapshot/report half runs in its own cold frame (several KiB
        // of diagnostic locals and format arguments), released before the
        // render-parent reports below, which nest further logging.
        let (mut observed_qids, incomplete_render_parents) =
            self.log_async_compute_timeout_report(queue);
        // Observe only the retained render pairs named by unsatisfied
        // hardware parents. This is fault-only, before VM poison propagates;
        // do not select the unrelated most-recent publisher's graph.
        let mut visited = 0u64;
        for qid in 0..128u8 {
            if incomplete_render_parents & (1u128 << qid) == 0 { continue; }
            let Some(slot) = self.render_slot_for_qid(qid) else { continue; };
            if slot >= 64 || visited & (1u64 << slot) != 0 { continue; }
            visited |= 1u64 << slot;
            if self.render_slots.allocated(slot).is_err() { continue; }
            let pair_slot = slot;
            dev_err!(self.resources.dev().as_ref(),
                "G17P async-timeout render-parent selection: child-qid={} parent-qid={} slot={}\n",
                queue.queue_id, qid, slot);
            if let Some(pair) = self.render_slots.get(pair_slot).sksm.as_ref() {
                observed_qids |= (1u128 << pair.tiling.queue_id)
                    | (1u128 << pair.fragment.queue_id);
            }
            self.report_render_wait_timeout(pair_slot, "blocked-compute-parent");
            self.log_selected_render_recovery_pre_ack(pair_slot, "blocked-compute-parent", Some(qid));
        }
        observed_qids
    }

    /// Whether the noisy per-submission traces should be emitted.
    ///
    /// A 400-deep run emits thousands of lines and overflows the kernel ring
    /// buffer, which destroys exactly the evidence at the end of the run that
    /// matters -- and makes any count obtained by grepping dmesg a measure of
    /// how much survived rather than of what happened. Trace the opening
    /// submissions, then go quiet; errors and end-of-run summaries are never
    /// gated.
    fn compute_trace_enabled(ordinal: u32) -> bool {
        let depth = module_parameters::g17p_trace_depth.value();
        depth != 0 && ordinal <= depth
    }

    /// Log the OUTER CL_2 work-channel cursors and its most recent slot.
    ///
    /// The inner item ring and the outer channel are two different rings and
    /// either can stall independently: the item ring says whether the firmware
    /// walked the records we appended, this says whether it ever looked at the
    /// slot that points at them. Also DRAM only -- `primary_state` and
    /// `shared_cluster` are driver-allocated objects, not MMIO.
    pub(crate) fn log_compute_outer_channel(&mut self, label: &'static str) {
        let snapshot = match self.resources.snapshot_compute_channel() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                dev_err!(
                    self.resources.dev().as_ref(),
                    "G17P outer[{}]: unreadable ({:?})\n",
                    label,
                    error
                );
                return;
            }
        };
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P outer[{}]: table {} ring {:#x} cursors(actual,mirror,producer)={:?} slot {} queue {:#x} kind {} flags {:#x}\n",
            label,
            snapshot.table_index,
            snapshot.ring,
            snapshot.cursors,
            snapshot.slot_index,
            snapshot.slot_queue,
            snapshot.slot_kind,
            snapshot.slot_flags
        );
    }

    /// Zero the KSM completion record and remember which descriptor the
    /// submission about to be published owns.
    ///
    /// Must run after the descriptor is encoded and before ANY firmware-
    /// visible publication, so that from the firmware's first opportunity to
    /// write the record onwards, "populated" means "written for this
    /// submission". Without it the record left by a previous submission
    /// satisfies the next wait immediately -- which is how a run whose GPU
    /// cores never powered (`gpc-state = 0x0`) still reported a woken doorbell
    /// and retired.
    fn arm_compute_completion(
        &mut self,
        queue: &mut G17PSksmQueue,
        descriptor: u64,
    ) -> Result {
        // Independent user commands now count kicks on their submit-owned TSQ
        // node before publication. That count can include other engines and is
        // deliberately independent of this physical queue's ordinal. Never
        // rewrite or compare the obsolete per-queue node here.
        let previous = if queue.independent_context { [0, 0] }
            else { self.resources.clear_compute_completion()? };
        queue.completion_descriptor = descriptor;
        self.last_published_descriptor = descriptor;
        queue.publication_resource_snapshot =
            if crate::g17_config::COMPUTE_RESOURCE_SNAPSHOT != 0 {
                Some((
                    queue.compute.snapshot_compute_submission(descriptor)?,
                    queue.storage.snapshot_compute_flist()?,
                ))
            } else {
                None
            };
        // Arm the completion stamp as well. The KSM completion record is
        // produced by the KSM hardware block and consumed by the firmware at
        // the TOP of the completion interrupt; the firmware's GPU
        // cache-maintenance command and the stamp store both happen after
        // that. So the record proves scheduling, and only the stamp proves
        // visibility -- see `await_compute_stamp`.
        //
        // Independent queues retain one stable stamp pair and advance values.
        // B1 may coalesce multiple completed kicks into one post-flush store;
        // clearing the live witness here both loses an older completion and
        // lets its later store masquerade as a newer kick's visibility proof.
        // The legacy synchronous path still arms its single outstanding kick.
        if crate::g17_config::COMPLETION_STAMP != 0 {
            let discarded = if queue.independent_context { 0 }
                else { queue.compute.arm_compute_stamp()? };
            self.compute_stamp_armed = true;
            queue.completion_stamp_armed = true;
            if discarded != 0 && Self::compute_trace_enabled(queue.submission_ordinal) {
                dev_info!(
                    self.resources.dev().as_ref(),
                    "G17P completion stamp: discarded {:#x} before publishing {:#x}\n",
                    discarded,
                    descriptor
                );
            }
        }
        // Do NOT reset `last_completion_end` here. It is the floor that makes
        // "strictly newer than the completion already consumed" mean anything,
        // and zeroing it once per submission made that test vacuous -- which
        // is why tightening the scan from `end == last_consumed_end` to
        // `end <= last_consumed_end` measured as noise: with the floor at 0 the
        // two predicates are identical. GPU timestamps are monotonic across a
        // manager's lifetime, so the previous submission's end is a valid
        // floor for this one; the field is initialised to 0 when the manager is
        // constructed, and a recovery rebuilds the manager, so a teardown
        // resets it without any per-submission help.
        if previous[0] != 0 {
            dev_info!(
                self.resources.dev().as_ref(),
                "G17P completion: cleared stale record descriptor {:#x} end {:#x} before publishing {:#x}\n",
                previous[0],
                previous[1],
                descriptor
            );
        }
        Ok(())
    }

    /// Completion ingress for the descriptor-provided completed-kick count.
    /// Sample everything host-visible that describes whether the retained
    /// compute queue and the firmware still agree about QID 4.
    ///
    /// Taken either side of a serviced firmware recovery, so the divergence a
    /// fault introduces can be SEEN rather than inferred. A boot with no fault
    /// hands the graph to any number of sequential clients; one GMMU fault in
    /// between and every later kick is accepted, retired and never executed --
    /// so whatever moves across this boundary is the mechanism.
    ///
    /// HOST DRAM ONLY, deliberately. Every field is a read of an object this
    /// driver allocated or of the firmware handoff region. Nothing here touches
    /// the sgx register file: a recovery is serviced with the GPU cores gated,
    /// and an sgx read in that state takes an asynchronous SError and panics
    /// the machine. That is also why the firmware's KSM queue slot at
    /// `0x127628 + qid*0x28` is NOT sampled here -- it lives in the ASC's own
    /// address space, reachable only through sgx MMIO or the firmware's own
    /// mapping, so there is no safe host read of it on this path.
    pub(crate) fn log_compute_recovery_alignment(
        &mut self,
        queue: &mut G17PSksmQueue,
        label: &'static str,
    ) {
        let fw_stamp = self.g17p_last_submitted_hw_timestamp(queue.queue_id as usize);
        let ring = queue.compute.read_compute_ring_state().ok();
        let free = queue.compute.compute_ring_free_slots().ok();
        let producer = queue.producer;
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P recovery-align[{}]: qid={} fw-last-submitted={:?} host(ordinal={} stamp={:#x} prev-valid={} prev-stamp={:#x} base={:#x} submitted={} addkicks={:#x}) ring(consumer={:?} producer={:?} free={:?})\n",
            label,
            queue.queue_id,
            fw_stamp,
            queue.submission_ordinal,
            producer.current_timestamp,
            producer.previous_valid,
            producer.previous_timestamp,
            producer.base_timestamp,
            producer.submitted,
            producer.last_add_kicks_timestamp,
            ring.as_ref().map(|state| state.consumer),
            ring.as_ref().map(|state| state.producer),
            free,
        );
        // The KSM pause-reason mask, read HOST-SAFELY.
        //
        // The mask itself lives at `sgx+0x2_11c8` and its refcount at the
        // firmware's `0x128a68`; both are inside the sgx 0x0..0x2ffff window,
        // which is unreachable from the AP and takes an asynchronous SError.
        // With the cores demonstrably gated after a fault-blamed recovery that
        // hazard is the NORMAL state on this path, so neither is read here.
        // The firmware publishes the same value itself: channel-0 KTrace code
        // 0x46 carries the pause reason, and the KTrace ring is host DRAM.
        // Needs `asahi.g17p_fw_trace=1` -- with tracing off the ring is empty
        // and `consumed` reads 0, which is why that is reported too.
        let ktrace = self.resources.drain_ktrace_ring(
            InstanceRole::Primary,
            module_parameters::g17p_fw_ktrace_verbose.value() != 0,
            module_parameters::g17p_fw_ktrace_budget.value(),
        );
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P recovery-align[{}]: ktrace(consumed={:?} pause-mask={:?} saw-addkicks={:?} lost={:?}) -- pause-mask needs g17p_fw_trace=1\n",
            label,
            ktrace.as_ref().map(|stats| stats.consumed),
            ktrace.as_ref().map(|stats| stats.last_pause_mask),
            ktrace.as_ref().map(|stats| stats.saw_add_kicks),
            ktrace.as_ref().map(|stats| stats.lost_records),
        );
        let status_a = self.resources.status_a_snapshot();
        let channel = self.resources.snapshot_compute_channel();
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P recovery-align[{}]: statusA(scan-active={:?} active={:?} scheduler-constructed={:?}) outer(cursors={:?} slot-queue={:?})\n",
            label,
            status_a.as_ref().map(|state| state.scan_active),
            status_a.as_ref().map(|state| state.active),
            status_a.as_ref().map(|state| state.scheduler_constructed),
            channel.as_ref().map(|state| state.cursors),
            channel.as_ref().map(|state| state.slot_queue),
        );
    }

    /// Realign the retained compute queue with the firmware after a recovery.
    ///
    /// A recovery is the one event that can move the firmware's idea of this
    /// QID without the host publishing anything, so continuity is exactly the
    /// assumption that must not be made across it. See `g17p_recovery_resync`
    /// for the individual arms; returns the mask actually applied.
    pub(crate) fn resynchronise_compute_queue_after_recovery(
        &mut self,
        queue: &mut G17PSksmQueue,
    ) -> Result<u32> {
        let mode = crate::g17_config::RECOVERY_RESYNC;
        if mode == 0 {
            return Ok(0);
        }
        let mut applied = 0u32;
        if mode & 1 != 0 && queue.producer.previous_valid {
            // The next kick now encodes the no-predecessor implicit dependency
            // -- dependency on (qid, stamp 0) -- which is byte-for-byte what a
            // proven first bind sends.
            queue.producer.previous_valid = false;
            queue.producer.previous_timestamp = 0;
            applied |= 1;
        }
        if mode & 2 != 0 {
            if let Ok((valid, stamp)) =
                self.g17p_last_submitted_hw_timestamp(queue.queue_id as usize)
            {
                // Forward only. Rewinding the stamp is what makes a kick look
                // already retired, which is the failure this whole line of work
                // started from.
                if valid != 0 && stamp >= queue.producer.current_timestamp {
                    let aligned = stamp.wrapping_add(1);
                    if g17_submission::decode_kick_timestamp(aligned).is_ok() {
                        queue.producer.current_timestamp = aligned;
                        queue.producer.base_timestamp = aligned;
                        applied |= 2;
                    }
                }
            }
        }
        if mode & 4 != 0 && queue.direct_lifecycle.cached_channel_identity != 0 {
            // Only the notification predicate: `publish_config_update` keys off
            // `queue_config_dirty`, which stays false, so this cannot re-assert
            // the tag-15 queue-install flag.
            queue.direct_lifecycle.cached_channel_identity = 0;
            applied |= 4;
        }
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P recovery-resync: qid={} mode={:#x} applied={:#x} -> stamp={:#x} prev-valid={} prev-stamp={:#x} channel-identity={:#x}\n",
            queue.queue_id,
            mode,
            applied,
            queue.producer.current_timestamp,
            queue.producer.previous_valid,
            queue.producer.previous_timestamp,
            queue.direct_lifecycle.cached_channel_identity,
        );
        Ok(applied)
    }

    /// Make a retained queue submittable again after a submission was
    /// published and never seen to retire.
    ///
    /// The queue's forward-only state -- the SKSM stamp sequence, the
    /// submission ordinal and the item-ring producer -- is deliberately left
    /// exactly where the abandoned kick left it. Rewinding any of them is what
    /// makes the firmware and the host disagree, and the stamp namespace is
    /// per-QID and monotonic for the life of the hardware queue.
    ///
    /// What IS repaired is the host-side accounting the abandoned kick leaked:
    ///
    ///  * `producer.submitted` is written off. `prepare_g17p_cl_kick_entry`
    ///    refuses once it reaches `stamp_count` (0x100), so one abandoned kick
    ///    per failing client would otherwise brick the queue after 256 of them
    ///    -- and a CTS run submits failing work by design.
    ///  * The armed completion stamp is disarmed, so the next retire does not
    ///    spend `g17p_completion_stamp_us` waiting for a stamp that belongs to
    ///    a kick nobody is waiting for any more.
    ///  * The KSM completion record and its expected-descriptor filter are
    ///    cleared, so a late write for the abandoned kick cannot satisfy the
    ///    next client's wait. `last_completion_end` is deliberately NOT reset:
    ///    it is the monotonic floor that makes that guarantee work.
    ///
    /// Reports the item-ring cursors, because whether the firmware CONSUMED
    /// the abandoned records is the one thing no capture has settled: a
    /// consumer that has passed the producer means the records are retired and
    /// the ring recycles normally; a consumer stuck behind them means the ring
    /// will fill and `compute_ring_free_slots` will eventually refuse.
    /// DRAM only -- safe with the GPU cores gated, which is the state a timed
    /// out submission leaves them in.
    pub(crate) fn resynchronise_compute_queue_after_abandon(
        &mut self,
        queue: &mut G17PSksmQueue,
    ) -> Result<u32> {
        let outstanding = queue.producer.submitted;
        if outstanding != 0 {
            queue.producer =
                g17_submission::complete_g17p_cl_kicks(queue.producer, outstanding)
                    .map_err(|_| EINVAL)?;
        }
        self.compute_stamp_armed = false;
        let stale = self.resources.clear_compute_completion()?;
        self.last_published_descriptor = 0;
        let ring = queue.compute.read_compute_ring_state().ok();
        let free = queue.compute.compute_ring_free_slots().ok();
        dev_info!(
            self.resources.dev().as_ref(),
            "G17P compute: abandoned kick written off on qid {}; outstanding {} -> 0, ordinal={} stamp={:#x} ring consumer={:?} producer={:?} free={:?} stale-record desc={:#x} end={:#x}\n",
            queue.queue_id,
            outstanding,
            queue.submission_ordinal,
            queue.producer.current_timestamp,
            ring.as_ref().map(|state| state.consumer),
            ring.as_ref().map(|state| state.producer),
            free,
            stale[0],
            stale[1],
        );
        Ok(outstanding)
    }

    pub(crate) fn complete_compute_entries(
        &self,
        queue: &mut G17PSksmQueue,
        completed_count: u32,
    ) -> Result {
        if queue.hardware_state != G17PSksmQueueHardwareState::HardwareRegistered
            || queue.scratch != self.sksm_scratch()
            || queue.uat_owner != self.uat().ttb_base()
        {
            return Err(EINVAL);
        }
        self.await_compute_stamp(queue);
        queue.producer = g17_submission::complete_g17p_cl_kicks(queue.producer, completed_count)
            .map_err(|_| EINVAL)?;
        Ok(())
    }

    /// Hold a retire until the firmware has published this submission's
    /// completion stamp, so that "complete" implies "the dispatch's writes are
    /// visible".
    ///
    /// Firmware RE (`b1`) settles the ordering. The KSM completion record is
    /// written by the KSM hardware block, and the firmware's completion
    /// interrupt handler CONSUMES it first (`0xd550` drains the ring, `0xab64`
    /// decodes one record). Only afterwards, in
    /// `AGFAcceleratorProcessTimeStampQueue` (`0x168d0`), does it issue the GPU
    /// cache-maintenance command to `sgx+0x1_8030`, poll `sgx+0x1_8038` bit 1
    /// until it clears, scan the fault units, and finally store the stamp
    /// value from descriptor `+0x0F50` through the pointer at descriptor
    /// `+0x0F40`. That pointer is this queue's support-page dispatch-A word.
    ///
    /// So the record's arrival is upstream of the flush and says nothing about
    /// the dispatch's stores; the stamp store is downstream of it and says
    /// exactly that. Waiting for the record and not the stamp is why a
    /// submission retired cleanly and the output buffer then took a further
    /// 0-200us to show correct values -- and why adding console verbosity or a
    /// settle "fixed" depth: both simply spent that time somewhere else.
    ///
    /// This is a data gate, not a settle: when the stamp is already present --
    /// which it will be whenever the driver was not the first thing to look --
    /// nothing is waited for at all. If the stamp never arrives the retire
    /// proceeds anyway, so the worst case is exactly the previous behaviour.
    fn await_compute_stamp(&self, queue: &mut G17PSksmQueue) {
        const POLL_US: u32 = 5;
        if crate::g17_config::COMPLETION_STAMP == 0 || !self.compute_stamp_armed {
            return;
        }
        let budget_us = module_parameters::g17p_completion_stamp_us.value();
        let mut waited_us = 0u32;
        loop {
            match queue.compute.read_compute_stamp() {
                Ok(0) => {}
                Ok(stamp) => {
                    if waited_us != 0 {
                        // Log the first wait unconditionally -- a quiet run
                        // must still say whether this gate does any work --
                        // and later ones only while tracing is on.
                        let first = self.compute_stamp_waits.fetch_add(1, Ordering::Relaxed) == 0;
                        if first || Self::compute_trace_enabled(queue.submission_ordinal) {
                            dev_info!(
                                self.resources.dev().as_ref(),
                                "G17P completion stamp: {:#x} arrived {} us after the KSM record (waits so far {})\n",
                                stamp,
                                waited_us,
                                self.compute_stamp_waits.load(Ordering::Relaxed)
                            );
                        }
                    }
                    return;
                }
                Err(error) => {
                    dev_warn!(
                        self.resources.dev().as_ref(),
                        "G17P completion stamp unreadable ({:?}); retiring on the KSM record alone\n",
                        error
                    );
                    return;
                }
            }
            if waited_us >= budget_us {
                // Rate-limited: hardware shows a real minority of submissions
                // never publish a stamp (19 of 300), and one console write per
                // occurrence costs about a millisecond, which would distort the
                // very timing this gate measures. Report the first, then every
                // 64th, always with the running total.
                let misses = self.compute_stamp_misses.fetch_add(1, Ordering::Relaxed) + 1;
                if misses == 1 || misses % 64 == 0 {
                    dev_warn!(
                        self.resources.dev().as_ref(),
                        "G17P completion stamp never published within {} us ({} so far); retiring on the KSM record alone\n",
                        budget_us,
                        misses
                    );
                }
                return;
            }
            fsleep(Delta::from_micros(POLL_US as i64));
            waited_us = waited_us.saturating_add(POLL_US);
        }
    }

    pub(crate) fn snapshot_compute_timeout(
        &mut self,
        queue: &mut G17PSksmQueue,
    ) -> Result<G17PComputeTimeoutSnapshot> {
        Ok(G17PComputeTimeoutSnapshot {
            graph: queue.compute.snapshot_initial_channel_group()?,
            channel: self.resources.snapshot_compute_channel()?,
            channel_scan: self.resources.snapshot_all_primary_work_channels()?,
            channel_control_prefix: queue.logical_channel_control.as_deref()
                .or(self.compute_channel_control.as_ref())
                .ok_or(EINVAL)?
                .snapshot_prefix()?,
            control: self.resources.control_counters()?,
        })
    }

    /// Publish newly appended kicks for a hardware-registered queue.
    pub(crate) fn add_sksm_kicks(
        &self,
        registers: &regs::Resources,
        queue: &mut G17PSksmQueue,
        add_count: u32,
    ) -> Result {
        if queue.hardware_state != G17PSksmQueueHardwareState::HardwareRegistered
            || queue.scratch != self.sksm_scratch()
            || queue.uat_owner != self.uat().ttb_base()
        {
            return Err(EINVAL);
        }

        let pair = g17_submission::prepare_sksm_add_kicks_mmio(
            g17_submission::SksmAddKicksTarget::G17P,
            queue.queue_id,
            add_count,
            queue.scratch,
        )
        .map_err(|_| EINVAL)?
        .write_pair();
        registers.g17p_sksm_publish_write_pair(pair)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: u64 = 0xffff_fc20_0100_0000;
    const UAT: u64 = 0x9000_0000;

    #[test]
    fn ta_hardware_buffer_ids_match_native_order_exhaustion_and_reuse() {
        let mut allocator = G17PHardwareBufferIdAllocator::new();
        for expected in 0..G17P_TA_HARDWARE_BUFFER_ID_COUNT as u32 {
            assert_eq!(allocator.allocate(), Ok(expected));
        }
        assert_eq!(
            allocator.allocate(),
            Err(G17PHardwareBufferIdError::Exhausted)
        );

        allocator.release(42).unwrap();
        assert_eq!(
            allocator.release(42),
            Err(G17PHardwareBufferIdError::DoubleFree)
        );
        assert_eq!(allocator.allocate(), Ok(42));
        assert_eq!(
            allocator.release(G17P_TA_HARDWARE_BUFFER_ID_COUNT as u32),
            Err(G17PHardwareBufferIdError::InvalidId)
        );
    }


    #[test]
    fn native_render_visibility_and_ta_order_are_strict() {
        assert!(g17p_native_fragment_only_visible(
            0, 0, 4, 4, 1, 1, 0, 0
        ));
        assert!(!g17p_native_fragment_only_visible(
            0, 4, 4, 4, 1, 1, 0, 0
        ));
        assert!(!g17p_native_fragment_only_visible(
            0, 0, 4, 4, 1, 1, 1, 0
        ));
        assert!(g17p_native_tiling_fully_visible(4, 4, 1, 1));
        assert!(!g17p_native_tiling_fully_visible(2, 4, 1, 1));

        let mut order = G17PNativeRenderPublishOrder::after_fragment_doorbell();
        assert_eq!(
            order.advance(G17PNativeRenderPublishPhase::TilingSksm),
            Err(())
        );
        for phase in [
            G17PNativeRenderPublishPhase::TilingPrefix,
            G17PNativeRenderPublishPhase::TilingSksm,
            G17PNativeRenderPublishPhase::TilingKick,
            G17PNativeRenderPublishPhase::TilingOuter,
            G17PNativeRenderPublishPhase::PoolTransition,
        ] {
            assert_eq!(order.advance(phase), Ok(()));
        }
        assert_eq!(
            order.advance(G17PNativeRenderPublishPhase::PoolTransition),
            Err(())
        );
    }

    #[test]
    fn native_render_mode_three_waits_for_exact_fragment_outer_consumption() {
        assert!(!g17p_native_fragment_first_mode(0));
        assert!(!g17p_native_fragment_first_mode(1));
        assert!(g17p_native_fragment_first_mode(2));
        assert!(g17p_native_fragment_first_mode(3));
        assert!(!g17p_native_fragment_first_mode(4));

        assert!(!g17p_native_fragment_preack_mode(2));
        assert!(g17p_native_fragment_preack_mode(3));

        assert!(!g17p_native_fragment_outer_consumed([0, 0, 1], 1));
        assert!(!g17p_native_fragment_outer_consumed([0, 1, 1], 1));
        assert!(g17p_native_fragment_outer_consumed([1, 0, 1], 1));
        assert!(g17p_native_fragment_outer_consumed([1, 1, 1], 1));
        assert!(!g17p_native_fragment_outer_consumed([1, 1, 2], 1));
    }

    #[test]
    fn render_outer_deferral_mode_matrix_is_explicit() {
        use g17_resources::G17PDeferredOuterPublication::{Both, None, Tiling};

        assert_eq!(g17p_render_deferred_outer_mode(0, false), Both);
        assert_eq!(g17p_render_deferred_outer_mode(1, false), None);
        assert_eq!(g17p_render_deferred_outer_mode(2, false), Tiling);
        assert_eq!(g17p_render_deferred_outer_mode(3, false), Tiling);
        for mode in 0..=3 {
            assert_eq!(g17p_render_deferred_outer_mode(mode, true), Both);
        }
    }

    #[test]
    fn bootstrap_async_flist_releases_only_after_type13_and_paired_render() {
        let mut lifecycle = G17PBootstrapUmaLifecycle::new();
        assert_eq!(
            lifecycle.match_completion(lifecycle.request.unwrap()),
            Err(G17PBootstrapUmaLifecycleError::WrongReferenceCount)
        );
        lifecycle.retire_prepare_reference().unwrap();
        lifecycle.match_completion(lifecycle.request.unwrap()).unwrap();
        assert_eq!(lifecycle.hardware_buffer_references, 2);
        assert_eq!(lifecycle.pending_release_count, 2);
        assert_eq!(lifecycle.pending_release(), None);
        let release = lifecycle.retire_render_references().unwrap().unwrap();
        assert_eq!(
            release,
            G17PBootstrapUmaRelease {
                sequence: 0,
                hardware_buffer_id: 0,
            }
        );
        assert_eq!(lifecycle.hardware_buffer_references, 0);
        assert_eq!(lifecycle.pending_release_count, 0);
        assert_eq!(lifecycle.pending_release(), Some(release));
        lifecycle.mark_published(release).unwrap();
        lifecycle.mark_sent(29).unwrap();
        assert_eq!(lifecycle.pending_release(), None);
        assert!(lifecycle.release_sent());
        assert_eq!(lifecycle.release_producer_after, Some(29));
        assert!(lifecycle.mark_completed(28).is_err());
        lifecycle.mark_completed(29).unwrap();
        assert!(!lifecycle.release_sent());
        assert_eq!(lifecycle.release_producer_after, None);
    }

    #[test]
    fn bootstrap_async_flist_release_backpressure_tracks_exact_producer() {
        fn sent_lifecycle(target: u32) -> G17PBootstrapUmaLifecycle {
            let mut lifecycle = G17PBootstrapUmaLifecycle::new();
            lifecycle.retire_prepare_reference().unwrap();
            lifecycle
                .match_completion(lifecycle.request.unwrap())
                .unwrap();
            let release = lifecycle.retire_render_references().unwrap().unwrap();
            lifecycle.mark_published(release).unwrap();
            lifecycle.mark_sent(target).unwrap();
            lifecycle
        }

        let mut delayed = sent_lifecycle(29);
        assert_eq!(delayed.observe_release_consumer(28, 29), Ok(false));
        assert!(delayed.release_sent());
        assert_eq!(delayed.observe_release_consumer(29, 29), Ok(true));
        assert_eq!(delayed.state, G17PBootstrapUmaRequestState::Completed);

        let mut timed_out = sent_lifecycle(41);
        for _ in 0..20 {
            assert_eq!(timed_out.observe_release_consumer(40, 41), Ok(false));
        }
        assert!(timed_out.release_sent());
        assert_eq!(timed_out.release_producer_after, Some(41));
        assert_eq!(timed_out.observe_release_consumer(41, 41), Ok(true));

        // Consumer may pass an earlier release when later owners or TVB
        // replies publish before the host observes the shared ring again.
        let mut passed = sent_lifecycle(41);
        assert_eq!(passed.observe_release_consumer(43, 45), Ok(true));

        // The same pending-interval test is exact across 255 -> 0 wrap.
        let mut wrapped = sent_lifecycle(0);
        assert_eq!(wrapped.observe_release_consumer(255, 1), Ok(false));
        assert_eq!(wrapped.observe_release_consumer(1, 2), Ok(true));
        assert_eq!(g17p_control_ring_target_consumed(255, 1, 1), Ok(false));
        assert_eq!(g17p_control_ring_target_consumed(0, 1, 1), Ok(false));
        assert_eq!(g17p_control_ring_target_consumed(1, 1, 1), Ok(true));
    }

    #[test]
    fn bootstrap_release_stays_ready_until_backpressure_clears() {
        let mut lifecycle = G17PBootstrapUmaLifecycle::new();
        lifecycle.retire_prepare_reference().unwrap();
        lifecycle
            .match_completion(lifecycle.request.unwrap())
            .unwrap();
        let release = lifecycle.retire_render_references().unwrap().unwrap();
        for _ in 0..1000 {
            assert_eq!(lifecycle.pending_release(), Some(release));
            assert_eq!(lifecycle.state, G17PBootstrapUmaRequestState::Ready);
        }
        lifecycle.mark_published(release).unwrap();
        assert_eq!(lifecycle.pending_release(), None);
        assert_eq!(
            lifecycle.mark_published(release),
            Err(G17PBootstrapUmaLifecycleError::WrongPhase)
        );
    }

    #[test]
    fn bootstrap_async_flist_rejects_raw_or_duplicate_type13_records() {
        let mut lifecycle = G17PBootstrapUmaLifecycle::new();
        lifecycle.retire_prepare_reference().unwrap();
        let exact = lifecycle.request.unwrap();
        for wrong in [
            G17PBootstrapUmaCompletion {
                cookie: exact.cookie + 0x40,
                ..exact
            },
            G17PBootstrapUmaCompletion {
                count: exact.count - 1,
                ..exact
            },
            G17PBootstrapUmaCompletion {
                hardware_buffer_id: 1,
                ..exact
            },
        ] {
            assert_eq!(
                lifecycle.match_completion(wrong),
                Err(G17PBootstrapUmaLifecycleError::WrongCompletion)
            );
        }
        lifecycle.match_completion(exact).unwrap();
        assert_eq!(
            lifecycle.match_completion(exact),
            Err(G17PBootstrapUmaLifecycleError::WrongPhase)
        );
    }

    #[test]
    fn shared_flist_holds_both_render_pairs_until_the_last_retirement() {
        let mut first = G17PBootstrapUmaLifecycle::new();
        first.retire_prepare_reference().unwrap();
        first.match_completion(first.request.unwrap()).unwrap();
        assert_eq!((first.hardware_buffer_references, first.pending_release_count), (2, 2));

        let mut both = G17PBootstrapUmaLifecycle::acquire(Some(&first), 1).unwrap();
        // The second command pair is another reference to the same native
        // hardware-buffer owner, not a new registration incarnation.
        assert_eq!(both.sequence, first.sequence);
        assert_eq!((both.hardware_buffer_references, both.pending_release_count), (5, 4));
        both.retire_prepare_reference().unwrap();
        assert_eq!((both.hardware_buffer_references, both.pending_release_count), (4, 4));
        assert_eq!(both.retire_render_references().unwrap(), None);
        assert_eq!((both.hardware_buffer_references, both.pending_release_count), (2, 2));
        let release = both.retire_render_references().unwrap().unwrap();
        assert_eq!(release.sequence, 0);
        assert_eq!((both.hardware_buffer_references, both.pending_release_count), (0, 0));
    }

    #[test]
    fn retained_flist_leases_grow_once_and_release_every_captured_generation() {
        let mut previous = None;
        let mut grows = 0;
        let mut releases = 0;
        for ordinal in 0..3 {
            let sequence = u64::from(g17_submission::g17p_render_flist_generation(ordinal));
            let mut lease = G17PBootstrapUmaLifecycle::acquire(previous.as_ref(), sequence).unwrap();
            assert_eq!(lease.sequence, sequence);
            assert_eq!(lease.pending_release_count, 2);
            assert_eq!(lease.hardware_buffer_references, if ordinal == 0 { 4 } else { 3 });
            assert_eq!(lease.retire_render_references(), Err(G17PBootstrapUmaLifecycleError::WrongPhase));
            lease.retire_prepare_reference().unwrap();
            if let Some(request) = lease.request {
                grows += 1;
                assert_eq!(lease.retire_render_references(), Err(G17PBootstrapUmaLifecycleError::WrongPhase));
                lease.match_completion(request).unwrap();
            }
            assert_eq!(lease.state, G17PBootstrapUmaRequestState::Ready);
            assert!(lease.retire_prepare_reference().is_err());
            let release = lease.retire_render_references().unwrap().unwrap();
            releases += 1;
            assert_eq!(release.sequence, sequence);
            assert_eq!(release.hardware_buffer_id, G17P_BOOTSTRAP_UMA_HARDWARE_BUFFER_ID);
            assert!(lease.retire_render_references().is_err());
            assert!(G17PBootstrapUmaLifecycle::acquire(Some(&lease), sequence + 1).is_err());
            // Submission ordinals are wide; channel ring producers remain u32.
            let producer_after = u32::try_from(ordinal + 20).unwrap();
            assert!(lease.mark_completed(producer_after).is_err());
            assert!(lease.mark_published(G17PBootstrapUmaRelease { sequence: sequence + 1, ..release }).is_err());
            lease.mark_published(release).unwrap();
            assert!(G17PBootstrapUmaLifecycle::acquire(Some(&lease), sequence + 1).is_err());
            lease.mark_sent(producer_after).unwrap();
            // Native does not wait for the release to be consumed: the pair
            // is acquirable now and the successor carries the cursor.
            let early = G17PBootstrapUmaLifecycle::acquire(Some(&lease), sequence + 1).unwrap();
            assert_eq!(early.carried_release_producer_after, Some(producer_after));
            assert!(early.has_release_witness());
            assert!(lease.mark_completed(producer_after + 1).is_err());
            lease.mark_completed(producer_after).unwrap();
            assert!(lease.mark_completed(producer_after).is_err());
            assert!(lease.mark_published(release).is_err());
            assert!(G17PBootstrapUmaLifecycle::acquire(Some(&lease), sequence).is_err());
            assert!(G17PBootstrapUmaLifecycle::acquire(Some(&lease), sequence + 2).is_err());
            previous = Some(lease);
        }
        assert_eq!((grows, releases), (1, 3));
        assert!(G17PBootstrapUmaLifecycle::acquire(None, 1).is_err());
        let mut wrong = previous.unwrap();
        wrong.hardware_buffer_id += 1;
        assert!(G17PBootstrapUmaLifecycle::acquire(Some(&wrong), 3).is_err());
        let mut warm = G17PBootstrapUmaLifecycle::acquire(previous.as_ref(), 3).unwrap();
        let stale = G17PBootstrapUmaLifecycle::new().request.unwrap();
        assert_eq!(warm.match_completion(stale), Err(G17PBootstrapUmaLifecycleError::WrongCompletion));
        warm.retire_prepare_reference().unwrap();
        assert_eq!(warm.match_completion(stale), Err(G17PBootstrapUmaLifecycleError::WrongPhase));
    }

    #[test]
    fn private_render_flist_owner_grows_once_then_reacquires_warm() {
        let owner = (129, 0xffff_fc20_1100_0000, 0x70_0410_0000, 0x70_0410_0440);
        let mut cold = G17PBootstrapUmaLifecycle::acquire_owner(
            None, 0, owner.0, owner.1, owner.2, owner.3, true,
        ).unwrap();
        let request = cold.request.expect("cold owner must grow");
        assert_eq!((cold.hardware_buffer_references, cold.pending_release_count), (4, 2));
        cold.retire_prepare_reference().unwrap();
        cold.match_completion(request).unwrap();
        let release = cold.retire_render_references().unwrap().unwrap();
        cold.mark_published(release).unwrap();
        cold.mark_sent(17).unwrap();
        cold.mark_completed(17).unwrap();

        let mut warm = G17PBootstrapUmaLifecycle::acquire_owner(
            Some(&cold), 1, owner.0, owner.1, owner.2, owner.3, false,
        ).unwrap();
        assert_eq!(warm.sequence, 1);
        assert_eq!(warm.request, None);
        assert_eq!((warm.hardware_buffer_references, warm.pending_release_count), (3, 2));
        warm.retire_prepare_reference().unwrap();
        assert_eq!(warm.state, G17PBootstrapUmaRequestState::Ready);
        assert!(warm.retire_render_references().unwrap().is_some());
    }

    #[test]
    fn reacquire_before_release_consumption_carries_the_witness() {
        let owner = (129, 0xffff_fc20_1100_0000, 0x70_0410_0000, 0x70_0410_0580);
        let mut first = G17PBootstrapUmaLifecycle::acquire_owner(
            None, 0, owner.0, owner.1, owner.2, owner.3, false,
        ).unwrap();
        first.retire_prepare_reference().unwrap();
        let release = first.retire_render_references().unwrap().unwrap();
        // Ready with zero references: the release is not yet in the ring, so
        // the pair is not acquirable (the event worker publishes it first).
        assert!(G17PBootstrapUmaLifecycle::acquire_owner(
            Some(&first), 1, owner.0, owner.1, owner.2, owner.3, false,
        ).is_err());
        first.mark_published(release).unwrap();
        first.mark_sent(7).unwrap();
        // Sent: acquirable immediately, cursor 7 carried.
        let mut second = G17PBootstrapUmaLifecycle::acquire_owner(
            Some(&first), 1, owner.0, owner.1, owner.2, owner.3, false,
        ).unwrap();
        assert_eq!(second.sequence, 1);
        assert_eq!(second.request, None);
        assert_eq!(second.carried_release_producer_after, Some(7));
        assert_eq!((second.hardware_buffer_references, second.pending_release_count), (3, 2));
        // Unconsumed: the witness stays; consumed: it clears, own state intact.
        assert_eq!(second.settle_release_witnesses(6, 7), Ok(0));
        assert!(second.has_release_witness());
        assert_eq!(second.settle_release_witnesses(7, 9), Ok(1));
        assert!(!second.has_release_witness());
        assert_eq!(second.state, G17PBootstrapUmaRequestState::Submitted);
        second.retire_prepare_reference().unwrap();
        assert_eq!(second.state, G17PBootstrapUmaRequestState::Ready);
        // The second lease's own release, sent before a third acquisition:
        // the third carries the LATEST cursor, which covers the earlier one.
        let release = second.retire_render_references().unwrap().unwrap();
        second.mark_published(release).unwrap();
        second.mark_sent(11).unwrap();
        let third = G17PBootstrapUmaLifecycle::acquire_owner(
            Some(&second), 2, owner.0, owner.1, owner.2, owner.3, false,
        ).unwrap();
        assert_eq!(third.carried_release_producer_after, Some(11));
        // A different owner taking over the pair after a sent release is a
        // cold owner: it requests a grow when its pool needs one and still
        // carries the witness.
        let other = (129, 0xffff_fc20_1200_0000, 0x70_0420_0000, 0x70_0420_0580);
        let cold = G17PBootstrapUmaLifecycle::acquire_owner(
            Some(&second), 2, other.0, other.1, other.2, other.3, true,
        ).unwrap();
        assert!(cold.request.is_some());
        assert_eq!(cold.carried_release_producer_after, Some(11));
    }

    #[test]
    fn presized_render_flist_first_lease_is_warm() {
        let owner = (129, 0xffff_fc20_1100_0000, 0x70_0410_0000, 0x70_0410_0580);
        let mut first = G17PBootstrapUmaLifecycle::acquire_owner(
            None, 0, owner.0, owner.1, owner.2, owner.3, false,
        ).unwrap();
        assert_eq!(first.request, None);
        assert_eq!((first.hardware_buffer_references, first.pending_release_count), (3, 2));
        first.retire_prepare_reference().unwrap();
        assert_eq!(first.state, G17PBootstrapUmaRequestState::Ready);
        // A second kick overlaps the warm first lease without a new sequence.
        let mut both = G17PBootstrapUmaLifecycle::acquire_owner(
            Some(&first), 1, owner.0, owner.1, owner.2, owner.3, false,
        ).unwrap();
        assert_eq!(both.sequence, 0);
        assert_eq!((both.hardware_buffer_references, both.pending_release_count), (5, 4));
        both.retire_prepare_reference().unwrap();
        assert!(both.retire_render_references().unwrap().is_none());
        assert!(both.retire_render_references().unwrap().is_some());
    }

    #[test]
    fn private_render_flist_replacement_waits_for_exact_old_owner_retirement() {
        let mut old = G17PBootstrapUmaLifecycle::acquire_owner(
            None, 0, 130, 0xffff_fc20_1200_0000, 0x70_0420_0000,
            0x70_0420_0440, true,
        ).unwrap();
        old.retire_prepare_reference().unwrap();
        old.match_completion(old.request.unwrap()).unwrap();
        assert_eq!(
            G17PBootstrapUmaLifecycle::acquire_owner(
                Some(&old), 1, 130, 0xffff_fc20_1300_0000,
                0x70_0430_0000, 0x70_0430_0440, true,
            ),
            Err(G17PBootstrapUmaLifecycleError::WrongPhase),
        );
        let release = old.retire_render_references().unwrap().unwrap();
        old.mark_published(release).unwrap();
        old.mark_sent(31).unwrap();
        old.mark_completed(31).unwrap();

        let replacement = G17PBootstrapUmaLifecycle::acquire_owner(
            Some(&old), 1, 130, 0xffff_fc20_1300_0000,
            0x70_0430_0000, 0x70_0430_0440, true,
        ).unwrap();
        assert_eq!(replacement.sequence, 1);
        assert_eq!(replacement.hardware_buffer_references, 4);
        assert_eq!(replacement.request.unwrap().shared_control, 0xffff_fc20_1300_0000);
    }

    #[test]
    fn private_render_flist_same_owner_overlap_keeps_identity_until_last_pair() {
        let owner = (131, 0xffff_fc20_1400_0000, 0x70_0440_0000, 0x70_0440_0440);
        let mut first = G17PBootstrapUmaLifecycle::acquire_owner(
            None, 0, owner.0, owner.1, owner.2, owner.3, true,
        ).unwrap();
        first.retire_prepare_reference().unwrap();
        first.match_completion(first.request.unwrap()).unwrap();
        let mut both = G17PBootstrapUmaLifecycle::acquire_owner(
            Some(&first), 1, owner.0, owner.1, owner.2, owner.3, false,
        ).unwrap();
        assert_eq!(both.sequence, first.sequence);
        assert_eq!(both.request, None);
        assert_eq!((both.hardware_buffer_references, both.pending_release_count), (5, 4));
        both.retire_prepare_reference().unwrap();
        assert_eq!(both.retire_render_references().unwrap(), None);
        let release = both.retire_render_references().unwrap().unwrap();
        assert_eq!((release.sequence, release.hardware_buffer_id), (0, owner.0));
    }

    fn decode_hex<const N: usize>(source: &[u8]) -> [u8; N] {
        let source = source.strip_suffix(b"\n").unwrap_or(source);
        assert_eq!(source.len(), N * 2);
        let nibble = |value: u8| match value {
            b'0'..=b'9' => value - b'0',
            b'a'..=b'f' => value - b'a' + 10,
            _ => panic!("invalid hex fixture"),
        };
        let mut out = [0u8; N];
        let mut index = 0;
        while index < N {
            out[index] = nibble(source[index * 2]) << 4 | nibble(source[index * 2 + 1]);
            index += 1;
        }
        out
    }

    fn instruction(bytes: &[u8], index: usize) -> u32 {
        u32::from_le_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap())
    }

    fn sign_extend(value: u64, bits: u32) -> i64 {
        ((value << (64 - bits)) as i64) >> (64 - bits)
    }

    fn adrp_target(pc: u64, value: u32) -> u64 {
        assert_eq!(value & 0x9f00_0000, 0x9000_0000);
        let immediate = (((value as u64 >> 5) & 0x7ffff) << 2)
            | ((value as u64 >> 29) & 0x3);
        (pc & !0xfff).wrapping_add((sign_extend(immediate, 21) << 12) as u64)
    }

    fn branch_target(pc: u64, value: u32) -> u64 {
        assert_eq!(value & 0x7c00_0000, 0x1400_0000);
        pc.wrapping_add((sign_extend((value & 0x03ff_ffff) as u64, 26) << 2) as u64)
    }

    fn conditional_branch_target(pc: u64, value: u32) -> u64 {
        assert_eq!(value & 0xff00_0010, 0x5400_0000);
        pc.wrapping_add((sign_extend(((value >> 5) & 0x7ffff) as u64, 19) << 2) as u64)
    }

    fn load_store_unsigned_offset(value: u32) -> usize {
        let scale = value >> 30;
        (((value >> 10) & 0xfff) as usize) << scale
    }

    fn add_immediate(value: u32) -> usize {
        assert_eq!(value & 0x7f00_0000, 0x1100_0000);
        (((value >> 10) & 0xfff) as usize) << (((value >> 22) & 1) * 12)
    }

    fn wide_immediate(value: u32) -> u64 {
        let shift = ((value >> 21) & 0x3) * 16;
        ((value as u64 >> 5) & 0xffff) << shift
    }

    fn target() -> TargetObservation {
        TargetObservation {
            chip_id: 0x8140,
            declared_gen: GpuGen::G17,
            declared_variant: GpuVariant::P,
            hw_family: 0x0a,
            hw_variant: 0,
            num_dies: 1,
            uat_ias: 42,
            uat_oas: 42,
        }
    }

    fn resources() -> ResourceObservation {
        ResourceObservation {
            uat_ttb_base: UAT,
            primary_uat_owner: UAT,
            secondary_uat_owner: UAT,
            primary_role: InstanceRole::Primary,
            secondary_role: InstanceRole::Secondary,
            primary_root: ROOT,
            secondary_root: ROOT + g17_initdata::SECONDARY_ROOT_DELTA,
            shared_cluster_size: g17_initdata::NATIVE_SHARED_CLUSTER_SIZE,
        }
    }

    #[test]
    fn manager_config_is_exact_t8140_geometry() {
        assert_eq!(T8140_MANAGER_CONFIG.chip_id, 0x8140);
        assert_eq!(T8140_MANAGER_CONFIG.base_clock_hz, 24_000_000);
        assert_eq!(T8140_MANAGER_CONFIG.uat_ias, 42);
        assert_eq!(T8140_MANAGER_CONFIG.uat_oas, 42);
        assert_eq!(T8140_MANAGER_CONFIG.uat_page_size, 0x4000);
        assert_eq!(T8140_MANAGER_CONFIG.uat_page_bits, 14);
        assert_eq!(
            T8140_MANAGER_CONFIG.uat_levels,
            [(36, 64), (25, 2048), (14, 2048)]
        );
        assert_eq!(
            T8140_MANAGER_CONFIG.initdata_version,
            [0x04c0, 0x0396, 0xa322, 0x0c8a]
        );
        assert_eq!(T8140_MANAGER_CONFIG.root_sizes, [0xb8, 0xc8]);
        assert_eq!(T8140_MANAGER_CONFIG.secondary_root_delta, 0x8000);
        assert_eq!(T8140_MANAGER_CONFIG.secondary_shared_region_delta, 0x40000);
        assert_eq!(T8140_MANAGER_CONFIG.shared_cluster_size, 0x28000);
        assert_eq!(
            T8140_MANAGER_CONFIG.submission_backend,
            identity::T8140_G17P_SUBMISSION_BACKEND
        );
        assert_eq!(T8140_MANAGER_CONFIG.work_channel_count, 12);
    }

    #[test]
    fn type4_diagnostic_fields_match_the_pinned_a000_consumers() {
        const GFX_DATA_BASE: u64 = 0x0000_0100_01d7_c000;
        const GFX_DATA_SIZE: u64 = 0x13_4000;
        assert_eq!(
            GFX_DATA_BASE + (G17P_SCHEDULER_ROOT_LINK_ADDRESS - G17P_GFX_LINK_DATA_BASE),
            G17P_SCHEDULER_ROOT_PHYSICAL_ADDRESS,
        );
        assert_eq!(G17P_GFX_LINK_DATA_BASE, 0xffff_fc00_0004_c000);
        assert_eq!(G17P_SCHEDULER_ROOT_PHYSICAL_ADDRESS, 0x0000_0100_01ea_7190);
        assert_eq!(
            G17P_SCHEDULER_ROOT_PHYSICAL_ADDRESS + G17P_SCHEDULER_ROOT_STATE_OFFSET as u64,
            0x0000_0100_01ea_71f8,
        );
        assert_eq!(
            GFX_DATA_BASE + (G17P_RUNTIME_MAIN_GLOBAL_LINK_ADDRESS - G17P_GFX_LINK_DATA_BASE),
            G17P_RUNTIME_MAIN_GLOBAL_PHYSICAL_ADDRESS,
        );
        assert_eq!(G17P_RUNTIME_MAIN_GLOBAL_PHYSICAL_ADDRESS, 0x0000_0100_01ea_7030);
        assert_eq!(
            GFX_DATA_BASE + (G17P_MBI_GLOBAL_LINK_ADDRESS - G17P_GFX_LINK_DATA_BASE),
            0x0000_0100_01d9_e840,
        );
        assert_eq!(G17P_MBI_STATIC_ARRAY_LINK_ADDRESS, 0xffff_fc00_0006_e800);
        assert_eq!(G17P_MBI_GROUP_COUNT, 8);
        assert_eq!(G17P_MBI_TRACE_CAPACITY, 128);
        assert_eq!(G17P_MBI_TRACE_RECORD_SIZE, 0x18);
        assert_eq!(
            GFX_DATA_BASE
                + (G17P_PRIMARY_PEER_LEASE_LINK_ADDRESS - G17P_GFX_LINK_DATA_BASE),
            G17P_PRIMARY_PEER_LEASE_PHYSICAL_ADDRESS,
        );
        assert_eq!(G17P_PRIMARY_PEER_LEASE_PHYSICAL_ADDRESS, 0x0000_0100_01ea_7798);
        assert_eq!(
            G17P_GFX1_PHYSICAL_DATA_BASE
                + (G17P_GFX1_PEER_LEASE_LINK_ADDRESS - G17P_GFX1_LINK_DATA_BASE),
            G17P_GFX1_PEER_LEASE_PHYSICAL_ADDRESS,
        );
        assert_eq!(G17P_GFX1_PEER_LEASE_PHYSICAL_ADDRESS, 0x0000_0100_01fd_11c8);
        assert!(
            G17P_SCHEDULER_ROOT_PHYSICAL_ADDRESS + G17P_SCHEDULER_ROOT_SNAPSHOT_SIZE as u64
                <= GFX_DATA_BASE + GFX_DATA_SIZE
        );
        assert_eq!(G17P_TYPE4_EP_HANDLER_LINK_ADDRESS, 0xffff_fc00_0002_2e24);
        assert_eq!(G17P_TYPE4_THUNK_LINK_ADDRESS, 0xffff_fc00_0002_635c);
        assert_eq!(G17P_TYPE4_DRAIN_LINK_ADDRESS, 0xffff_fc00_0000_3a74);
        assert_eq!(G17P_TYPE4_PENDING_MASK, 1 << 17);
        assert_eq!(
            G17P_SCHEDULER_DEFERRED_REQUEUE_LINK_ADDRESS,
            0xffff_fc00_0002_5ea4
        );
        assert_eq!(
            G17P_SCHEDULER_ROOT_LINK_ADDRESS + G17P_SCHEDULER_ROOT_PENDING_OFFSET as u64,
            0xffff_fc00_0017_7190
        );
        assert_eq!(
            G17P_SCHEDULER_ROOT_LINK_ADDRESS + G17P_SCHEDULER_ROOT_DEFERRED_OFFSET as u64,
            0xffff_fc00_0017_71e0
        );
        assert_eq!(
            G17P_SCHEDULER_ROOT_LINK_ADDRESS + G17P_SCHEDULER_ROOT_STATE_OFFSET as u64,
            0xffff_fc00_0017_71f8
        );
        let control = g17_initdata::channel_table_entry_range(
            g17_initdata::CONTROL_CHANNEL_INDEX,
        )
        .unwrap();
        assert_eq!(control.start, G17P_TYPE4_MAIN_CONTROL_POINTER_OFFSETS[0]);
        assert_eq!(
            G17P_TYPE4_MAIN_CONTROL_POINTER_OFFSETS,
            [control.start, control.start + 8, control.start + 0x10]
        );
    }

    #[test]
    fn pinned_j700_a000_instruction_fixtures_prove_the_type4_pointer_chain() {
        const BOOT_MAIN: &[u8] =
            include_bytes!("fixtures/g17p_j700_a000/boot-main-config-global.hex");
        const SELECTOR: &[u8] = include_bytes!("fixtures/g17p_j700_a000/type4-selector.hex");
        const PUBLISH: &[u8] = include_bytes!("fixtures/g17p_j700_a000/type4-publish.hex");
        const COMMON: &[u8] =
            include_bytes!("fixtures/g17p_j700_a000/type4-common-publish.hex");
        const TAKE: &[u8] =
            include_bytes!("fixtures/g17p_j700_a000/type4-pending-take.hex");
        const DEFERRED: &[u8] =
            include_bytes!("fixtures/g17p_j700_a000/scheduler-deferred.hex");
        const THUNK: &[u8] = include_bytes!("fixtures/g17p_j700_a000/type4-thunk.hex");
        const DISPATCH: &[u8] =
            include_bytes!("fixtures/g17p_j700_a000/type4-dispatch-record.hex");
        const CURSORS: &[u8] =
            include_bytes!("fixtures/g17p_j700_a000/control-drain-cursors.hex");

        let boot = decode_hex::<0x30>(BOOT_MAIN);
        assert_eq!(
            adrp_target(0xffff_fc00_0001_e9dc, instruction(&boot, 8)),
            0xffff_fc00_0017_7000
        );
        assert_eq!(load_store_unsigned_offset(instruction(&boot, 9)), 0x30);

        let selector = decode_hex::<0x1c>(SELECTOR);
        let extract = instruction(&selector, 0);
        assert_eq!((extract >> 5) & 0x1f, 1);
        assert_eq!(extract & 0x1f, 8);
        assert_eq!((extract >> 16) & 0x3f, 48);
        assert_eq!(
            ((extract >> 10) & 0x3f) - ((extract >> 16) & 0x3f) + 1,
            6
        );
        let compare_type4 = instruction(&selector, 5);
        assert_eq!((compare_type4 >> 5) & 0x1f, 8);
        assert_eq!((compare_type4 >> 10) & 0xfff, 4);
        assert_eq!(
            conditional_branch_target(0xffff_fc00_0002_2db0, instruction(&selector, 6)),
            G17P_TYPE4_EP_HANDLER_LINK_ADDRESS
        );

        let publish = decode_hex::<0x10>(PUBLISH);
        assert_eq!(
            adrp_target(G17P_TYPE4_EP_HANDLER_LINK_ADDRESS, instruction(&publish, 0)),
            0xffff_fc00_0017_7000
        );
        assert_eq!(add_immediate(instruction(&publish, 1)), 0x190);
        assert_eq!(wide_immediate(instruction(&publish, 2)), G17P_TYPE4_PENDING_MASK);
        assert_eq!(
            branch_target(
                G17P_TYPE4_EP_HANDLER_LINK_ADDRESS + 0xc,
                instruction(&publish, 3)
            ),
            0xffff_fc00_0002_2ebc
        );

        let common = decode_hex::<0xc>(COMMON);
        assert_eq!(instruction(&common, 0), 0xf8e8_3008);
        let take = decode_hex::<0x20>(TAKE);
        assert_eq!(instruction(&take, 4), 0xf83c_1275);

        let deferred = decode_hex::<0x14>(DEFERRED);
        assert_eq!(
            adrp_target(
                G17P_SCHEDULER_DEFERRED_REQUEUE_LINK_ADDRESS,
                instruction(&deferred, 0)
            ),
            0xffff_fc00_0017_7000
        );
        assert_eq!(load_store_unsigned_offset(instruction(&deferred, 1)), 0x1e0);
        assert_eq!(load_store_unsigned_offset(instruction(&deferred, 3)), 0x1e0);
        assert_eq!(
            0xffff_fc00_0017_7000 + 0x1e0,
            G17P_SCHEDULER_ROOT_LINK_ADDRESS + G17P_SCHEDULER_ROOT_DEFERRED_OFFSET as u64
        );

        let thunk = decode_hex::<0x28>(THUNK);
        assert_eq!(
            adrp_target(G17P_TYPE4_THUNK_LINK_ADDRESS, instruction(&thunk, 0)),
            0xffff_fc00_0017_7000
        );
        assert_eq!(load_store_unsigned_offset(instruction(&thunk, 1)), 0x1f8);
        assert_eq!(instruction(&thunk, 2), 0x721d_051f);
        assert_eq!(
            conditional_branch_target(
                G17P_TYPE4_THUNK_LINK_ADDRESS + 0xc,
                instruction(&thunk, 3)
            ),
            G17P_TYPE4_THUNK_LINK_ADDRESS + 0x24
        );
        assert_eq!(
            adrp_target(G17P_TYPE4_THUNK_LINK_ADDRESS + 0x10, instruction(&thunk, 4)),
            0xffff_fc00_0017_7000
        );
        assert_eq!(load_store_unsigned_offset(instruction(&thunk, 5)), 0x30);
        assert_eq!(load_store_unsigned_offset(instruction(&thunk, 6)), 0x1b0);
        assert_eq!(load_store_unsigned_offset(instruction(&thunk, 7)), 0);
        assert_eq!(
            branch_target(G17P_TYPE4_THUNK_LINK_ADDRESS + 0x20, instruction(&thunk, 8)),
            G17P_TYPE4_DRAIN_LINK_ADDRESS
        );

        let dispatch = decode_hex::<0x18>(DISPATCH);
        assert_eq!(
            u64::from_le_bytes(dispatch[0..8].try_into().unwrap()),
            G17P_TYPE4_PENDING_MASK
        );
        let signed_target = u64::from_le_bytes(dispatch[8..16].try_into().unwrap());
        assert_eq!(signed_target >> 48, 0x8030);
        assert_eq!(
            signed_target & 0xffff_ffff,
            G17P_TYPE4_THUNK_LINK_ADDRESS & 0xffff_ffff
        );
        assert_eq!(u64::from_le_bytes(dispatch[16..24].try_into().unwrap()), 0);

        let cursors = decode_hex::<0x18>(CURSORS);
        assert_eq!(load_store_unsigned_offset(instruction(&cursors, 1)), 0x1a8);
        assert_eq!(load_store_unsigned_offset(instruction(&cursors, 3)), 0x1a0);
        assert_eq!(
            G17P_TYPE4_MAIN_CONTROL_POINTER_OFFSETS,
            [0x1a0, 0x1a8, 0x1b0]
        );
    }

    #[test]
    fn exact_target_and_shared_dual_role_resources_pass() {
        assert_eq!(validate_target(target()), Ok(()));
        assert_eq!(validate_resources(resources()), Ok(()));
    }

    #[test]
    fn scratch_geometry_uses_values_not_accelerator_object_offsets() {
        let platform = T8140_G17P_PLATFORM_CONFIG;
        assert_eq!(platform.sksm_scratch.write0_base, 0x4000);
        assert_eq!(platform.sksm_scratch.write1_base, 0x4008);
        assert_eq!(platform.sksm_scratch.stride, 0x10);
        assert_eq!(
            platform.sksm_queue_geometry,
            g17_submission::g17_sksm_queue_geometry(
                g17_submission::G17ChannelAbiTarget::A18ProG17P,
                g17_submission::G17ClSubmissionBackend::G17PHal200Hybrid,
            )
            .unwrap()
        );
        assert_eq!(platform.sksm_queue_geometry.validate(), Ok(()));
        assert_eq!(
            platform.sksm_scratch.fifo_offsets(),
            Ok(G17SksmScratchOffsets {
                write0: 0x4090,
                write1: 0x4098,
            })
        );
        assert_eq!(validate_platform(platform), Ok(()));
    }

    #[test]
    fn non_g17p_scratch_geometry_fails_before_resource_allocation() {
        for geometry in [
            G17SksmScratchGeometry {
                write0_base: 0,
                ..T8140_G17P_PLATFORM_CONFIG.sksm_scratch
            },
            G17SksmScratchGeometry {
                write1_base: 0,
                ..T8140_G17P_PLATFORM_CONFIG.sksm_scratch
            },
            G17SksmScratchGeometry {
                stride: 0,
                ..T8140_G17P_PLATFORM_CONFIG.sksm_scratch
            },
        ] {
            assert_eq!(
                validate_platform(G17PPlatformConfig {
                    sksm_scratch: geometry,
                    ..T8140_G17P_PLATFORM_CONFIG
                }),
                Err(ConstructionError::WrongSksmScratchGeometry)
            );
        }
    }

    #[test]
    fn scratch_geometry_overflow_fails_before_resource_allocation() {
        for geometry in [
            G17SksmScratchGeometry {
                write0_base: 0,
                write1_base: 0,
                stride: u32::MAX,
            },
            G17SksmScratchGeometry {
                write0_base: u32::MAX,
                write1_base: 0,
                stride: 1,
            },
            G17SksmScratchGeometry {
                write0_base: 0,
                write1_base: u32::MAX,
                stride: 1,
            },
        ] {
            assert_eq!(
                geometry.fifo_offsets(),
                Err(ConstructionError::SksmScratchOffsetOverflow)
            );
        }
    }

    #[test]
    fn non_t8140_identity_and_uat_geometry_are_invalid() {
        let mut observed = target();
        observed.chip_id = 0x8142;
        assert_eq!(validate_target(observed), Err(ConstructionError::WrongChip));

        let mut observed = target();
        observed.declared_variant = GpuVariant::G;
        assert_eq!(
            validate_target(observed),
            Err(ConstructionError::WrongDeclaredIdentity)
        );

        let mut observed = target();
        observed.hw_family = 0x0b;
        assert_eq!(
            validate_target(observed),
            Err(ConstructionError::WrongHardwareIdentity)
        );

        let mut observed = target();
        observed.num_dies = 2;
        assert_eq!(
            validate_target(observed),
            Err(ConstructionError::WrongHardwareIdentity)
        );

        let mut observed = target();
        observed.uat_ias = 39;
        assert_eq!(
            validate_target(observed),
            Err(ConstructionError::WrongUatGeometry)
        );
    }

    #[test]
    fn split_null_or_wrong_role_uat_is_invalid() {
        let mut observed = resources();
        observed.primary_uat_owner = 0;
        assert_eq!(
            validate_resources(observed),
            Err(ConstructionError::NullUatOwner)
        );

        let mut observed = resources();
        observed.secondary_uat_owner += 0x4000;
        assert_eq!(
            validate_resources(observed),
            Err(ConstructionError::SplitUatOwner)
        );

        let mut observed = resources();
        observed.primary_role = InstanceRole::Secondary;
        assert_eq!(
            validate_resources(observed),
            Err(ConstructionError::WrongRoleTopology)
        );
    }

    #[test]
    fn root_or_shared_cluster_mismatch_is_invalid() {
        let mut observed = resources();
        observed.secondary_root += 0x4000;
        assert_eq!(
            validate_resources(observed),
            Err(ConstructionError::WrongRootPlacement)
        );

        let mut observed = resources();
        observed.shared_cluster_size -= 0x4000;
        assert_eq!(
            validate_resources(observed),
            Err(ConstructionError::WrongSharedClusterSize)
        );
    }
}
