# T8140 GFX AXI2AF transition tests

Run from the kernel checkout with Python 3 and Clang installed:

```sh
python3 drivers/pmdomain/apple/tests/test_t8140_axi2af.py
```

This host-only test extracts the actual provider C functions and compiles them
against deterministic OF, regmap, and MMIO stubs. It never accesses hardware.
It checks all 1024 TARGET/DEV_DISABLE transition combinations against an
independent transcription of Apple's branch structure, plus unrelated bits,
full/masked writes, ON/OFF/re-ON order, inherited-active restoration without
duplicate writes, mapping failures, unchanged requests, and non-GFX behavior.
Failed power-on, BUSY timeout, and set/clear readback tests ensure the next
unsafe operation is not performed. DRM's late validation must remain read-only.

## Protocol evidence

AppleT8140PMGR in macOS 25G83, UUID
`4C94969B-D110-3CF8-A9B5-26BC3C5B7C32`, provides the complete protocol:

- `_applyAxi2afWorkaround` at `fffffe000983c6ec` gives post-ON priority when
  TARGET changes to `0xf` or DEV_DISABLE changes from 1 to 0.
- Otherwise, TARGET changing to 0 or DEV_DISABLE changing from 0 to 1 clears
  AXI2AF bit0 at `0x481000104`, then `0x481000108`, before the PMGR write.
- `writeReg32` at `fffffe000983cb98` waits for ACTUAL=`0xf`, then BUSY=0,
  before the post-ON set of those same two bits.
- Table row 7 names Main PMGR offset `0x1f0` and AXI base `0x481000000`.
- `_restoreAxi2afWorkaround` explicitly restores inherited-active domains.
  `_quiesceAxi2afWorkaround` only asserts that non-exempt domains, including
  GFX, are off at Awake exit; it does not establish an always-on policy.

The pre-OFF half is essential. An ON-only J700 test reached ACTIVE but the
next early OFF left PMGR permanently BUSY (`0x0f002b00`). The complete protocol
was tested on J700 boot `94ed3f11`: both early ON/OFF cycles and the later DRM
ON succeeded, and both DRM validation reads had bit0 set. Fragment rendering
still timed out, so this is a power-sequencing correction, not a rendering fix.

Live GFX does not use `apple,force-disable`. The generic force-disable/OFF
combination subsequently clears DEV_DISABLE while requesting OFF, which the
native precedence treats as requiring an ACTIVE wait. The test deliberately
verifies a safe failure for this unsupported combination; it does not invent
a policy exception. Linux's existing bounded poll budget remains unchanged.
