# G17P Vulkan bring-up checkpoints

Keep this list short and evidence-only. A build is not a hardware checkpoint.

## Previously verified

- Vulkan compute is stable on commit `ef3cc0d683485`: the staged 1, 2, 100 and
  1000 exact-output gates all pass with an empty module parameter set. That
  same boot retained QID 4 through 1103 real hardware retires before the first
  render submission.
- Render TA: the 32x32 Vulkan path publishes and executes TA; both TA hardware
  timestamps become non-zero.
- The older QID-1 fragment path reached firmware admission and assigned PDM
  pipe 0 (`state=0x0100000d`), but did not enter 3D activation.
- Fragment descriptor reachability: all four SKSM RCE bindings resolve through
  the submitting VM to the same physical descriptor bytes written by the CPU.

## Current boundary

- Compute and render no longer overwrite one global region-view topology.
  Compute automatically publishes same-backing scheduler/B2 low/high pairs;
  render automatically publishes its PB/UMA pairs at its own submission
  boundary. The compute entry also restores Metal's `0x003fffffffffffff`
  auxiliary value and the first-work context-0 empty-high transition.
- Queue identity is now one kernel-owned protocol, not a parameter. QIDs 0..3
  stay reserved, compute starts at 4, TA/3D use 5/6 with their matching
  `base + qid * 0x28000` low/high aliases, and rebuilt compute graphs consume
  7..127 without wrapping. Tag-15, tag-14, outer slots, QoS, SKSM entries,
  barriers, TA's 3D link, descriptor metadata and compute recovery all use the
  queue object's ID.
- Hardware accepted the coherent QID-5/6 graph: the QoS bitmap became `0x60`,
  the outer slots carried 5/6, TA executed with real timestamps, and both
  queues retired their three item records. Fragment activation still did not
  run: descriptor `+0x20f0` and both 3D timestamps remained zero.
- SGX `+0x10408` remains at its `0x100` idle/reset value, no bit-3 completion
  interrupt appears, and the resource callback stays idle because all three
  are downstream of the missing new-work interrupt/job allocation.
- The resource-manager clear/go masks and callback state stay zero throughout
  the live post-doorbell polls. KTrace `0x44/0x45` appears only after recovery
  starts, so it is recovery teardown, not the stalled admission callback.
- B1's later 3D activation routine at `0x162f8` writes `1` to fragment
  descriptor `+0x20f0`. On hardware `+0x20f0` and both 3D timestamps remain
  zero, consistent with the earlier state-1 callback stall.
- Metal keeps the type-1 fragment descriptor's `+0x79c=0` and packed
  `+0x7a8=0x042c0059` even while its actual 3D queue is QID 6 or 15. Kext RE
  proves these are the reserved word and `{0x42c bytes, 0x59 records}` header
  of an SKU register stream, not a KSM dependency. Its TA descriptor instead
  links dynamically to the actual 3D QID/stamp, and Linux matches that link.
- B1's bit-4 new-work handler registers the 0x9a0-byte TA descriptor read from
  SGX `+0xc0f0`. FUN_14cd4 follows TA `+0x79c/+0x7a8`, writes the per-launch
  slot/mode/strobe at `+0x10398/+0x10408/+0x1c998`, and the bit-3 completion
  callback ultimately calls FUN_162f8 on the real 3D KSM entry. The current
  boundary is before that callback chain: TA executes, while `+0x10408` stays
  idle and 3D `+0x20f0` stays zero.

## Corrected claims

- Fragment has not yet executed. Ring counters reaching `3/3/3`, the job list
  becoming empty after recovery, and `3D completion scratch=...timestamp=1`
  are not proof of execution. The scratch value is staged by the host before
  the doorbell, and recovery retires the software-visible ring state.
- The earlier "fragment engine reaches hardware completion" progress update was
  incorrect and is retracted.

## Eliminated at this boundary

- RCE register values, PTC maximum, descriptor tail fields, shader/VDM content,
  and the four RCE VM mappings cannot prevent entry into `0x162f8`; firmware has
  not reached the point where it reads them.
- Native has no tag-4 command for this same-context TA-to-3D dependency.
- Native HardwareBufferID values are allocator results, not fixed ABI values.
- A Linux-only render power request is not the cause. With the power-wire mode
  disabled Linux matches Metal's message ordering, while the same launch bit-3
  failure remains.

## Grounded parameter work

- Native PTC formula is `max(min_count, min(input / max_concurrent_kicks,
  max_count))`, with Neo limits `0x60..0x200` and priority-2 divisor 3.
- The traced cold 32x32 pool state computes TA `0xa9` and 3D `0xd5`. Linux's
  `0x60` is the native zero/unprepared fallback, not the correct prepared value.
- These are per-submit computed values, not architecture-wide literals. The
  durable kernel model still needs stage-specific inputs/computed maxima.

## Next experiment

- On the automatic empty-parameter path, retain the firmware KTrace and sample
  B1's exact bit-4/bit-3 IRQ status plus the new-work/completion sub-bitmaps,
  launch slot/mode/strobe and TA descriptor link. Compare that state directly
  with Metal's TA descriptor and firmware callback contract, then fix the first
  mismatched producer or registration field in the kernel.
