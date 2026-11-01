#!/usr/bin/env python3
"""Compile actual HWData serialization; verify live pointers cannot be fixture bytes."""
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
    pub static g17p_fw_log: Param = Param(1);
    pub static g17p_progress_watchdog: Param = Param(1);
    pub static g17p_idle_selector: Param = Param(0);
}
'''
with tempfile.TemporaryDirectory(prefix="g17-hwdata-live-") as tmp:
    test = Path(tmp) / "test.rs"
    test.write_text(with_g17_config(source + '#[path = "' + str(root / "g17_initdata.rs") + '"] mod g17_initdata;\n'))
    exe = Path(tmp) / "test"
    subprocess.run(["rustc", "-Awarnings", "--edition=2021", "--test", str(test), "-o", str(exe)], check=True)
    listing = subprocess.check_output([str(exe), "--list"], text=True)
    for method in ("hw_data_live_pointers_survive_opaque_constants",
                   "hw_data_opaque_runs_do_not_overlap_live_fields",
                   "hw_data_size_acceptance_completion_table_and_firmware_state",
                   "recorded_run_tables_match_the_serializer_totals"):
        name = "g17_initdata::tests::" + method
        assert name + ": test" in listing
        subprocess.run([str(exe), name, "--exact"], check=True)
