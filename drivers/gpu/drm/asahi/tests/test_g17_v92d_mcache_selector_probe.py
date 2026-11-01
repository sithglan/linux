#!/usr/bin/env python3
"""v92d: fragment MCache selector diagnostic (bit23) and the aux-fb small-primitive cfg.

What the selector is (native kext 25G83, AGXG17P):
  * AGXTACommandDescriptor::prepare stores AGXContextIDManager::alloc(gart, ..,
    select 0) -- the command's GART context ID -- at descriptor +0x1274
    (0xfffffe000890061c/0x...620);
  * AGXTAChannelSKSM::submitBuffer passes that word as the int argument of the
    virtual mcacheApertureBufferSetup (vtable +0xfe0, call at 0x...88fdc74..ca8);
  * mcacheApertureBufferSetup forwards it unchanged as w3 of
    halMCacheApertureSetup (vtable +0xfb0), which packs q0 = start |
    selector << 41 | hwsid << 47 | 1 << 55 (0xfffffe00088c7018).
So the selector names an address-space context. The SLC dataset is the
separate hwsid field, bits 47..54. Linux writes its own queue's app-GART
context ID (g17_resources.rs encode_g17p_fragment_mcache, selector 64 ->
render_root_slot = execution_context.context_id()). Native's 1 is its own
process context. Bit23 forces 1 anyway, as a "does the aperture match
matter" arm; it is not parity.

Checks: the verbatim override helper and mapping encoder, compiled and run,
change only bits 41..46 of qword 0; the default path is untouched unless bit23
is set; the lab knob values leave bit23 clear; the build uses the cfg.
"""
import os, re, subprocess, tempfile
from pathlib import Path

ROOT = Path(os.environ.get('DRIVER_ROOT', Path(__file__).resolve().parents[1]))
RES = (ROOT / 'g17_resources.rs').read_text()
REN = (ROOT / 'g17_render.rs').read_text()
TB = (ROOT / 'g17_timing_buffer.rs').read_text()
UAPI = (ROOT / 'g17_uapi.rs').read_text()


def item(text, needle):
    start = text.index(needle)
    depth, i = 1, text.index('{', start) + 1
    while depth:
        depth += (text[i] == '{') - (text[i] == '}')
        i += 1
    return text[start:i]


def code(text):
    return '\n'.join(line.split('//', 1)[0] for line in text.splitlines())


bit = int(re.search(r'DIAG_MCACHE_SELECTOR_NATIVE: u32 = 1 << (\d+);', TB).group(1))
assert bit == 23
for knobs in (0x9000, 0x4009000):
    assert not (knobs >> bit) & 1, 'default and lab knob values leave the probe off'
apply = code(item(RES, 'fn apply_g17p_fragment_mcache('))
assert apply.index('encode_g17p_fragment_mcache(') < apply.index('DIAG_MCACHE_SELECTOR_NATIVE')
assert re.search(r'if crate::g17_timing_buffer::diag_knob\(crate::g17_timing_buffer::DIAG_MCACHE_SELECTOR_NATIVE\) \{\s*'
                 r'g17p_mcache_selector_override\(', apply), 'override only behind bit23'
enc = code(item(RES, 'fn encode_g17p_fragment_mcache('))
assert 'render_root_slot' in enc and '64 =>' in enc, 'default selector = own GART context'
# aux-fb small-primitive: uapi accepts flag bit31 and the encoder sets bit0 only under the cfg
assert 'if cfg!(g17p_aux_fb_small_probe) { 1 << 31 } else { 0 }' in UAPI
axfb = code(item(RES, 'fn g17p_aux_fb_flags('))
assert "cfg!(g17p_aux_fb_small_probe) && command_flags & (1 << 31) != 0" in axfb
build = ROOT.parent / 'BUILD-COMMAND'
if build.exists():
    assert '--cfg g17p_aux_fb_small_probe' in build.read_text(), 'v92d is built with the cfg'

override = item(RES, 'fn g17p_mcache_selector_override(')
mapping = item(REN, 'pub(crate) fn encode_g17p_mcache_mapping(')
struct = item(REN, 'pub(crate) struct G17PMcacheMapping')
consts = '\n'.join(re.findall(r'const G17P_MCACHE_[A-Z_]+: [a-z0-9]+ = [^;]+;', RES))
rust = r'''#![allow(dead_code, unused_macros)]
type Result<T = ()> = core::result::Result<T, i32>;
const EINVAL: i32 = 22; const EOVERFLOW: i32 = 75; const EIO: i32 = 5;
macro_rules! pr_err { ($($t:tt)*) => { eprintln!($($t)*) } }
macro_rules! pr_info { ($($t:tt)*) => { eprintln!($($t)*) } }
const G17P_FRAGMENT_MCACHE_MAX_ENTRIES: usize = 17;
mod g17_submission {
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub(crate) struct G17PClMcacheAperture { pub(crate) address: u64, pub(crate) index: u8, pub(crate) count: u8 }
}
mod g17_render {
    pub(crate) const G17P_MCACHE_MAPPING_SIZE: usize = 0x10;
    #[derive(Debug)] pub(crate) enum RenderBuildError { McacheAddressOutOfRange, McacheRangeEmpty, McacheSelectorOutOfRange, ArithmeticOverflow }
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    %s
    %s
}
%s
%s
#[test] fn only_selector_bits_change() {
    let mut frag = vec![0u8; 0x2240];
    let base = 0x6a0usize;
    let ranges = [(0x1000_0000_00u64, 0x80u32, 2u16), (0x1004_0000_00, 0x8000, 1), (0x1004_4000_00, 0x10, 1)];
    for (i, (a, n, s)) in ranges.iter().enumerate() {
        let m = g17_render::encode_g17p_mcache_mapping(g17_render::G17PMcacheMapping {
            address: *a, size_units: *n, hwsid: 24, selector: 13, range_shift: *s }).unwrap();
        frag[base + 16 * i..base + 16 * i + 16].copy_from_slice(&m);
    }
    let before = frag.clone();
    let ap = g17_submission::G17PClMcacheAperture { address: 0x7080_0000_0000 + base as u64, index: 0, count: 3 };
    g17p_mcache_selector_override(&mut frag, 0x7080_0000_0000, ap, 1).unwrap();
    for i in 0..3 {
        let q = |b: &Vec<u8>, at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        let (a0, b0) = (q(&frag, base + 16 * i), q(&before, base + 16 * i));
        assert_eq!((a0 ^ b0) & !(0x3fu64 << 41), 0);
        assert_eq!((a0 >> 41) & 0x3f, 1);
        assert_eq!((b0 >> 41) & 0x3f, 13);
        assert_eq!(q(&frag, base + 16 * i + 8), q(&before, base + 16 * i + 8));
    }
    // Bytes outside the table are untouched.
    assert_eq!(&frag[..base], &before[..base]);
    assert_eq!(&frag[base + 48..], &before[base + 48..]);
    // Out-of-range selector and bad geometry are refused without writing.
    let snap = frag.clone();
    assert!(g17p_mcache_selector_override(&mut frag, 0x7080_0000_0000, ap, 0x40).is_err());
    let bad = g17_submission::G17PClMcacheAperture { address: 0x7080_0000_0000 + 0x2238, index: 0, count: 3 };
    assert!(g17p_mcache_selector_override(&mut frag, 0x7080_0000_0000, bad, 1).is_err());
    assert_eq!(frag, snap);
}
''' % (struct, mapping, consts, override)
with tempfile.TemporaryDirectory() as tmp:
    src = Path(tmp) / 't.rs'
    src.write_text(rust)
    out = subprocess.run([os.environ.get('RUSTC', 'rustc'), '--edition=2021', '--test', str(src), '-o',
                          str(Path(tmp) / 't')], capture_output=True, text=True)
    assert out.returncode == 0, out.stderr[-3000:]
    run = subprocess.run([str(Path(tmp) / 't')], capture_output=True, text=True)
    assert run.returncode == 0, run.stdout + run.stderr
print('v92d MCache selector probe: all checks passed')
