#!/usr/bin/env python3
"""Compile the real render builder and run its focused memory-backed policy test."""
from pathlib import Path
from g17_config_test_support import with_g17_config
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
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
with tempfile.TemporaryDirectory(prefix="g17-memory-backed-") as tmp:
    test = Path(tmp) / "test.rs"
    test.write_text(with_g17_config(source + '#[path = "' + str(root / "g17_render.rs") + '"] mod g17_render;\n'))
    exe = Path(tmp) / "test"
    subprocess.run(["rustc", "-Awarnings", "--edition=2021", "--test", str(test), "-o", str(exe)], check=True)
    # Qualified name is required for Rust's exact filter; assert a real test
    # exists before the final invocation so a rename cannot silently pass zero.
    name = "g17_render::tests::memory_backed_render_policy_is_independent_of_tib_size"
    listing = subprocess.check_output([str(exe), "--list"], text=True)
    assert name + ": test" in listing
    subprocess.run([str(exe), name, "--exact"], check=True)
