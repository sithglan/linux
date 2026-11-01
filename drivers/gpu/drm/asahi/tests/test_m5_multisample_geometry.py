#!/usr/bin/env python3
"""Verify the M5 multisample geometry derivation against the captured witness.

Splices the REAL geometry() and set_samples() out of g17_render.rs and compiles
them standalone, in the idiom of tests/test_g17_render_work_key.py -- so this
checks the driver's own code, not a reimplementation of it.

The expected values are measured, not derived: logs/m5-msaa-native-hv-
20260915-101837 holds a 256x256 colour render at one, two and four samples
with a same-mode control in a second oracle process for the noise floor.
"""
from pathlib import Path
import re, subprocess, tempfile

src = (Path(__file__).resolve().parent.parent / 'g17_render.rs').read_text()

def slice_fn(text, header):
    start = text.index(header)
    i = text.index("{", start)
    depth, j = 0, i
    while True:
        if text[j] == "{": depth += 1
        elif text[j] == "}":
            depth -= 1
            if depth == 0: return text[start:j+1]
        j += 1

geometry    = slice_fn(src, "fn geometry(")
set_samples = slice_fn(src, "pub(crate) fn set_samples(")
align       = slice_fn(src, "const fn align(")

for required in ["parameters.samples == 4", "utiles_per_tile_y * 2",
                 "0xeaa2_6e26", "0x4_0000"]:
    assert required in geometry + set_samples, required

consts = "\n".join(re.findall(r"^const (?:TILE_WIDTH|TILE_HEIGHT|MACRO_TILES_X|MACRO_TILES_Y|REGION_ENTRY_SIZE).*$",
                              src, re.M))

source = f"""
#![allow(dead_code, unused_parens)]
{consts}
{align}

#[derive(Debug, Copy, Clone, Default)]
pub struct G17pRenderParameters {{
    pub width: u32, pub height: u32, pub layers: u16, pub samples: u8,
    pub utile_width: u8, pub utile_height: u8,
    pub multisample_control: u64, pub utile_config: u64, pub tib_blocks: u64,
}}
#[derive(Debug, Copy, Clone)]
struct Geometry {{
    tiles_x: u64, tiles_y: u64, size1: u64, size2: u64, size3: u64,
    x_blocks: u64, y_blocks: u64, screen: u64, pixels: u64, macro_size: u64,
}}
#[derive(Debug, PartialEq, Eq)]
enum RenderBuildError {{ ZeroDimensions, LayerCountOutOfRange, UtileDimensions }}

impl G17pRenderParameters {{
{set_samples}
}}

{geometry}

fn params(samples: u8) -> G17pRenderParameters {{
    let mut p = G17pRenderParameters {{
        width: 256, height: 256, layers: 1, samples: 1,
        utile_width: 32, utile_height: 32,
        multisample_control: 0, utile_config: 0, tib_blocks: 0,
    }};
    assert!(p.set_samples(samples));
    p
}}

#[test]
fn witnessed_multisample_triples() {{
    let p1 = params(1);
    assert_eq!((p1.multisample_control, p1.utile_config, p1.tib_blocks),
               (0x88, 0xa000, 0));
    let p2 = params(2);
    assert_eq!((p2.multisample_control, p2.utile_config, p2.tib_blocks),
               (0x44cc, 0xa001, 0x20000));
    let p4 = params(4);
    assert_eq!((p4.multisample_control, p4.utile_config, p4.tib_blocks),
               (0xeaa26e26, 0x6002, 0x40000));
    let mut bad = params(1);
    assert!(!bad.set_samples(3));
    assert!(!bad.set_samples(8));
}}

#[test]
fn geometry_matches_the_captured_registers() {{
    // 0x1c0b1/0x1c850 = size1, 0x1c0a9 = size3, 0x16030/0x100b1/0x16469 = macro_size
    let g1 = geometry(&params(1)).unwrap();
    assert_eq!((g1.size1, g1.size3, g1.macro_size), (0x14, 0x20, 0x40004));
    let g2 = geometry(&params(2)).unwrap();
    assert_eq!((g2.size1, g2.size3, g2.macro_size), (0x14, 0x20, 0x40004),
               "two samples must NOT split the utile");
    let g4 = geometry(&params(4)).unwrap();
    assert_eq!((g4.size1, g4.size3, g4.macro_size), (0x28, 0x40, 0x40008));
}}

#[test]
fn one_sample_is_the_untouched_default() {{
    let p = G17pRenderParameters {{
        width: 256, height: 256, layers: 1, samples: 1,
        utile_width: 32, utile_height: 32,
        multisample_control: 0, utile_config: 0, tib_blocks: 0,
    }};
    let g = geometry(&p).unwrap();
    assert_eq!((g.size1, g.size3, g.macro_size), (0x14, 0x20, 0x40004));
}}
"""

with tempfile.TemporaryDirectory(prefix="m5-multisample-") as tmp:
    binary = Path(tmp) / "tests"
    subprocess.run(["rustc", "--edition=2021", "--test", "-", "-o", str(binary)],
                   input=source, text=True, check=True)
    subprocess.run([str(binary)], check=True)

print("PASS M5 multisample geometry and witnessed register triples")
