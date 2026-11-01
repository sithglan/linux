#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Test the actual bounded M5 resource parser and optional private reference."""
import argparse
from pathlib import Path
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--bundle', type=Path)
parser.add_argument('--native-rgba', type=Path)
args = parser.parse_args()
root = Path(__file__).resolve().parents[1]
source = ('#![allow(dead_code)]\n#[path = "' + str(root / 'm5_graphics_bundle.rs')
          + '"] mod bundle;\n')
probe = r'''
fn main() {
    for arg in std::env::args().skip(1) {
        let (kind, path) = arg.split_once('=').unwrap();
        let bytes = std::fs::read(path).unwrap();
        if kind == "bundle" {
            let parsed = bundle::GraphicsBundle::parse(&bytes).unwrap();
            let ro = (0..parsed.page_count()).filter(|i| !parsed.page(*i).unwrap().writable).count();
            println!("PRIVATE_BUNDLE_PARSED pages={} ro={} rw={}", parsed.page_count(), ro, parsed.page_count()-ro);
        } else if kind == "native-rgba" {
            assert_eq!(bytes.len(), bundle::PIXEL_BYTES);
            let mut result = bundle::TriangleResult::default();
            for (index, color) in bytes.chunks_exact(4).enumerate() {
                result.add(index, color.try_into().unwrap());
            }
            println!("NATIVE_REFERENCE_PIXEL_CHECK {:?}", result);
            assert!(result.verified());
        } else { panic!("unknown input"); }
    }
}
'''
with tempfile.TemporaryDirectory(prefix='m5-graphics-bundle-') as tmp:
    test = Path(tmp) / 'test.rs'
    test.write_text(source + probe)
    exe = Path(tmp) / 'test'
    subprocess.run(['rustc', '--edition=2021', '--test', str(test), '-o', str(exe)], check=True)
    listing = subprocess.check_output([str(exe), '--list'], text=True)
    assert 'bundle::tests::output_must_be_complete_writable_and_untouched: test' in listing
    subprocess.run([str(exe)], check=True)
    inputs = []
    if args.bundle:
        inputs.append('bundle=' + str(args.bundle.resolve()))
    if args.native_rgba:
        inputs.append('native-rgba=' + str(args.native_rgba.resolve()))
    if inputs:
        subprocess.run(['rustc', '--edition=2021', str(test), '-o', str(exe)], check=True)
        subprocess.run([str(exe), *inputs], check=True)
