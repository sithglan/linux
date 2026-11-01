#!/usr/bin/env python3
"""Compile the real descriptor builder and check native MSAA sample metadata."""
from pathlib import Path
from g17_config_test_support import with_g17_config
import argparse
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--all", action="store_true", help="Run all descriptor-builder regressions")
args = parser.parse_args()
source = r'''
#![allow(dead_code, non_upper_case_globals)]
mod module_parameters {
    pub struct Param(u32);
    impl Param { pub fn value(&self) -> &u32 { &self.0 } }
    pub static g17p_ta_oracle: Param = Param(0);
    pub static g17p_frag_oracle: Param = Param(0);
    pub static g17p_render_ta_qid_field: Param = Param(1);
}
mod g17_submission {
    pub const G17P_NATIVE_RENDER_DOORBELL_PRIORITY: u32 = 0;
}
'''
with tempfile.TemporaryDirectory(prefix="g17-render-samples-") as tmp:
    test = Path(tmp) / "test.rs"
    test.write_text(with_g17_config(source + '#[path = "' + str(root / "g17_render.rs") + '"] mod g17_render;\n'))
    exe = Path(tmp) / "test"
    subprocess.run(["rustc", "-Awarnings", "--edition=2021", "--test", str(test), "-o", str(exe)], check=True)
    name = "g17_render::tests::fragment_sample_count_matches_tile_configuration_on_every_submission"
    listing = subprocess.check_output([str(exe), "--list"], text=True)
    assert name + ": test" in listing
    subprocess.run([str(exe)] if args.all else [str(exe), name, "--exact"], check=True)
