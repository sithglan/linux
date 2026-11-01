# G17P first-submit SKSM fixtures

These 0x200-byte fixtures were admitted from
`neo-bringup/research/g17p-submit-contract/fixtures/first-submit/`.
`mine_contract.py` generates them independently of the Rust encoder from the
KEXT-proven G17P slot packing and the experiment-proven output-positive add3
operands recorded in `encoder-parity.md`.

- `sksm-entry-before.bin`: all-zero selected runtime stride;
  SHA-256 `076a27c79e5ace2a3d47f9dd2e83e4ff6ea8872b3c2218f66c92b89b55f36560`.
- `sksm-entry-after.bin`: populated first-submit stride;
  SHA-256 `ba104e14eb777736df4cec2716c5840b36241e4af2c3617e00650366ec391ca7`.
- `sksm-entry-second-after.bin`: populated second-submit stride for native
  state `current=2`, `previous_valid=1`, `previous_timestamp=1`; SHA-256
  `d888b2d41ebd53794bc7782fcf9baa910e567969148244d61ce682fbda873f47`.
- `sksm-entry-relocation-mask.bin`: one bits mark address bits normalized by
  fixture comparisons; SHA-256
  `c87e4b5e32d986a29f7901989993d3fbeada763e1e7a741f969f58aa3e926d82`.
- `channel-event-primary.bin`: KEXT-written 0x15-byte conditional tag-14
  fallback prefix, used only when the direct feature is disabled;
  SHA-256 `01c5248a31ed8a479b049417fa03abfcd56a975770d245baaa491fffd1266aa3`.
- `channel-event-optional.bin`: conditional 0x12-byte tag-16 prefix, used only
  when the host descriptor carries its optional event pointer;
  SHA-256 `8667778da9e2d0c7c1f82fa249f2ecf14f1c911d006f4f9830e6a3018ffe48f2`.
- `ep21-first-activation.bin`: first/changed-channel activation
  `0x008300000000000a`; SHA-256
  `b43479c5862a12a33c95b5ce7f364912b1f859f685dff861b501ede6b23c7b44`.
- `ep21-direct-kick.bin`: enabled direct-path `kickFirmware(4, 3, false)`
  `0x0083000000000010`; SHA-256
  `62bcb506c3491e7cb7433d93fa590f9cfbaf992427075149e5bbbab17d17b4d5`.
- `ep21-first-direct-sequence.bin`: activation followed by the distinct direct
  kick; SHA-256
  `d3eb514a7f214b07e955a3330e340937d9f91db8383ecd18315c0a5674248fd8`.
- `tag15-config-update-first.bin`: first-bind tag-15 ConfigUpdate meaningful
  extent, independently matching the existing selector-0x0f builder; SHA-256
  `8a1a6b62ee712abe235860077e29d08f232f8c3c2432f327e82b04800706e204`.
- `tag15-config-update-relocation-mask.bin`: address normalization mask for
  the tag-15 extent; SHA-256
  `e453d785542953408e8fc17657d91b536c60475fd472ee72e2f2dc2b439755d5`.
- `channel-state-before-tag15.bin`: producer 1 with descriptor slot zero
  already present; SHA-256
  `e3ee359d1eab12c393dc53d908ea5546f8637d13811afc8627971694d9c2ed07`.
- `channel-state-after-tag15.bin`: pointer store, barrier, producer 1 -> 2;
  SHA-256 `2c4add86dfcda9191c99dc096a482e4672719d69231f044e44f7f1af2d5fb359`.
- `channel-state-after-tag16.bin`: pointer store, barrier, producer 2 -> 3;
  SHA-256 `9c09a40589df6e45ac9126a3cdc4cca25c9e88f77749efa50717b7bfe3e569da`.

Direct-feature G17P skips tag 14. A first bind marks the queue configuration
dirty, which makes descriptor preparation allocate tag 15 and then tag 16;
submitBuffer publishes tag 15 before the SKSM entry and tag 16 after it. The
tag-16 fixture locks that conditional encoder while the admitted tag-15 bytes
and full direct-path predicate/order remain in the contract corpus.

The retained channel pointer ring is `0x500` qwords at graph `+0x4000`, ending
at `+0x6800`. Tag 15 and tag 16 storage is placed after it at aligned offsets
`+0x6800` and `+0x6c00`; neither object overlaps a repeat-submit ring slot.

Tag 15 `+0x08/+0x10` are the low and high GPU aliases of the same retained
`0x20000` SKSM entry backing (native KSM queue `+0x80/+0x88`). They are not
aliases of the classic CL2 queue-context record. The relocation mask covers
both complete qwords independently while their backing offset remains equal.

T8140 G17P leaves the dedicated `AGXSKSMFlushID` feature bit clear. The first
host descriptor therefore uses logical fallback ID `0`; that value is absent
from tag 15 and needs no completion release. The taken descriptor-correlation
branch is host-only (`IOMallocData`, no GPU alias); Linux represents its
nonzero sequence through retained queue/job lifecycle state and publishes no
extra firmware object.

Evidence classification: entry packing and runtime geometry are
`KEXT_PROVEN`; first/second add3 operands are `EXPERIMENT_PROVEN`. Native queue
initialization sets `current_timestamp`, `last_add_kicks_timestamp`, and
`base_timestamp` to `1`.
