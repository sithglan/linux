# Deferred render ownership at scheduler teardown

The immutable full-resolution Buoy fragment workload rendered for 60 seconds
but poisoned the device when its worker was terminated. Captures reported
`render-submission-lost`, ECANCELED, and `firmware-retired=false` around the
shutdown boundary. This reproduced with qualified DRM c3080a3c4674, default-off
SCS DRM f89f12475eba, and a clean retry while other shader testing was stopped.
The retry's read-only GPU-FD capture observed the desktop and vulkan-buoy;
it did not observe an experimental shader client.

Two source-level lifetime problems were found:

1. DRM scheduler destruction calls the render watch's cancel callback for
   pending jobs. Render publication precedes the scheduler watch. The callback
   immediately treated that cancellation as GPU failure, even when the GPU
   was merely finishing the exiting process's last frame.
2. The failure entry selected a physical slot without checking its current
   completion object. A stale watch could therefore settle a successor's slot.

The candidate waits on the retained hardware fence only during scheduler
teardown, outside the runtime mutex, so the independent RTKit event worker can
retire it. The wait is uninterruptible because an exiting task may already
carry a fatal signal, and bounded to the render scheduler's existing two-second
timeout. Success returns without touching a slot. A genuine timeout retains
the existing final-observation and quarantine behavior. Every render failure
entry now checks the exact completion Arc before touching runtime state.

This does not add waits to ordinary submissions, reduce slot count or batching,
or suppress a real timeout. Identity checking alone did not fix the Buoy
shutdown reproduction; the retained fence wait addresses that separate cause.

Host tests compile the actual entry point and cancellation callback. Against
209ff90dfb078, the two identity cases fail and two of three cancellation cases
fail; all five pass with the change. The mocked wait asserts the runtime mutex
is not held, the wait is uninterruptible and finite, and ordinary watchdogs do
not perform it. VM handoff, output-fence, HardwareBufferID and retained-FList
tests remain enabled and pass. The owned-scheduler model also passes after
updating its stale mock mutex/pin-init API and obsolete packet constructor
argument; those mock compilation failures reproduce on unchanged 209ff90.

Candidate render-teardown-r2:

- SHA256 `50c9a1170046b552918e3d5c94518fbbe90ff31245a25eb0c127cac472e3245a`
- build ID `623488e8f0d41099aa4e746287f5b5b1b60c9005`
- vermagic `7.1.6-neo-g17p2 SMP preempt mod_unload aarch64`
- undefined imports unchanged from the qualified module

Two full-resolution immutable fragment retests now pass their 60-second run
and process termination on boot 39f9e5ac-2904-4588-a254-493208a38eb0:
`buoy-20260916T230152Z` (2,423 confirmed fenced frames) and
`buoy-20260916T230340Z` (2,325). No timeout, quarantine, lost-submission or
session-rebuild marker appeared; the desktop remains alive. The unchanged
baseline had 2,430 frames before its shutdown failure: these measurements do
not suggest a throughput tradeoff, but are not a dedicated performance study.
Chromium and post-close device qualification are still pending. This is not
full conformance qualification.
