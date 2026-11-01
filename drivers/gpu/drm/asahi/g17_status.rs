// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! CPU-owned sticky VM failure reporting. No GPU mapping or firmware access.
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, Ordering};
use kernel::prelude::*;

pub(crate) const PARAM_GROUP_VM_STATUS: u32 = 1;
pub(crate) const FEATURE_VM_STATUS: u64 = 1 << 1;
pub(crate) const VM_STATUS_SIZE: usize = 16;
/// v87: GET_PARAMS group 3 registers a VM-status mirror word (feature bit 4).
/// Request (16 bytes, input only): vm_id, GEM handle of a buffer (at most
/// 64 KiB) owned by the calling file, byte offset of a 4-byte-aligned i32
/// inside it, flags=0.
pub(crate) const PARAM_GROUP_VM_STATUS_MIRROR: u32 = 3;
pub(crate) const FEATURE_VM_STATUS_MIRROR: u64 = 1 << 4;
pub(crate) const VM_STATUS_MIRROR_SIZE: usize = 16;

/// Keeps the mirror word's buffer and its kernel mapping alive for as long as
/// the status can be recorded.
#[cfg(not(test))]
pub(crate) type MirrorOwner = crate::gem::ObjectRef;
#[cfg(test)]
pub(crate) type MirrorOwner = ();

/// Shared by the VM and accepted jobs; never cleared by queue destruction or
/// successful work. A newly created VM receives a new status object.
///
/// v87 mirror: userspace may register one word that always holds `get()`
/// (0 until the first recorded error, then that error). The word is written
/// by `record` before it returns, i.e. before any fence that the recording
/// path signals afterwards, so a successful wait followed by a load of the
/// word observes exactly what a GET_PARAMS group 1 query would return (when
/// the query merges no device-wide error; see `supports_vm_status_mirror`).
pub(crate) struct VmStatus {
    error: AtomicI32,
    mirror: AtomicPtr<i32>,
    mirror_claimed: AtomicBool,
    mirror_owner: UnsafeCell<Option<MirrorOwner>>,
    /// `g17p_recovery_isolation` bit3: monotonic ns of the last recovery in
    /// which this VM was guilty but spared (0 = never).
    recovery_grace_ns: AtomicU64,
    /// VM id for diagnostics only (0 = unknown).
    vm_id: u64,
}
// SAFETY: `mirror_owner` is written once, by the single thread that won
// `mirror_claimed`, before `mirror` is published; it is otherwise only
// dropped with the status. All other fields are atomics.
unsafe impl Sync for VmStatus {}
// SAFETY: see Sync; the owner is a GEM reference and its kernel mapping.
unsafe impl Send for VmStatus {}

impl VmStatus {
    pub(crate) fn new() -> Self { Self::new_for(0) }

    /// Status of VM `vm_id`; the id only names the VM in diagnostics.
    pub(crate) fn new_for(vm_id: u64) -> Self {
        Self {
            error: AtomicI32::new(0),
            mirror: AtomicPtr::new(core::ptr::null_mut()),
            mirror_claimed: AtomicBool::new(false),
            mirror_owner: UnsafeCell::new(None),
            recovery_grace_ns: AtomicU64::new(0),
            vm_id,
        }
    }

    pub(crate) fn vm_id(&self) -> u64 { self.vm_id }

    /// Classify this VM for one recovery (see `g17_recovery_isolation`).
    /// Consumes the guilty grace when it is granted.
    pub(crate) fn recovery_class(&self, guilty: bool, grace: bool, now_ns: u64) -> u8 {
        let last = self.recovery_grace_ns.load(Ordering::Acquire);
        let (class, consumed) =
            crate::g17_recovery_isolation::classify(guilty, grace, now_ns, last);
        if consumed {
            self.recovery_grace_ns.store(now_ns.max(1), Ordering::Release);
        }
        class
    }

    /// Register the mirror word. `word` must point to a 4-byte-aligned i32
    /// inside a mapping `owner` keeps alive. One registration per status.
    ///
    /// Ordering (all SeqCst): attach stores the pointer, then loads `error`;
    /// `record` stores `error`, then loads the pointer. At least one side
    /// sees the other, and every mirror store after the zeroing below writes
    /// the one sticky error, so the word never regresses to 0.
    pub(crate) fn attach_mirror(&self, owner: MirrorOwner, word: *mut i32) -> Result {
        if word.is_null() || word as usize & 3 != 0 { return Err(EINVAL); }
        if self.mirror_claimed.swap(true, Ordering::AcqRel) { return Err(EBUSY); }
        // SAFETY: the claim above makes this the only writer; nothing reads
        // the owner before the status is dropped.
        unsafe { *self.mirror_owner.get() = Some(owner); }
        // SAFETY: checked non-null and aligned; `owner` keeps it mapped.
        unsafe { AtomicI32::from_ptr(word) }.store(0, Ordering::SeqCst);
        self.mirror.store(word, Ordering::SeqCst);
        let error = self.error.load(Ordering::SeqCst);
        if error != 0 {
            // SAFETY: as above.
            unsafe { AtomicI32::from_ptr(word) }.store(error, Ordering::SeqCst);
        }
        Ok(())
    }

    fn publish_mirror(&self, error: i32) {
        let word = self.mirror.load(Ordering::SeqCst);
        if !word.is_null() {
            // SAFETY: published only by `attach_mirror`, whose owner is
            // retained by this status for its whole lifetime.
            unsafe { AtomicI32::from_ptr(word) }.store(error, Ordering::SeqCst);
        }
    }

    /// Poisoning a VM status is terminal for that client: every later
    /// submission refuses at `stage=vm-status`.  Until now nothing said WHO
    /// poisoned it, so a wedged machine showed 114 `stage=vm-status` failures
    /// across 135 client files with no way to attribute a single one.  Name the
    /// caller on the 0 -> non-zero transition only, so this is one line per VM
    /// that actually dies rather than per submission.
    /// Does this error mean "this work will never run on this device"?
    ///
    /// Only those may be recorded on a VM's sticky status, because that status
    /// permanently retires the VM: every later submission refuses at
    /// `stage=vm-status`. `G17PQueue::note_submission_failure` already states
    /// the policy -- "Contention and rejection do not: an EBUSY from the render
    /// lease, an interrupted wait, or a malformed command buffer all leave a
    /// perfectly healthy VM, and recording a sticky error for one of those
    /// would retire a client that merely lost a frame" -- but two paths
    /// recorded unconditionally and so contradicted it.
    ///
    /// Measured on G17P 2026-09-18: a compute submission arriving while a
    /// firmware recovery was still open logged "firmware recovery still open
    /// after 200 polls; refusing to publish" and then poisoned its VM with
    /// EBUSY. Recovery is transient by definition. 52 further VMs were poisoned
    /// in the cascade and the GPU was dead until reboot.
    pub(crate) fn is_device_loss(error: i32) -> bool {
        error == -(kernel::error::code::EIO.to_errno().unsigned_abs() as i32)
            || error == -(kernel::error::code::ETIMEDOUT.to_errno().unsigned_abs() as i32)
            || error == -(kernel::error::code::ENODEV.to_errno().unsigned_abs() as i32)
    }

    /// Record only if the error actually means the device is gone for this VM.
    /// The caller still reports the error to its own client through the fence.
    #[track_caller]
    pub(crate) fn record_if_device_loss(&self, error: i32) {
        if Self::is_device_loss(error) {
            self.record(error);
        }
    }

    #[track_caller]
    pub(crate) fn record(&self, error: i32) {
        if error < 0 {
            if self
                .error
                .compare_exchange(0, error, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                // v87: the mirror holds the error before record() returns.
                self.publish_mirror(error);
                let loc = core::panic::Location::caller();
                pr_err!(
                    "G17PMARK vm-status POISONED vm={} err={} by {}:{}\n",
                    self.vm_id,
                    error,
                    loc.file(),
                    loc.line(),
                );
            }
        }
    }
    pub(crate) fn get(&self) -> i32 { self.error.load(Ordering::Acquire) }
}

/// GET_PARAMS group 1 is an in/out request: vm_id, flags=0, error(out), pad=0.
/// Decode once, before lookup, so a racing userspace edit cannot change owner.
pub(crate) fn decode_request(bytes: &[u8; VM_STATUS_SIZE]) -> Option<u32> {
    let word = |i| u32::from_ne_bytes(bytes[i..i+4].try_into().unwrap());
    if word(4) != 0 || word(12) != 0 { return None; }
    Some(word(0))
}
pub(crate) fn encode_response(vm_id: u32, error: i32) -> [u8; VM_STATUS_SIZE] {
    let mut bytes = [0; VM_STATUS_SIZE];
    bytes[0..4].copy_from_slice(&vm_id.to_ne_bytes());
    bytes[8..12].copy_from_slice(&error.to_ne_bytes());
    bytes
}

/// GET_PARAMS group 3 request: (vm_id, GEM handle, byte offset). Decode once;
/// flags must be zero and the offset naturally aligned for an i32.
pub(crate) fn decode_mirror_request(bytes: &[u8; VM_STATUS_MIRROR_SIZE]) -> Option<(u32, u32, u32)> {
    let word = |i| u32::from_ne_bytes(bytes[i..i+4].try_into().unwrap());
    if word(12) != 0 || word(8) & 3 != 0 || word(4) == 0 { return None; }
    Some((word(0), word(4), word(8)))
}
