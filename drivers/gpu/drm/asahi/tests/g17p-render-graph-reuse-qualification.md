# G17P render graph reuse qualification

Measured on Neo, 2026-09-16, kernel `7.1.6-neo-g17p2`.

The complete render graph remains cached after queue destruction and context
supersession. Reuse retains its queue records, descriptors, support, PM/TVB
backing and reserved HardwareBufferID. The incoming VM receives client aliases
and a new command context. Foreign reuse requires the recorded context epoch,
queue release, and an unoccupied destination alias set. Returning owners take
precedence over reusable foreign graphs.

The destination check is necessary even when the graph's old owner has retired.
The r8/r9 captures show an incoming desktop VM already mapping
`0x7040040000..0x7040058000`, which a cached graph needed for render state. Another
capture shows an occupied diagnostic descriptor alias at `0x704003c000`.
Rejecting those candidates preserves the existing mappings and allows another
cached graph or normal allocation to be selected.

## Process-lifetime stress module

- Module SHA-256: `b457bb99ca641a8b12574934129d59e50aab9b241d4f31b2f93f80d0e0d1cca1`
- Mesa candidate: `k4-blend-mrt3-r2`
- Mesa SHA-256: `daffdeba4c6e63e33e24a67ee385a9026c11f6506c82b8dab297cdcc0bfca028`
- Test: `dEQP-VK.query_pool.occlusion_query.basic_conservative`, each iteration in
  a fresh process, shader cache disabled, Vulkan version override 1.0.
- Desktop present: 300/300 iterations passed.
- Headless, same firmware session after stopping the display manager: 300/300
  iterations passed.
- Through completion of these two gates: 593 graph rebinds, 11 PM generations,
  no recorded submission failures, firmware recovery requests, or session-crash
  markers. This does not describe the remainder of the boot.

Neo evidence directories:

- `/opt/neo-gpu/results/drm-graph-rebind-r10-desktop300`
- `/opt/neo-gpu/results/drm-graph-rebind-r10-headless300`

Both contain per-process QPA results, module/driver hashes, boot ID and kernel
log window. This is process-lifetime stress evidence, not Vulkan 1.4 mustpass
qualification or a throughput benchmark.

## Subsequent failure and integration status

The same module subsequently failed during the native-API batch
`blend-unresolved69-graph-r10`: nine cases passed and one was recorded as a
crash, with 59 cases not executed. The active case was
`dEQP-VK.pipeline.pipeline_library.blend.dual_source.multi_attachments.r32g32b32a32_sfloat`.
At 20:05:29 UTC firmware requested recovery with stamp 1 and data master 1.
Chromium and Hyprland then crashed at 20:05:31 UTC. The user also reported a
long-running workload crashing around this time; the captured evidence does
not establish which application triggered the recovery or its root cause.

The graph-reuse change is now integrated on `5ad3e3e960acc`, which includes
newer completion-generation, mapping-invalidation, and compute-queue changes.
The stress results above predate that integration and must not be represented
as hardware qualification of the combined tree.

The installed stress module had been built from a stale `origin/drm-agx3`:
the repository's fetch refspec tracked only `asahi`, so fetching `drm-agx3`
updated `FETCH_HEAD` but not the tracking ref. The tracking ref and fetch
configuration have been corrected. The omitted range includes the existing
Buoy compute page-list fix, `198876055a1da`.

An independent failure occurred on the following boot at 20:09:37 UTC while
CTS was stopped: firmware reported compute data master 2, stamp 7, and a stable
halted GMMU snapshot at VA `0x4c0`, VM slot 10. Recovery failed and the later
isolated CTS attempt could not enumerate a GPU; that attempt is not a test
result. The missing fix is established by source ancestry; these logs alone
do not prove that both incidents have the same fault mechanism.

With the `5ad3e3e960acc` base restored, the previously crashing native-API CTS
case passed in `blend-r11-isolated-crash`, with zero recorded kernel faults
and Hyprland alive. Module SHA-256 was
`4ce5f07238ecd4b64b21157f5dfcd6587545fbc0feb5202db9d2972be870b2af`.

## Complete integration audit

The final candidate additionally includes J700's render timestamp-epoch fix
`378b14ac9d899` and same-VM sibling-queue graph reuse `0259778d72951`.
The merge keeps DRM's completion-envelope timestamp publication rather than
reintroducing the obsolete caller timestamp aliases. The layer-metadata
address helper accepts the same 64-bit ordinal as the other render helpers.
Cache selection still prefers the VM's graph before eligible foreign graphs;
sibling queues retarget the cached context without remapping the same VM.

- Live remote base checked: `5ad3e3e960acccf273ebe54f78d52b4b116cdb28`.
- All six commits in `1df1ac0f9ea1..198876055a1d` are ancestors.
- All 79 runtime/build source files match the archive used for compilation.
- Module SHA-256: `c3080a3c4674bcf085927037d0869b613dd88962c3f0ca0c88a7b9a4d563f4cf`.
- Resident build ID: `468f568abb3215f4865aa9006c92f9de8e66c347`.
- Boot ID: `fc3dd315-4cf1-4509-a27b-9bb3b84cacb1`.
- Seventeen focused host scripts pass, including actual Buoy page-list
  encoders, timestamp-epoch codecs, sibling-queue selection and rebind faults.

On boot `c8748fb4-2506-41e5-ad7e-1289f9343c29`, the same final module passed
300/300 fresh-process occlusion-query iterations with the desktop running.
The gate recorded 294 complete-graph rebinds and no matching kernel-fault or
Hyprland-exit condition. Evidence:
`/opt/neo-gpu/results/drm-graph-r12-integrated-desktop300`.
This gate still uses the explicitly recorded Vulkan 1.0 override; it is a
graph-lifetime regression test, not Vulkan 1.4 qualification.

### Full-size Chromium Buoy remains failing

`buoy-20260916T202456Z` ran the original hashed shader/texture assets at
1180x664 in Chromium/ANGLE with the system Mesa library SHA-256
`0c52b044d54e9a7e1dc9e154bc160437e535f41ebbe3392dcfdbddd02d580e25`.
It failed at 20:25:00 UTC: firmware requested stamp-3 recovery, data master 1,
and the page reported WebGL context loss after 20 frames. No reduced resolution,
forced finish/readback, or compositor pause was used. This is not a successful
soak. Comparison against unmodified `5ad3e3e960acc` is required before attributing
the failure to the new CTS graph-reuse changes.

The same browser/assets/Mesa/resolution also failed on unmodified
`5ad3e3e960acc` in `buoy-20260916T202904Z`, after 10 page-reported frames.
The baseline module SHA-256 is
`cd85f62861eab1b6e5b58278b06beb60c54f038c6ba12254fef86171185e8701`, resident
build ID `120c657a85a48f3f2fecce5ad062769f0f71a0de`. Firmware reported two
sources and data master 0 at 20:29:08 UTC. Both trees reproduce the symptom;
their different fault reports do not prove an identical underlying mechanism.

The archived, previously qualified `198876055a1da` module was also checked,
using its original SHA-256
`9d58b9f355c0de76736ee06abc2ab789822b03c8fa6e292e8c2773c0776e62e4` and
resident build ID `0324fc674d17b41b08c0aba14b3d5a90753f78cf`:

- Full-size offline Chromium wrapper: recovery after 14 frames
  (`buoy-20260916T203507Z`). It therefore supplies no passing baseline for
  bisecting that particular wrapper/workload.
- Original 128x128 fixed-time, immutable-command-buffer compute test with
  validation, launch tracing and the display manager stopped: 60 seconds,
  3,541 confirmed fenced submissions, no detected fault
  (`buoy-20260916T203843Z`).
- Original reduced fragment test under the same isolation: 60 seconds,
  3,601 confirmed fenced submissions, no detected fault
  (`buoy-20260916T204151Z`).

These reproduce the earlier targeted compute/fragment success. They do not
qualify full-size Chromium, and do not disprove the user's reported regression
in the actual Shadertoy page. Both targeted tests must also be checked on the
integrated candidate.

Both targeted tests subsequently passed on the integrated candidate with the
desktop left running: compute completed 3,541 confirmed fenced submissions in
60 seconds, and fragment completed 3,601 in 60 seconds. Neither run detected a
kernel fault. The resident build ID remained
`468f568abb3215f4865aa9006c92f9de8e66c347`; system Mesa remained `0c52b044...`.
Evidence directories are under the local incident capture's
`r12-compute-desktop` and `r12-fragment-desktop`. This preserves the prior
targeted Buoy fix; the full browser scene is a separate outstanding check.

### Actual Shadertoy page: contaminated observation

The actual `https://www.shadertoy.com/view/XdsGDB` page was then observed in an
isolated Chromium profile at its normal 840x473 canvas size. Its last captured
state showed continued animation and 60.0 FPS before a tiling-stage recovery
at 20:49:15 UTC. The user identified concurrent experimental ray-tracing
shaders from another agent. This run is excluded from Buoy regression
attribution and is not a successful soak. The user requested a reboot and
return to CTS after this discovery.

## Host regressions

Seventeen focused scripts passed on the integrated source. Three compile the
actual driver functions or closure into Rust tests: cache selection has eight cases, alias availability
has five, and rebind fault injection has five. They cover returning-owner
precedence, physical-pair isolation, both retirement prerequisites, destination
overlaps, page alignment and overflow, and graph/root retention at every
fallible rebind stage. Existing constructor, root, SKSM, PM and HardwareBufferID
checks also passed.

Partial PM/support reinitialization and the experimental blocking graph wait
are absent. Quarantined PM/support objects retain their IDs until processor
shutdown and cannot be used as constructor input.
