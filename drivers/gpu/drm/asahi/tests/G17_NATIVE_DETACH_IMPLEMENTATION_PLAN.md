# G17 native resource release: evidence and implementation boundary

Status: host-side native audit complete; firmware acknowledgement contract **not
established**. This document proposes a Linux lifecycle. It does not enable or
implement opcode `0x13` release, published-QID recycling, or early root/control
reclamation. Opcode `0x13` is decimal 19, not the decimal-13 event used elsewhere.

## Pinned evidence and reproducibility

Local inputs in `/home/dj/Developer/Asahi/research/kernelcache/kexts/`:

| Binary | SHA256 |
| --- | --- |
| `com.apple.AGXG17X` | `ccbe120a3a197038216426b507b5409873397b947308239b4ecc75d76759e87f` |
| `com.apple.iokit.IOGPUFamily` | `cd124ce0fb35e9af5fab2950e4d975e739b52a7e7754cdfcc1853e077c180e19` |

Run `audit_g17_native_detach.py AGX_BINARY IOGPU_BINARY` from this directory.
It checks both hashes, 52 instruction witnesses, the opcode route-table entry,
five encoded virtual-function targets, and 521,218 host wait-predicate boundary
and wrap cases. All passed. These checks reproduce host observations; they do
not execute firmware or prove that its release handler has finished.

Addresses below are unslid: add `0xfffffe0000000000` to each short hex address.
Virtual target reconstruction uses the encoded low 32 bits plus
`0xfffffe0007004000`. Its agreement with the inspected functions is evidence,
but original kernelcache chained-fixup metadata is unavailable here. The exact
Neo J700 firmware consumer has not been qualified against these binaries.

## Record and identity

`AGXArmFirmware::submitReleaseResource` at `0x8b37c74` zeroes a 64-byte record,
then writes the following little-endian layout:

| Offset | Width | Observed value/source |
| --- | --- | --- |
| `0x00` | 4 | Opcode `0x13` |
| `0x04` | 4 | Zero |
| `0x08` | 1 | Argument D: control byte `0x26` |
| `0x09` | 1 | Argument A: control byte `0x00` |
| `0x0a` | 1 | Argument B: control byte `0x01` |
| `0x0b` | 1 | Argument C: control byte `0x04` |
| `0x0c` | 8 | Converted resource GPU VA; unaligned field |
| `0x14` | 44 | Zero |

The converter override at `0x8b37c68` returns its input unchanged in this image.
The local submission-flags word is initialized to 2; it is **not** record `+4`.

The caller, `AGXCommandQueueReclaimableState::free` at `0x8b033d8`, loads the
resource GPU VA from state `+0xe0` and the CPU control pointer from state `+0xc8`.
It loads A/B/C/D at `0x8b0341c..0x8b03428`, and invokes the firmware method at
`0x8b0344c` while holding a native read lock. The state initializer establishes
the CPU and GPU views of the selected state-pool object. It initially sets
control bytes `[0..1]` to `0xffff`; control `[0x26]` comes through command queue
`+0x488`, object byte `+0x120` (`0x8b03320..0x8b0332c`).

These offsets are observed. The complete semantic meaning, writers and lifetime
of the four bytes remain unresolved. In particular, do not fill them from host
QIDs or owner IDs by assumption. Capture their coherent values after draining,
including firmware modifications and invalid-sentinel handling.

## Publication and notification

The u32 routing table at `0x714fa08` has entry 19 equal to zero. The selected
ring is firmware `+0xa50 + route * 0x130 + 0x68`, hence `+0xab8`.

`AGXFirmware::submitDeviceControl` at `0x8b10bfc` reads producer and consumer,
checks `(producer + 1) & 0xff == consumer` for fullness, and waits/rechecks when
full. There are 256 slots with one reserved empty slot: capacity 255. Its
command-gate deadline uses a 10 ms interval; an overall timeout for this
full-ring loop has not been established.

After admission it copies exactly 64 bytes, returns the **written slot index**
through the out-parameter (`0x8b10db4`), executes `dmb ish` (`0x8b10e18`), then
publishes `(producer + 1) & 0xff`. The written slot and next producer are distinct.
A conditional virtual helper between the copy and barrier is not invoked for
opcode 19 according to the inspected mask and byte table. That helper's meaning
is unresolved; this does not establish a power-management contract.

The Arm override at `0x8b37f7c` can set notification flag bit 0 at occupancy
193 or greater. Opcode 19 does not select its additional type-`0x3b` path.
On successful submission with flags `& 3 != 0`, it sends mailbox value
`0x0084000000000011` through the route-zero controller at firmware `+0x1980`.
The target firmware's endpoint and host power/tally obligations still require
qualification. Linux's current opcode-power classification is not disproved
by an unidentified native helper being skipped.

## Exact host wait and failure behavior

The producer snapshots the initial **consumer** before submitting. If submission
returns nonzero, or submission flag bit 1 is clear afterward, it skips the wait.
Otherwise it calls `AGXFirmware::waitForDeviceControlCompletion` at `0x8b0dd6c`
with opcode 19, the initial consumer and the written slot. Register dataflow
establishes the opcode argument even though the recovered symbol's prototype
looks different.

The selected ring's virtual getters load u32 consumer at state `+0` and producer
at state `+0x20`, each checked below 256. The separate CFI word at state `+0x10`
is not read by this wait. At `0x8b0de58..0x8b0de64`, the actual
`SUB W / CMP / CCMP #4,LS / B.NE` sequence implements:

```text
S = initial consumer
T = written slot
R = current consumer
W = current producer
span  = u32(T - S)
delta = u32(R - S)
host_wait_success = (delta > span) || (R == W)
```

The subtraction wraps at 32 bits although cursor values are 8-bit bounded.
Do not substitute `R >= T`, equality with next producer, or an 8-bit-distance
test without proving equivalence under the intended publication invariants.
The target slot is inclusive in the pending interval. Empty-ring success relies
on valid cursors, capacity discipline and unchanged firmware generation.
Multiple ring revolutions between observations are not qualified by this audit.

The wait returns zero on that host condition. It returns `0xe00002bc` on a
deadline or one of the inspected failure conditions:

- Budget: accelerator u32 `+0x27c` multiplied by one billion and converted from
  nanoseconds to absolute time; elapsed absolute time is compared with it.
- Nonzero word through accelerator `+0x158`, object `+0xf0`.
- `isFirmwareStoppedForRecovery`; the Arm override at `0x8b30388` reads a word
  through firmware `+0xcb0`, object `+0x4ae0`.
- Accelerator byte `+0x688`, bit 0 set.

The two raw accelerator fields' full semantics remain unresolved. The loop
also conditionally services an object through accelerator `+0x12b38`; there is
no explicit sleep in the inspected wait loop. Linux should not busy-spin under
its global runtime mutex. The cursor success check precedes these failure
checks, so Linux must independently reject a stale generation or fault snapshot
before reclaiming anything, even when the ring appears empty.

Crucially, native `free` does **not check the release return value**: instruction
`0x8b03450` continues cleanup immediately. Later it frees the state-pool slot and
clears `+0xc8/+0xe0`. Any outer native stop/reset/quiescence guarantee that makes
that safe has not been proved here. This is not permission to free on Linux
timeout or failure.

## Separate ownership transitions

IOGPU's stamp/QID management is not the resource-release acknowledgement:

| Operation | Observed native rule |
| --- | --- |
| `releaseStampIndex`, `0xa866c74` | Records a release sequence under the event-machine lock; skips the invalid sequence sentinel. It does not clear the owner or perform firmware detach. |
| `requestStampIndexForChannel`, `0xa8688fc` | Cached reuse checks exact owner. An occupied candidate must be released and its completed/target stamp subtraction must show no pending work. It chooses an eligible older release sequence, then installs the new owner. Previous-device bookkeeping also runs; its full target semantics remain unresolved. |
| `freeStampIndexForChannel`, `0xa868cd0` | Checks exact owner before clearing it. This function itself has no completed-stamp check; caller preconditions matter. |

UAT invalidation is another operation: `AGXUATSingleStage::invalidateGPTBATEntry`
at `0x8b995c0` clears both valid bits, barriers, invalidates the translation, and
finishes with a DSB (`0x8b99604`). The G17X context-LUT setter at `0x8b5d678` is a
no-op. Do not invent a missing LUT write or substitute a userspace fence for
firmware detach, released-stamp eligibility, or root invalidation.

## Proposed Linux integration, after ABI qualification

Use an owner lifecycle `Open -> Draining -> DetachPending -> Reclaimable`.
Fault, timeout, ambiguous publication or generation change must retain/quarantine
the resources until a stronger acknowledgement or proven processor stop.

1. Atomically close admission for the exact logical owner. Cancel only genuinely
   unpublished leases, retire already-published jobs and account for all TA/3D/CL
   references to shared scheduler control. A single engine's close is insufficient.
2. Retain the control Arc, relevant roots/mappings and stable channel identities.
   Snapshot the verified firmware identity bytes. Allocate one bounded release
   ticket containing session generation, ring identity, initial consumer, written
   slot, next producer and a host publication sequence. Do not publish a command
   until the firmware contract below is established.
3. Admit and publish atomically with other device-control producers. Preserve the
   one-empty-slot invariant, record-before-producer ordering and the qualified
   notification/power accounting. A no-credit result must leave record bytes,
   producer and power counters unchanged.
4. Service tickets in bounded batches with brief ownership locking. Wait outside
   the runtime mutex and use deadline/event-driven servicing; no per-poll or
   per-completion allocation and no global GPU execution gate. Validate the same
   generation, exact owner and qualified acknowledgement before transitioning.
5. Reclaim each resource only after its own prerequisites: detach the shared
   scheduler object, release/reassign eligible exact-owner QIDs, remove mappings
   and invalidate roots with the required barriers. The relative order must be
   derived from the firmware consumer and reference graph; it is not finalized
   by this host audit. Drop the control only when no remaining channel/root uses it.

The existing `G17PDeviceControlPublication` already reports opcode, argument,
slot, record address, consumer-before, producer-before and producer-after.
`publish_primary_device_control_raw` validates CPU views before writes, writes
the record, accounts for its configured power assertion, fences and publishes.
The subsequent [stable-channel audit](G17_STABLE_CHANNEL_AUDIT.md) reproduced
full-ring cursor aliasing in this production method and added an atomic capacity
check before any write: invalid cursors return `EIO`, and a full ring returns
`EAGAIN`. Mandatory TVB replies now use a one-record, generation-bound outbox
with exact-once retry and worker backoff outside the runtime mutex. This does
not provide a general asynchronous-release outbox; that still needs design
before adding detach traffic.
The capacity guard does not establish a detach acknowledgement or qualify sustained
ring saturation. No opcode-`0x13` release was enabled by that correction.

Other hot-path observations: dependency/admission waits are outside backend
publication locking; completion processes bounded channel batches; logical
control retention uses an O(1) object marker; parked render storage is allocated
on first owner selection, not each switch. Preserve these properties. Profile
mutex hold time, completion batch length, ring occupancy and allocation pressure
before adding lookup structures or changing batch sizes. None of this is a
measured throughput or latency claim: corrected hardware load results are pending.

## Missing firmware facts and capture plan

The blocking fact is whether consumer advancement occurs **after all effects of
opcode 19**, including secondary-ASC references, in-flight work and cached state,
or merely after fetching/dispatching the record. No separate resource-specific
acknowledgement field is established by the host record.

Capture a legitimate native close on matching firmware, first idle and then
with preceding work draining. Pin the firmware UUID/hash and handler dispatch;
trace the handler's completion relative to consumer updates, relevant secondary
ASC traffic and resource accesses. Record the resource address, control bytes
`0/1/4/0x26` before and after, producer/consumer/CFI, session/fault state,
notification route and power/tally transitions. Establish all identity writers,
sentinel handling, handler validation and error behavior. Correlate released
stamp eligibility, owner replacement, control detach and root invalidation.

Exercise natural wrap and full-ring pressure after the basic ABI is known.
Do not send guessed records or malformed identities to the device to fill these
gaps. Initially test stale owner/generation, delayed acknowledgements, full-ring
no-write behavior and failure paths in host models/production-method tests.

Qualification must then demonstrate:

- No reclaim before the verified firmware effect; timeout/fault/reset races retain
  resources, and a late acknowledgement cannot release a newer owner.
- Long published create/submit/close churn with bounded retained bytes and QID
  reuse; create-only tests alone do not prove this.
- Cross-client canaries under overlapping TA/3D/CL work, queue churn and recovery;
  no stale writes, owner confusion, leaked state or double fence settlement.
- Mixed-load CTS/r20, throughput and tail-latency comparisons, with ring pressure,
  mutex hold times and memory usage captured. No default serialization or reduced
  batching as a substitute for the root-cause fix.

Until those prerequisites are met, published resources remain retained. That
avoids speculative early reclamation but leaves finite-session exhaustion and
its potential denial-of-service impact open; the architecture is not yet
qualified for unlimited queue churn or sustained heavy load.
