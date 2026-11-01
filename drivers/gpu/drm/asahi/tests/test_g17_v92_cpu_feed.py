#!/usr/bin/env python3
"""v92 per-cluster feed levels and P-core nudge gating, on the actual source.

Extracts `cluster_floor_khz` and `big_policy` from g17_host_path.rs verbatim
and checks them against the J700 policies (P 4044 MHz max, E 2424 MHz max),
the v84/v91 single-level behaviour (overrides 0), clamping and withdrawal.
Also checks statically that the nudge is default-off and gated on the applied
request and on a non-big CPU, and that the default mask keeps it clear.
"""
from pathlib import Path
import re, subprocess, tempfile
ROOT = Path(__file__).resolve().parents[1]
hp = (ROOT / 'g17_host_path.rs').read_text()


def item(text, needle):
    start = text.index(needle)
    op = text.index('{', start)
    depth = 1
    i = op + 1
    while depth:
        depth += (text[i] == '{') - (text[i] == '}')
        i += 1
    return text[start:i]


src = '\n'.join([item(hp, 'pub(crate) const fn cluster_floor_khz('), item(hp, 'pub(crate) fn big_policy(')])
tests = r'''
#[cfg(test)]
mod v92 {
    use super::*;
    const P: u32 = 4_044_000; const E: u32 = 2_424_000;
    #[test] fn inherits_single_level_like_v91() {
        for base in [1u32, 30, 50, 100] {
            assert_eq!(cluster_floor_khz(P, true, base, 0, 0), (P as u64 * base as u64 / 100) as u32);
            assert_eq!(cluster_floor_khz(E, false, base, 0, 0), (E as u64 * base as u64 / 100) as u32);
        }
    }
    #[test] fn per_cluster_overrides_and_withdrawal() {
        assert_eq!(cluster_floor_khz(P, true, 50, 100, 0), P);
        assert_eq!(cluster_floor_khz(E, false, 50, 100, 0), E / 2);
        assert_eq!(cluster_floor_khz(E, false, 50, 0, 100), E);
        assert_eq!(cluster_floor_khz(P, true, 50, 0, 100), P / 2);
        for (pp, pe) in [(0, 0), (100, 100), (75, 25)] {
            assert_eq!(cluster_floor_khz(P, true, 0, pp, pe), 0, "level 0 always withdraws");
            assert_eq!(cluster_floor_khz(E, false, 0, pp, pe), 0);
        }
        assert_eq!(cluster_floor_khz(P, true, 50, 250, 0), P, "clamped to 100%");
    }
    #[test] fn big_policy_is_the_highest_maximum() {
        assert_eq!(big_policy(&[P, E]), Some(0));
        assert_eq!(big_policy(&[E, P]), Some(1));
        assert_eq!(big_policy(&[E, E]), None);
        assert_eq!(big_policy(&[P]), None);
        assert_eq!(big_policy(&[]), None);
    }
}
'''
nudge = item(hp, 'pub(crate) fn nudge_feeding_thread()')
assert nudge.index('enabled(PCORE_NUDGE)') < nudge.index('set_cpus_allowed_ptr'), 'knob gate first'
assert "REQS.applied_pct.load(Ordering::Relaxed) == 0 { return; }" in nudge, 'only while the request is applied'
assert 'big & (1u64 << cpu) != 0 { return; }' in nudge, 'never from a big CPU'
assert 'if target.bits[0] == 0 { return; }' in nudge, 'never outside the user mask'
assert nudge.index('set_cpus_allowed_ptr(task, &target)') < nudge.index('set_cpus_allowed_ptr(task, &saved)')
default = re.search(r'const DEFAULT_MASK: u32 = ([^;]+);', hp).group(1)
assert 'PCORE_NUDGE' not in default, default
assert 'static FEED_PCT_P: AtomicU32 = AtomicU32::new(0);' in hp and 'static FEED_PCT_E: AtomicU32 = AtomicU32::new(0);' in hp
file_rs = (ROOT / 'file.rs').read_text()
submit = item(file_rs, '    pub(crate) fn submit(')
assert submit.index('nudge_feeding_thread()') < submit.index('.queues()'), 'nudge before any driver lock'
with tempfile.TemporaryDirectory() as d:
    f = Path(d) / 'v92.rs'
    f.write_text('#![allow(dead_code)]\n' + src + tests)
    subprocess.run(['rustc', '--edition=2021', '--test', str(f), '-o', str(Path(d) / 't')], check=True)
    subprocess.run([str(Path(d) / 't')], check=True)
print('PASS v92 static gates')
