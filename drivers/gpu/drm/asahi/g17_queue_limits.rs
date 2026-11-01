// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Allocation contract for independent G17P queues.
//! Every accepted DRM queue owns an execution context; hardware compute graphs
//! are allocated lazily when that logical queue first submits compute.
pub(crate) const FEATURE_INDEPENDENT_QUEUES: u64 = 1 << 2;
/// GET_PARAMS feature shared with DRM_ASAHI_FEATURE_PER_COMMAND_INPUT_SYNCS.
/// Input fences gate every hardware command, including error propagation;
/// userspace need not serialize independent commands behind the first one.
pub(crate) const FEATURE_PER_COMMAND_INPUT_SYNCS: u64 = 1 << 3;
pub(crate) const PARAM_GROUP_QUEUE_LIMITS: u32 = 2;
pub(crate) const QUEUE_LIMITS_SIZE: usize = 16;
pub(crate) const TTBAT_CONTEXT_COUNT: u32 = 64;
pub(crate) const COMPUTE_CONTEXT_FIRST: u32 = 5;
/// The current exclusive renderer needs one duplicate app-context root.
pub(crate) const RENDER_APP_CONTEXT: u32 = TTBAT_CONTEXT_COUNT - 1;
/// Ordinary queues consume no second bind context. Context 1 permanently owns
/// the device-global render FList closure after bootstrap; contexts 5..62 are
/// dynamically allocated application-GART roots and context 63 remains the
/// driver-owned bootstrap render identity.
pub(crate) const BIND_CONTEXT_COUNT: u32 = 0;
pub(crate) const BIND_CONTEXT_FIRST: u32 = RENDER_APP_CONTEXT - BIND_CONTEXT_COUNT;
pub(crate) const BIND_CONTEXT_MASK: u64 =
    ((1u64 << RENDER_APP_CONTEXT) - 1) & !((1u64 << BIND_CONTEXT_FIRST) - 1);
pub(crate) const COMPUTE_CONTEXT_MASK: u64 =
    ((1u64 << BIND_CONTEXT_FIRST) - 1) & !((1u64 << COMPUTE_CONTEXT_FIRST) - 1);
pub(crate) const MAX_QUEUES: u32 = COMPUTE_CONTEXT_MASK.count_ones();
/// Physical TA/3D pairs per logical queue. Native G17P (AGXG17P 353.14) uses
/// two work queues per command queue and pipelines further commands on each,
/// ordered by firmware kick parents, instead of waiting for a retirement.
pub(crate) const MAX_IN_FLIGHT_PER_QUEUE: u32 = 2;
/// Renders a physical pair may hold at once: one per retained command
/// generation of its render storage.
pub(crate) const RENDER_KICKS_PER_PAIR: u32 =
    crate::g17_resources::G17P_RENDER_MAX_IN_FLIGHT as u32;
/// Compute kicks one logical queue may have published and unretired at once.
///
/// Every CL kick entry carries an implicit dependency on the previous kick of
/// its own QID (`prepare_g17p_cl_kick_entry`, the same implicit parent native
/// AGXKSMKickQueueG16 encodes), and cross-engine order is carried by explicit
/// native stamp barriers. So firmware, not host retirement, orders
/// consecutive compute jobs: the host publishes kick n+1 without waiting for
/// kick n to retire. This bound only sizes host bookkeeping (the in-flight
/// FIFO, completion requests and per-publication channel records); it is far
/// below the queue's 256 stamps and 320 item-ring publications.
pub(crate) const MAX_COMPUTE_IN_FLIGHT_PER_QUEUE: u32 = 16;
/// Per-publication tag-14/15/16 record slots. A slot is reused only
/// `COMPUTE_RECORD_SLOTS` publications later, by which time its owner has
/// retired, and retirement requires firmware to have consumed its records.
pub(crate) const COMPUTE_RECORD_SLOTS: usize = 2 * MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize;
const _: () = assert!(COMPUTE_RECORD_SLOTS > MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize);
/// Two GiB boundary inside the existing four GiB runtime reservation. In
/// addition to descriptor arrays, each independently executing CL graph owns
/// a 23-MiB private-memory pool. Keep room for every advertised queue without
/// reducing concurrency; the remaining 2 GiB hold per-VM private aliases.
pub(crate) const CLIENT_LOW_VA_START: u64 = 0x70_8000_0000;

/// Host ownership eligibility for an empty graph, not firmware detachment.
/// Callers must separately establish whether the graph was installed and what
/// firmware references survive. Command completion alone is not a proven
/// cross-VM graph-switch contract; the repeat path does not reinstall the QID.
pub(crate) const fn reusable_idle_compute_graph(
    owner_free: bool,
    active_free: bool,
    binding_free: bool,
    context_free: bool,
    quarantined: bool,
) -> bool {
    owner_free && active_free && binding_free && context_free && !quarantined
}

/// An idle graph whose exact client has gone but whose binding is deliberately
/// retained while the same physical QID is handed to a replacement owner.
///
/// The firmware keeps the installed slot pointer for a queue until something
/// clears it, and the only clearing transaction the driver has --
/// `unregister_compute_sksm_queue`'s disable pair -- is a suppressed no-op
/// whenever `g17p_sksm_mmio` is 0, which is its default because the SKSM port
/// raises an SError on this SoC. So a released queue's SKSM entry mapping and
/// context root must outlive the client. Rebinding preserves the installed
/// graph and monotonic stamp sequence, while the outgoing root remains owned
/// through the replacement submission's exact completion.
///
/// This predicate never authorizes releasing roots. Timeout, fault and
/// ambiguous publication still quarantine the graph through processor stop.
pub(crate) const fn rebindable_released_compute_graph(
    owner_free: bool,
    active_free: bool,
    released: bool,
    quarantined: bool,
) -> bool {
    owner_free && active_free && released && !quarantined
}

pub(crate) fn encode_limits() -> [u8; QUEUE_LIMITS_SIZE] {
    let mut bytes = [0; QUEUE_LIMITS_SIZE];
    bytes[0..4].copy_from_slice(&MAX_QUEUES.to_ne_bytes());
    bytes[4..8].copy_from_slice(&MAX_IN_FLIGHT_PER_QUEUE.to_ne_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_graph_can_be_rebound_after_exact_owner_retirement() {
        assert!(reusable_idle_compute_graph(true, true, true, true, false));
    }

    #[test]
    fn active_owned_or_quarantined_graph_is_never_rebound() {
        assert!(!reusable_idle_compute_graph(false, true, true, true, false));
        assert!(!reusable_idle_compute_graph(true, false, true, true, false));
        assert!(!reusable_idle_compute_graph(true, true, false, true, false));
        assert!(!reusable_idle_compute_graph(true, true, true, false, false));
        assert!(!reusable_idle_compute_graph(true, true, true, true, true));
    }
}
