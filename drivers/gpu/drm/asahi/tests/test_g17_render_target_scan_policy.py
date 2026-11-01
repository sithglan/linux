#!/usr/bin/env python3
"""Compile the real RT/TVB diagnostic predicate; check its call-site scope."""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / "g17_resources.rs").read_text()
start = source.index("fn g17p_render_target_scan_due(")
end = source.index("\n}", start) + 2
predicate = source[start:end]

start = source.index("    pub(crate) fn log_status_pages(")
end = source.index("\n    fn log_render_target_state(", start)
call_site = source[start:end]
assert "let status_read_failed = ta_read.is_err() || fragment_read.is_err();" in call_site
assert "match (ta_read, fragment_read)" in call_site
assert "G17P render status[{}]: TA" in call_site
assert "G17P render status[{}]: 3D" in call_site
assert """if g17p_render_target_scan_due(
            label,
            crate::g17_config::DEP_RECORDS != 0,
            status_read_failed,
        ) {
            self.log_render_target_state(dev, label);
        }""" in call_site
assert source.count("self.log_render_target_state(dev, label);") == 1

tests = r'''
#[test]
fn normal_success_checkpoints_skip_full_scans_only() {
    for label in ["pre-doorbell", "post-completion"] {
        assert!(!g17p_render_target_scan_due(label, false, false));
        assert!(g17p_render_target_scan_due(label, true, false));
        assert!(g17p_render_target_scan_due(label, false, true));
        assert!(g17p_render_target_scan_due(label, true, true));
    }
}

#[test]
fn failure_recovery_and_unknown_checkpoints_keep_diagnostics() {
    for label in ["wait-failed", "recovery-state-1", "timeout", "",
                  "future-failure", "pre-doorbell-failed", "post-completion-failed",
                  "PRE-DOORBELL", "post-completion "] {
        for diagnostic in [false, true] {
            for failed_read in [false, true] {
                assert!(g17p_render_target_scan_due(label, diagnostic, failed_read),
                        "must retain diagnostics for {label:?}");
            }
        }
    }
}
'''

with tempfile.TemporaryDirectory(prefix="g17-render-target-scan-") as tmp:
    binary = str(Path(tmp) / "test")
    subprocess.run(["rustc", "--edition=2021", "--test", "-", "-o", binary],
                   input=predicate + tests, text=True, check=True)
    subprocess.run([binary], check=True)
print("actual RT/TVB scan policy and status-only call-site scope PASS")
