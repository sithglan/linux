#!/usr/bin/env python3
"""Compile the real g17_initdata.rs and run the G18G (J873, 26A428) layout tests.

The tests live in g18_initdata_test.rs, which g17_initdata.rs includes as
`g18_tests` under #[cfg(test)]. They prove two things offline:

  * regression: the layout functions over the 25G76 allocation-size table
    reproduce every literal M5 constant, and the J700/J813 encoders stay
    bit-identical (golden FNV-1a hashes of every encoder output);
  * G18G: every hw_data/region-C offset the 26A428 b000/b010 copiers read lands
    on a field the encoder writes, and no unrelocated J813 value is left behind.

The whole module (the pre-existing g17_initdata tests included) is then run so
a G18G change can never silently break the M5 path.
"""
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

with tempfile.TemporaryDirectory(prefix="g18-initdata-") as tmp:
    test = Path(tmp) / "test.rs"
    test.write_text(with_g17_config(
        source + '#[path = "' + str(root / "g17_initdata.rs") + '"] mod g17_initdata;\n'))
    exe = Path(tmp) / "test"
    subprocess.run(["rustc", "-Awarnings", "--edition=2021", "--test", str(test), "-o", str(exe)],
                   check=True)
    # Run everything in the module: the new G18G tests and the existing M5/J700 ones.
    subprocess.run([str(exe), "-q"], check=True)
    print("G18_INITDATA_OK")
