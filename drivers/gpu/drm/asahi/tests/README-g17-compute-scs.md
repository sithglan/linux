# G17P compute SCS request integration

Integrates the default-off request encoder proposed in `drm-rt-scs2` commits
517eb344c478c, 90554a73a356f and e5e25eb4d8c46, on drm-agx3 3e0a5ee07e0b1.
It does not advertise ray tracing or establish that scratch backing works.
The enabled request still needs a controlled hardware experiment paired with
the compute UMA pool. Scheduling, allocation and queue ownership are unchanged.

The three words follow the proposal's Apple register-builder analysis:
0x101d1 combines pages and MiB, 0xd479 carries extras, and 0x107a1 carries
the window/enable word. A zero MiB field derives ceil(pages / 256).

Integration corrections:

- Pages zero ignores every override and preserves all three historical words.
- Reject page/MiB fields above 65535 and windows above 255 before publication.
  These are non-overlap bounds for the encoding, not claimed hardware capacity.
- Use wide arithmetic for the MiB derivation.
- Compile the complete real compute module in the host test, with parameter
  defaults read from asahi.rs. All 15 tests pass, including the unchanged
  historical 40-register hash and every representable page-count boundary.
- The full-module test exposed a pre-existing channel-graph golden mismatch.
  Unchanged 3e0a5ee07e0b1 fails identically. Comparing graph bytes with
  fcc1b7dfc8847's parent isolates exactly the FlushID index at optional+0x46
  (old 1, current 0). Assert the current field, then normalize only that field
  before checking the original golden; do not replace the whole hash.
- The retained-FList fixture also mixed a widened u64 submission ordinal with
  a u32 channel producer. Convert the fixture's bounded producer explicitly;
  no runtime counter is narrowed. Its five lifecycle/wire tests now pass.

Build artifact `scs-request-r1/asahi.ko`:

- vermagic: `7.1.6-neo-g17p2 SMP preempt mod_unload aarch64`
- SHA256: `f89f12475eba4288f4325f7f2d9c42db8bb63f69e1da3ccbff6914cccbd43df4`
- build ID: `4661a16c72f8808a91db14c92e5e3b7a4efc90ff`
- all 321 undefined imports exactly match the qualified r12 module

The later retained-FList change is inside the test module only. The deployed
artifact contains the same runtime source. Hardware results must identify this
artifact and boot; successful default-off runs are not enabled-SCS validation.

Hardware: `scs-default-query300-r1` passes all 300 fresh-process native CTS
occlusion queries on boot 48604e79-00ca-4c86-8a37-5b779b4df0d2, no fault
markers and desktop alive. The full-resolution immutable Buoy fragment probe
is NOT clean: it records `render-submission-lost` at worker termination after
60 seconds. Repeating the same Mesa/probe on the unchanged qualified kernel
(c3080a3c4674..., boot 908cd793-c68a-42f7-80c8-06f1f6f7e628) reproduces the
same shutdown loss. This is an open cancellation/lifetime investigation,
not a passing Buoy qualification and not established as an SCS regression.
The compute Buoy shader is refused before execution by current Mesa at a
uniform-to-high-GPR copy; that run provides no kernel stability evidence.
