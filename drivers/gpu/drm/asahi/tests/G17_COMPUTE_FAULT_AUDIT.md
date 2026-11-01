# G17P concurrent compute fault: ownership audit

Historical investigation. For current code status and superseded findings, see
[the architecture and load review](G17_ARCHITECTURE_LOAD_REVIEW.md).

2026-09-19. Scope: the Rust Asahi driver and its tests. Linux HEAD at initial
inspection: `9fc9aa2fe0373a95e6d2f0224655917ad77de8a7`, with substantial existing
uncommitted work preserved. No Mesa or DCP changes were made by this audit.

## Hardware evidence and current limit

The supplied minimal reproducer runs three concurrent copies of
`dEQP-VK.subgroups.ballot_broadcast.framebuffer.subgroupbroadcast_i16vec4tess_control`.
The established failure is firmware reason 2 with active compute TSQs one
command behind, consumed outer records, and no reported GMMU fault. Serial
success and the passing four-process shared-memory control do not establish
correct spill allocator ownership under concurrency.

The first patch corrects the recovery observation. The old
`snapshot_compute_uma()` always returned UMA slots 0/1 and the bootstrap
control object, even when logging another queue. The new reader selects the
queue's assigned HardwareBufferID and reads that queue's `shared_support`
FList. It is bounded and read-only; it adds no submission-path work.

The coordinator's clean-boot capture with that patch is
`diagnostics/neo-dmesg-perqid-uma-3way-fail.log` at the project root. Its module
SHA256 is `a38625d70a3c27d5f2965b2304102621ac08ea9b1f7403ce01d930cd636bd9fd`.
The failure persists. At recovery:

| Queue | FList +0x08 / UMA slot | Tag15 +0x46 | Page-list VA | UMA counter A / B | Total pages |
|---|---:|---:|---|---:|---:|
| QID4 | 1 | 0 | `0x7040000000` | 11664 / 17040 | 5376 |
| QID5 | 2 | 1 | `0x7041a0c000` | 0 / 5376 | 5376 |

The counters are decoded with the inspected `encode_g17p_umadesc` packing;
their names do not assert hardware allocation semantics. Slot 2 matches its
initial counter values, whereas slot 1 changed. This is evidence to test an
ownership mismatch, not proof that any particular hardware register selects
the wrong pool.

The logged `usage` words come from the buffer named by register `0x14070`.
They are **not** a readback of the separate pointer at descriptor `+0xfcb`.
Do not substitute those words when interpreting the firmware consumer of
`+0xfcb + 8` described in `RT_HANDOFF.md`.

## Rejected owner-equality experiment

`prepare_g17p_compute_queue` assigns each private FList the same numeric ID as
the newly allocated QoS owner. `apply_g17p_cl_retained_static_state` writes it
at FList `+0x08`; `stage_compute_queue_uma_descriptor` uses it as the UMA
table slot. Previously the constructor subtracted one before publishing the
FList ID through ConfigUpdate `+0x46`.

The experiment removed that subtraction. This is not a rule that a queue's QoS
ID must always equal its FList ID. The existing render encoder distinguishes
`usc_freelist_hardware_buffer_id` at `+0x46` from QoS identity at `+0x56`, and
the native contract in `RT_HANDOFF.md` identifies `+0x46` as the command's
FList ID. Native queues sharing FList ID zero are compatible with that rule;
they do not justify rebasing the IDs of independent private FLists.

The first private page list already occupies `0x7040000000`. With the proposed
nonzero ID, mapping the fixed aperture again would reserve the same VA twice.
The experimental `g17p_compute_flist_alias_va` omitted that alias, retaining the
established fixed addresses: index zero selects `0x7000000000`, every nonzero
index selects `0x7040000000`.

The coordinator captured another failure in
`diagnostics/neo-dmesg-flist-owner-single-fail.log`: QIDs 4/5/6 were active,
UMA slots 2/3 still held their initial counters, and the actual descriptor
`+0xfcb` records were zero. The file name alone does not establish a serial
test sequence. The coordinator removed the ID change and its alias helper.
The diagnostic correction remains. Equality of the ID fields was a tested
hypothesis, not a confirmed firmware requirement; the host regression's
passing result did not prove the hardware contract. The original concurrent
fault remains unresolved.

## Adjacent findings, ranked

1. **High: nontransactional retained-queue rebind.**
   The former `prepare_async_compute_queue` dropped `q.binding` and `q.context`
   before the fallible `bind_independent_compute_user_vm` call, even though
   preparing aliases does not switch the installed firmware queue. The repair
   retains one previous binding/root bundle per queue, prepares new aliases
   before moving ownership, and reuses the existing binding for the same VM.
   The first replacement publication records its submission ID and recovery
   epoch. Only the existing exact-descriptor/fresh-timestamp/final-stamp
   completion path, in that epoch with recovery inactive, releases the old
   bundle. Unpublished close restores the old binding; ambiguous publication,
   zero-duration completion and recovery mismatch retain both for quarantine
   and proven processor-stop cleanup. Production-method host tests cover
   prepare/publication failure, rollback, completion identity and independent
   progress by another queue. Hardware validation of this repair is pending;
   it is not an explanation for the earlier clean-boot no-rebind fault.

2. **High: multiple private FLists conflict in one VM.**
   `cache_compute_vm_mappings` reserves the same fixed nonzero aperture for
   every distinct private FList. `mmu::Vm::map_at` uses `reserve_node`, so an
   existing mapping at that VA makes the next reservation fail. Omitting a
   second alias of the *same* canonical object would not fix the general
   multiple-owner collision. A VM-shared FList owner,
   or a verified alternative ABI mapping contract, is needed; inventing
   per-index apertures is not valid. The failing captured processes have
   distinct VMs, so this is separate from their initiating fault.

3. **Medium: two unused MiB allocations per physical compute queue.**
   `G17PClRuntimeStorage::new` allocates 23 one-MiB `flist_buffers` and
   `cache_compute_vm_mappings` maps all 23. Both queue FList encoders use
   `COMPUTE_READINESS_OPERAND_ENTRY_COUNT`, currently 21, and no other code
   references the final two queue-private buffers. This costs 2 MiB and two
   client mappings per retained graph, in addition to their kernel mappings.
   This is a static resource count, not a measured frame-time cost. The
   separate bootstrap `compute_operand_buffers` really uses index 22 and
   must not be reduced on the same reasoning. Leave allocation/layout changes
   separate from attempts to establish the initiating fault.

4. **Medium: retirement regression harness had stopped compiling.**
   `test_g17_async_retirement.py` lacked production's `ENODATA`, `ECANCELED`,
   and `retired_by_teardown`. The mock definitions are now synchronized;
   all 15 retirement tests pass. No runtime behavior was changed for this.

5. **Low: stale explanatory comments obscure current defaults.**
   `encode_compute_config_update` still calls the real-context behavior
   “default-off”, although `g17_config::COMPUTE_CONFIG_CONTEXT` is 1.
   `g17_job.rs`'s module introduction still describes a shared QID0/1/4
   runtime as inadequate for the ownership contract implemented by the
   asynchronous path. These comments should be condensed to current
   invariants, with historical experiments kept in documents. No default
   change is justified merely by a stale comment. In particular, retain the
   current real-context selection and native command order, and do not revive
   failed B2-global or usage-ring-rotation experiments without new evidence.

## Validation and hardware plan

The following host suites passed during this audit. The four compute and
retirement suites were rerun after the coordinator removed the owner-equality
experiment and adjusted its test; the other three suites are unchanged:

| Script in this directory | Rust tests |
|---|---:|
| `test_g17_compute_flist_relocation.py` | 19 |
| `test_g17_compute_flist_snapshot.py` | 3 |
| `test_g17_compute_timeout_snapshot.py` | 5 |
| `test_g17_async_retirement.py` | 15 |
| `test_g17_async_release_retains_firmware_state.py` | 4 |
| `test_g17_render_hardware_buffer_id_ownership.py` | 6 |
| `test_g17_workload_va_arenas.py` | 5 |

The experimental ownership regression compiled the production constructor's ID expression,
the static FList encoder, and the ConfigUpdate encoder. It checks first and
retained submissions, including a distinct QoS ID. Reinstating `hbid - 1` in
the test's in-memory source failed that ownership assertion (0 versus 1).
That assertion tested the proposed model, which the hardware result did not
validate; it is not retained as a production requirement. These tests verify
host encoding and bounds, not firmware semantics.

The diagnostic-only module completed an isolated kernel module rebuild with
the existing Rust 1.85 toolchain and vermagic `7.1.6-neo-unified`. Its build
reported 69 warnings. The coordinating task owns candidate builds and all
hardware deployment, so there is only one writer of the build outputs.

For the next evidence-backed candidate:

1. Record the built module hash and boot identity, and verify that first bind
   succeeds before interpreting a concurrent result.
2. Run the exact three-process single-case repro on a clean boot. Capture
   each active QID's ConfigUpdate, FList address/ID, selected UMA slot, TSQs,
   app-context roots, and fault-requestor state. Preserve the distinct ID
   fields rather than assuming their equality.
3. If it fails, preserve the capture and reject a claim of causal resolution.
   Determine whether the selected UMA counters changed and capture the actual
   `+0xfcb` record before inferring allocator pressure from the `0x14070` data.
4. If it passes, repeat the clean-boot repro, run two long-lived concurrent
   processes beyond the previously observed approximately 60-second failure,
   then exercise four processes, serial controls, and the shared-memory
   control. Broaden CTS and measure throughput before calling the fault solved.
5. Test multiple compute queues sharing one VM separately; its known alias
   conflict must not be mistaken for recurrence of the distinct-VM stall.
