// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! Windowed host submission accounting and optional GPU interval statistics.
//!
//! The asynchronous DRM path supplies host ioctl/publication costs only.
//! GPU statistics require separately supplied firmware completion intervals;
//! an empty interval set means unmeasured, not idle. Submission counts are
//! accepted jobs, not completed jobs or presented frames.
//!
//! Production arithmetic stays in `u64`: the kernel has no 128-bit division
//! helper. Tests use wider arithmetic as an independent rounding reference.

/// Nominal GPU timestamp rate, in kHz.
///
/// The render pass envelope uses firmware's 24 MHz command-time counter, the
/// rate `g17_job::G17P_COMMAND_TIMESTAMP_HZ` converts to UAPI nanoseconds.
/// `SubmitReport::tick_khz` re-derives the rate from each window so a wrong
/// constant is visible instead of silent.
pub(crate) const NOMINAL_TICK_KHZ: u64 = 24_000;

/// Opt-in subdivisions of one G17P render publication. The bounded trace
/// prints these after preparation, so its printk cannot inflate inner stages.
#[derive(Default)]
pub(crate) struct RenderStageTimes {
    pub(crate) enabled: bool,
    pub(crate) runtime_lock_ns: u64,
    pub(crate) handoff_ns: u64,
    pub(crate) prepare_ns: u64,
    pub(crate) restage_ns: u64,
    pub(crate) reachability_ns: u64,
    pub(crate) reachability_cache_hit: bool,
    pub(crate) flist_request_ns: u64,
    pub(crate) tvb_blocks: u32,
    pub(crate) retained: bool,
    /// Finer split of the publication path, recorded only for renders that
    /// carry a timing-buffer marker (record type 4). Indices are the
    /// `SUB_*` constants below; values are nanoseconds.
    pub(crate) sub_ns: [u32; SUB_COUNT],
    /// Lap origin for `lap`; `None` whenever sub-stage timing is off.
    pub(crate) lap_origin: Option<LapInstant>,
    /// ARM generic-timer counts (24 MHz) at the doorbell send, for joining
    /// host publication to the firmware's pass-start/pass-end ticks.
    pub(crate) doorbell_cntvct: u64,
    /// Exact selected item publication, captured only with existing timing.
    pub(crate) config_present: [bool; 2],
    pub(crate) item_targets: [u32; 2],
    pub(crate) item_plan_valid: bool,
}

/// Sub-stage indices of `RenderStageTimes::sub_ns`.
pub(crate) const SUB_FRONT: usize = 0; // entry: slot select, recovery settle
pub(crate) const SUB_DEPS: usize = 1; // recycle check, dependency snapshot
pub(crate) const SUB_BEGIN: usize = 2; // HWBID, TVB checks, region views
pub(crate) const SUB_FLIST: usize = 3; // PB retarget, FList owner, lease token
pub(crate) const SUB_SKSM: usize = 4; // SKSM queue staging (tag-15 groups)
pub(crate) const SUB_CONFIG: usize = 5; // tag-15 headers, QoS, pair configure
pub(crate) const SUB_OPENING: usize = 6; // PM op, partial-opening pair publish
pub(crate) const SUB_TRACKER: usize = 7; // mapping audits, tracker push
pub(crate) const SUB_POST: usize = 8; // power release, completion install
pub(crate) const SUB_DOORBELL: usize = 9; // producer publish + RTKit doorbell
pub(crate) const SUB_COMMIT: usize = 10; // commit and return
pub(crate) const SUB_COUNT: usize = 11;

#[cfg(not(test))]
pub(crate) type LapInstant = kernel::time::Instant<kernel::time::Monotonic>;
#[cfg(test)]
pub(crate) type LapInstant = std::time::Instant;

#[cfg(not(test))]
fn lap_now() -> LapInstant { LapInstant::now() }
#[cfg(test)]
fn lap_now() -> LapInstant { std::time::Instant::now() }

#[cfg(not(test))]
fn lap_ns(origin: LapInstant) -> u64 { origin.elapsed().as_nanos().max(0) as u64 }
#[cfg(test)]
fn lap_ns(origin: LapInstant) -> u64 { origin.elapsed().as_nanos() as u64 }

impl RenderStageTimes {
    /// Begin sub-stage timing; a no-op unless this render is traced.
    #[inline]
    pub(crate) fn lap_start(&mut self) {
        if self.enabled && self.lap_origin.is_none() {
            self.lap_origin = Some(lap_now());
        }
    }

    /// Restart the lap without charging it: the interval is measured by one
    /// of the pre-existing stage fields and must not be counted twice.
    #[inline]
    pub(crate) fn skip(&mut self) {
        if self.lap_origin.is_some() {
            self.lap_origin = Some(lap_now());
        }
    }

    /// Charge the time since the previous lap to `index` and restart the lap.
    #[inline]
    pub(crate) fn lap(&mut self, index: usize) {
        if let Some(origin) = self.lap_origin {
            let ns = lap_ns(origin);
            let now = lap_now();
            if let Some(slot) = self.sub_ns.get_mut(index) {
                *slot = slot.saturating_add(u32::try_from(ns).unwrap_or(u32::MAX));
            }
            self.lap_origin = Some(now);
        }
    }
}

/// Backoff schedule for a completion poll loop.
///
/// The render wait used to sleep a flat `fsleep(Delta::from_millis(1))`
/// between polls. On this kernel `fsleep` grants 25% slack
/// (`max_slack_shift = 2`) and 1000 us is below `USLEEP_RANGE_UPPER_BOUND`
/// (16000 us at `CONFIG_HZ=250`), so that call is `usleep_range(1000, 1250)`:
/// every render paid a full millisecond or more of pure sleep *after* the GPU
/// had already finished, and a submission could not be observed complete any
/// sooner than that no matter how short the pass was.
///
/// Exponential backoff fixes both ends: the first poll after publication comes
/// 20 us later, so a short pass is observed promptly, and a long pass settles
/// back to the same 1 ms interval so a slow render costs no more CPU than it
/// did before. The schedule is deliberately a plain state machine with no
/// clock in it, so it can be unit-tested off-target.
pub(crate) struct PollSchedule {
    next_us: u32,
    cap_us: u32,
}

impl PollSchedule {
    /// First interval of the adaptive schedule, in microseconds.
    ///
    /// `fsleep` uses `udelay` -- a busy wait -- for 10 us and below, so
    /// staying above that keeps every poll interval a real sleep. 20 us
    /// becomes `usleep_range(20, 25)`.
    pub(crate) const MIN_US: u32 = 20;
    /// Ceiling of the adaptive schedule, in microseconds. Identical to the
    /// interval the flat loop used, so a long wait costs what it always did.
    pub(crate) const MAX_US: u32 = 1000;

    /// `fixed_us == 0` selects the adaptive schedule. Any other value pins the
    /// interval, which is how the old flat behaviour is restored for an A/B
    /// measurement (`g17p_render_poll_us=1000`).
    pub(crate) const fn new(fixed_us: u32) -> Self {
        if fixed_us == 0 {
            Self {
                next_us: Self::MIN_US,
                cap_us: Self::MAX_US,
            }
        } else {
            Self {
                next_us: fixed_us,
                cap_us: fixed_us,
            }
        }
    }

    /// Interval to sleep before the next poll, doubling up to the cap.
    pub(crate) fn next_delay_us(&mut self) -> u32 {
        let delay = self.next_us;
        let doubled = delay.saturating_mul(2);
        self.next_us = if doubled > self.cap_us {
            self.cap_us
        } else {
            doubled
        };
        delay
    }
}

/// What one completed render submission cost.
#[derive(Default, Copy, Clone)]
pub(crate) struct RenderSample {
    /// Wall time inside the submit ioctl, in nanoseconds.
    pub(crate) ioctl_ns: u64,
    /// Wall time inside `submit_translated_render`, in nanoseconds.
    pub(crate) publish_ns: u64,
    /// Wall time inside the completion wait, in nanoseconds.
    pub(crate) wait_ns: u64,
    /// Completion polls made, including the one that observed completion.
    pub(crate) polls: u32,
    /// Native GPU pass envelope, in GPU ticks, when the firmware published one.
    pub(crate) gpu_interval: Option<[u64; 2]>,
}

/// Accumulated counters for one reporting window.
#[derive(Default)]
pub(crate) struct SubmitWindow {
    open: bool,
    renders: u32,
    computes: u32,
    intervals: u32,
    ioctl_ns: u64,
    ioctl_ns_max: u64,
    publish_ns: u64,
    wait_ns: u64,
    compute_ioctl_ns: u64,
    polls: u64,
    gpu_ticks: u64,
    gpu_ticks_max: u64,
    first_start: u64,
    last_end: u64,
    window_index: u64,
}

/// One window's finished measurement, in units a log line can print directly.
///
/// Fixed-point suffixes name the scale: `_x10` is tenths, `_x100` hundredths.
#[derive(Default, Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct SubmitReport {
    pub(crate) window_index: u64,
    pub(crate) renders: u32,
    pub(crate) computes: u32,
    pub(crate) intervals: u32,
    pub(crate) wall_us: u64,
    pub(crate) renders_per_s_x100: u64,
    pub(crate) ioctl_avg_us_x10: u64,
    pub(crate) ioctl_max_us_x10: u64,
    pub(crate) publish_avg_us_x10: u64,
    pub(crate) wait_avg_us_x10: u64,
    pub(crate) compute_avg_us_x10: u64,
    pub(crate) polls_avg_x10: u64,
    pub(crate) gpu_avg_us_x10: u64,
    pub(crate) gpu_max_us_x10: u64,
    /// GPU busy ticks over the tick span the window covered, in tenths of a
    /// percent. Both terms are GPU-written timestamps, so this needs no
    /// timebase at all. Two render slots may overlap, so it can exceed 100%.
    pub(crate) occupancy_pct_x10: u64,
    /// Host time spent inside the submit ioctl over wall time, tenths of a
    /// percent. This is what the synchronous render wait costs the client.
    pub(crate) ioctl_pct_x10: u64,
    /// Tick rate measured from this window: tick span divided by wall span.
    /// Zero when the window carried fewer than two published intervals.
    pub(crate) tick_khz: u64,
}

/// Divide, rounding to nearest, without overflowing on the rounding term.
const fn div_round(numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 {
        0
    } else {
        numerator / denominator
            + (numerator % denominator >= denominator - denominator / 2) as u64
    }
}

/// Scale a nanosecond count to tenths of a microsecond.
const fn ns_to_us_x10(ns: u64) -> u64 {
    div_round(ns, 100)
}

impl SubmitWindow {
    /// Reset the counters and mark the window open.
    ///
    /// Wall time is the caller's: the kernel's `Instant` does not expose its
    /// absolute nanoseconds outside the `kernel` crate, so the caller holds
    /// the opening `Instant` and passes the elapsed span to `close`. That also
    /// keeps this module free of kernel types and unit-testable off-target.
    pub(crate) fn open(&mut self) {
        let index = self.window_index;
        *self = Self::default();
        self.open = true;
        self.window_index = index;
    }

    /// Whether `open` has been called since the last `close`.
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn submissions(&self) -> u32 {
        self.renders.saturating_add(self.computes)
    }

    pub(crate) fn record_render(&mut self, sample: RenderSample) {
        self.renders = self.renders.saturating_add(1);
        self.ioctl_ns = self.ioctl_ns.saturating_add(sample.ioctl_ns);
        if sample.ioctl_ns > self.ioctl_ns_max {
            self.ioctl_ns_max = sample.ioctl_ns;
        }
        self.publish_ns = self.publish_ns.saturating_add(sample.publish_ns);
        self.wait_ns = self.wait_ns.saturating_add(sample.wait_ns);
        self.polls = self.polls.saturating_add(sample.polls as u64);
        let Some([start, end]) = sample.gpu_interval else {
            return;
        };
        if end <= start {
            return;
        }
        let busy = end - start;
        self.intervals = self.intervals.saturating_add(1);
        self.gpu_ticks = self.gpu_ticks.saturating_add(busy);
        if busy > self.gpu_ticks_max {
            self.gpu_ticks_max = busy;
        }
        if self.first_start == 0 || start < self.first_start {
            self.first_start = start;
        }
        if end > self.last_end {
            self.last_end = end;
        }
    }

    pub(crate) fn record_compute(&mut self, ioctl_ns: u64) {
        self.computes = self.computes.saturating_add(1);
        self.compute_ioctl_ns = self.compute_ioctl_ns.saturating_add(ioctl_ns);
        self.ioctl_ns = self.ioctl_ns.saturating_add(ioctl_ns);
        if ioctl_ns > self.ioctl_ns_max {
            self.ioctl_ns_max = ioctl_ns;
        }
    }

    /// Close the window over an elapsed span of `wall_ns` and report it.
    ///
    /// The window index advances here so consecutive reports are visibly
    /// consecutive and a dropped line is detectable.
    pub(crate) fn close(&mut self, wall_ns: u64) -> SubmitReport {
        self.open = false;
        let renders = self.renders as u64;
        let computes = self.computes as u64;
        let submissions = renders + computes;
        let tick_span = self.last_end.saturating_sub(self.first_start);
        let report = SubmitReport {
            window_index: self.window_index,
            renders: self.renders,
            computes: self.computes,
            intervals: self.intervals,
            wall_us: div_round(wall_ns, 1_000),
            renders_per_s_x100: if wall_ns == 0 {
                0
            } else {
                div_round(renders.saturating_mul(100_000_000_000), wall_ns)
            },
            ioctl_avg_us_x10: ns_to_us_x10(div_round(self.ioctl_ns, submissions)),
            ioctl_max_us_x10: ns_to_us_x10(self.ioctl_ns_max),
            publish_avg_us_x10: ns_to_us_x10(div_round(self.publish_ns, renders)),
            wait_avg_us_x10: ns_to_us_x10(div_round(self.wait_ns, renders)),
            compute_avg_us_x10: ns_to_us_x10(div_round(self.compute_ioctl_ns, computes)),
            polls_avg_x10: div_round(self.polls.saturating_mul(10), renders),
            gpu_avg_us_x10: div_round(
                div_round(self.gpu_ticks, self.intervals as u64).saturating_mul(10_000),
                NOMINAL_TICK_KHZ,
            ),
            gpu_max_us_x10: div_round(self.gpu_ticks_max.saturating_mul(10_000), NOMINAL_TICK_KHZ),
            occupancy_pct_x10: div_round(self.gpu_ticks.saturating_mul(1_000), tick_span),
            ioctl_pct_x10: div_round(self.ioctl_ns.saturating_mul(1_000), wall_ns),
            tick_khz: if self.intervals < 2 {
                0
            } else {
                div_round(tick_span.saturating_mul(1_000_000), wall_ns)
            },
        };
        self.window_index = self.window_index.wrapping_add(1);
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_matches_a_wide_reference_at_integer_boundaries() {
        let values = [0, 1, 2, 3, 99, 100, 101, u64::MAX / 2, u64::MAX - 1, u64::MAX];
        for numerator in values {
            for denominator in values {
                let expected = if denominator == 0 {
                    0
                } else {
                    ((numerator as u128 + denominator as u128 / 2) / denominator as u128) as u64
                };
                assert_eq!(div_round(numerator, denominator), expected,
                           "{numerator} / {denominator}");
            }
        }
    }

    #[test]
    fn saturated_accounting_does_not_overflow_when_reporting() {
        let mut window = SubmitWindow::default();
        window.open();
        window.record_compute(u64::MAX);
        window.record_compute(u64::MAX);
        let report = window.close(1_000);
        assert!(report.ioctl_avg_us_x10 > 0);
        assert!(report.ioctl_pct_x10 > 0);
    }

    #[test]
    fn adaptive_schedule_doubles_from_twenty_to_one_thousand_and_stays() {
        let mut schedule = PollSchedule::new(0);
        let observed: [u32; 8] = core::array::from_fn(|_| schedule.next_delay_us());
        assert_eq!(observed, [20, 40, 80, 160, 320, 640, 1000, 1000]);
    }

    #[test]
    fn a_pinned_interval_never_changes() {
        let mut schedule = PollSchedule::new(1000);
        for _ in 0..8 {
            assert_eq!(schedule.next_delay_us(), 1000);
        }
    }

    #[test]
    fn occupancy_and_tick_rate_come_out_of_the_gpu_timestamps() {
        // Four renders, each 2400 ticks (100 us at 24 MHz) of GPU work,
        // starting every 24000 ticks (1 ms). Wall time 4 ms.
        let mut window = SubmitWindow::default();
        window.open();
        for index in 0..4u64 {
            let start = 10_000 + index * 24_000;
            window.record_render(RenderSample {
                ioctl_ns: 500_000,
                publish_ns: 120_000,
                wait_ns: 370_000,
                polls: 6,
                gpu_interval: Some([start, start + 2_400]),
            });
        }
        let report = window.close(4_000_000);
        assert_eq!(report.renders, 4);
        assert_eq!(report.intervals, 4);
        assert_eq!(report.wall_us, 4_000);
        // 4 renders in 4 ms is 1000/s.
        assert_eq!(report.renders_per_s_x100, 100_000);
        assert_eq!(report.ioctl_avg_us_x10, 5_000);
        assert_eq!(report.publish_avg_us_x10, 1_200);
        assert_eq!(report.wait_avg_us_x10, 3_700);
        assert_eq!(report.polls_avg_x10, 60);
        assert_eq!(report.gpu_avg_us_x10, 1_000);
        assert_eq!(report.gpu_max_us_x10, 1_000);
        // Busy 4*2400 = 9600 ticks over a span of 3*24000 + 2400 = 74400.
        assert_eq!(report.occupancy_pct_x10, div_round(9_600 * 1_000, 74_400));
        // Host spent 4*500us of 4ms inside the ioctl: 50.0%.
        assert_eq!(report.ioctl_pct_x10, 500);
        // 74400 ticks in 4 ms is 18600 kHz -- deliberately NOT 24000, so the
        // test proves the rate is measured rather than assumed.
        assert_eq!(report.tick_khz, 18_600);
    }

    #[test]
    fn a_window_with_no_published_interval_reports_zero_instead_of_dividing_by_zero() {
        let mut window = SubmitWindow::default();
        window.open();
        window.record_render(RenderSample {
            ioctl_ns: 1_000,
            publish_ns: 400,
            wait_ns: 600,
            polls: 1,
            gpu_interval: None,
        });
        window.record_render(RenderSample {
            ioctl_ns: 1_000,
            publish_ns: 400,
            wait_ns: 600,
            // A retirement-only pair: start == end is not an interval.
            gpu_interval: Some([7, 7]),
            polls: 1,
        });
        let report = window.close(2_000_000);
        assert_eq!(report.renders, 2);
        assert_eq!(report.intervals, 0);
        assert_eq!(report.gpu_avg_us_x10, 0);
        assert_eq!(report.occupancy_pct_x10, 0);
        assert_eq!(report.tick_khz, 0);
    }

    #[test]
    fn compute_submissions_are_counted_without_a_render_denominator() {
        let mut window = SubmitWindow::default();
        window.open();
        window.record_compute(20_000);
        window.record_compute(40_000);
        let report = window.close(1_000_000);
        assert_eq!(report.renders, 0);
        assert_eq!(report.computes, 2);
        assert_eq!(report.compute_avg_us_x10, 300);
        assert_eq!(report.ioctl_avg_us_x10, 300);
        assert_eq!(report.publish_avg_us_x10, 0);
        assert_eq!(report.renders_per_s_x100, 0);
    }

    #[test]
    fn overlapping_renders_report_over_one_hundred_percent_occupancy() {
        let mut window = SubmitWindow::default();
        window.open();
        // Two renders of 1000 ticks each covering the same 1000-tick span.
        window.record_render(RenderSample {
            gpu_interval: Some([100, 1_100]),
            ..Default::default()
        });
        window.record_render(RenderSample {
            gpu_interval: Some([100, 1_100]),
            ..Default::default()
        });
        let report = window.close(1_000_000);
        assert_eq!(report.occupancy_pct_x10, 2_000);
    }
}
