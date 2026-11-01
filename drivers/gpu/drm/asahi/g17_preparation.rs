// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Host preparation ownership across recovery and permanent removal.
//!
//! Ordinary admission/release uses one atomic state word. The wait mutex is
//! used only by recovery/removal and the final preparer waking a closed epoch.
use core::sync::atomic::{AtomicU64, Ordering};

const COUNT_BITS: u32 = 20;
const COUNT_MASK: u64 = (1 << COUNT_BITS) - 1;
const CLOSED: u64 = 1 << COUNT_BITS;
const REMOVED: u64 = CLOSED << 1;
const FAILED: u64 = CLOSED << 2;
const EPOCH_STEP: u64 = CLOSED << 3;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum AdmissionError { Closed, Removed, Failed, Full }

struct PreparationState(AtomicU64);
impl PreparationState {
    fn new() -> Self { Self(AtomicU64::new(EPOCH_STEP)) }

    fn enter(&self) -> Result<u64, AdmissionError> {
        let mut state = self.0.load(Ordering::Acquire);
        loop {
            if state & REMOVED != 0 { return Err(AdmissionError::Removed); }
            if state & FAILED != 0 { return Err(AdmissionError::Failed); }
            if state & CLOSED != 0 { return Err(AdmissionError::Closed); }
            if state & COUNT_MASK == COUNT_MASK { return Err(AdmissionError::Full); }
            match self.0.compare_exchange_weak(state, state + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(state & !COUNT_MASK),
                Err(observed) => state = observed,
            }
        }
    }

    /// True only for the last reader of a closed epoch; its caller must wake
    /// joiners using their wait mutex to avoid a check/sleep lost wakeup.
    fn leave(&self) -> bool {
        let previous = self.0.fetch_sub(1, Ordering::AcqRel);
        previous & COUNT_MASK == 1 && previous & CLOSED != 0
    }

    fn current(&self, epoch: u64) -> bool {
        self.0.load(Ordering::Acquire) & !COUNT_MASK == epoch
    }

    /// Invalidate every old token before waiting. Repeated closure coalesces;
    /// epoch exhaustion permanently closes admission rather than wrapping.
    fn close(&self, permanent: bool) {
        let mut state = self.0.load(Ordering::Acquire);
        loop {
            let mut next = state | CLOSED;
            if state & CLOSED == 0 {
                next = next.checked_add(EPOCH_STEP).unwrap_or(state | CLOSED | REMOVED);
            }
            if permanent { next |= REMOVED; }
            if next == state { return; }
            match self.0.compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(observed) => state = observed,
            }
        }
    }

    fn fail(&self) { self.close(false); self.0.fetch_or(CLOSED | FAILED, Ordering::AcqRel); }

    fn drained(&self) -> bool { self.0.load(Ordering::Acquire) & COUNT_MASK == 0 }

    fn reopen(&self) -> bool {
        let mut state = self.0.load(Ordering::Acquire);
        loop {
            if state & (REMOVED | COUNT_MASK) != 0 { return false; }
            if state & CLOSED == 0 { return true; }
            match self.0.compare_exchange_weak(state, state & !(CLOSED | FAILED), Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(observed) => state = observed,
            }
        }
    }
}

#[cfg(not(test))]
mod runtime {
    use super::PreparationState;
    use kernel::{prelude::*, new_condvar, new_mutex, sync::{Arc, CondVar, Mutex}};

    #[pin_data]
    pub(crate) struct G17PPreparations {
        state: PreparationState,
        #[pin]
        join: Mutex<()>,
        #[pin]
        drained: CondVar,
    }

    impl G17PPreparations {
        pub(crate) fn new() -> Result<Arc<Self>> {
            Arc::pin_init(pin_init!(Self {
                state: PreparationState::new(),
                join <- new_mutex!((), "G17PPreparations::join"),
                drained <- new_condvar!("G17PPreparations::drained"),
            }), GFP_KERNEL)
        }

        pub(crate) fn enter(self: &Arc<Self>) -> Result<G17PPreparationLease> {
            let epoch = self.state.enter().map_err(|error| match error {
                super::AdmissionError::Closed => EAGAIN,
                super::AdmissionError::Removed | super::AdmissionError::Failed => ENODEV,
                super::AdmissionError::Full => ENOSPC,
            })?;
            Ok(G17PPreparationLease { owner: self.clone(), epoch })
        }

        /// Only recovery closure can wait here; ordinary open admission takes
        /// no mutex. Waiting owns neither runtime nor any preparation lease.
        pub(crate) fn enter_wait(self: &Arc<Self>) -> Result<G17PPreparationLease> {
            match self.enter() {
                Err(error) if error == EAGAIN => {}
                result => return result,
            }
            let mut join = self.join.lock();
            loop {
                match self.enter() {
                    Err(error) if error == EAGAIN => {}
                    result => return result,
                }
                if self.drained.wait_interruptible(&mut join) { return Err(ERESTARTSYS); }
            }
        }

        /// v84: as `enter_wait`, but uninterruptible. Used only by the submit
        /// ioctl's early publication, which already holds a physical lease and
        /// must not turn a signal during recovery into a failed render.
        pub(crate) fn enter_wait_uninterruptible(self: &Arc<Self>) -> Result<G17PPreparationLease> {
            match self.enter() {
                Err(error) if error == EAGAIN => {}
                result => return result,
            }
            let mut join = self.join.lock();
            loop {
                match self.enter() {
                    Err(error) if error == EAGAIN => {}
                    result => return result,
                }
                self.drained.wait(&mut join);
            }
        }

        pub(crate) fn close(&self, permanent: bool) {
            self.state.close(permanent);
            if permanent {
                let _join = self.join.lock();
                self.drained.notify_all();
            }
        }
        pub(crate) fn is_removed(&self) -> bool { self.state.0.load(super::Ordering::Acquire) & super::REMOVED != 0 }
        pub(crate) fn is_drained(&self) -> bool { self.state.drained() }
        pub(crate) fn reopen(&self) -> bool {
            let _join = self.join.lock();
            let reopened = self.state.reopen();
            if reopened { self.drained.notify_all(); }
            reopened
        }

        pub(crate) fn fail(&self) {
            let _join = self.join.lock();
            self.state.fail();
            self.drained.notify_all();
        }

        /// Never call while holding runtime or a pair lock a preparer needs.
        /// The final closed-epoch lease takes this same mutex before notifying.
        pub(crate) fn wait_for_preparers(&self) {
            let mut join = self.join.lock();
            while !self.state.drained() { self.drained.wait(&mut join); }
        }
    }

    /// Keep this after tentative graph/mapping fields in an owning transaction,
    /// so their destruction precedes decrementing the active-preparer count.
    pub(crate) struct G17PPreparationLease {
        owner: Arc<G17PPreparations>,
        epoch: u64,
    }

    impl G17PPreparationLease {
        /// The caller owns runtime at commit. Existing work-state preparation
        /// transfers back to runtime here; future graph builds must also install
        /// their exact graph/pair ownership before relinquishing this lease.
        pub(crate) fn is_current(&self) -> bool { self.owner.state.current(self.epoch) }

        pub(crate) fn finish(self) -> Result {
            if self.owner.state.current(self.epoch) { Ok(()) } else { Err(ECANCELED) }
        }
    }

    impl Drop for G17PPreparationLease {
        fn drop(&mut self) {
            if self.owner.state.leave() {
                let _join = self.owner.join.lock();
                self.owner.drained.notify_all();
            }
        }
    }
}
#[cfg(not(test))]
pub(crate) use runtime::{G17PPreparationLease, G17PPreparations};
