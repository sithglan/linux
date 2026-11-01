#!/usr/bin/env python3
"""Compile production halt gate and selector; verify slot flows into MMIO tags."""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / 'g17_completion.rs').read_text()

def block(text, marker):
    start = text.index(marker)
    end = text.index('{', start) + 1
    depth = 1
    while depth:
        depth += (text[end] == '{') - (text[end] == '}')
        end += 1
    return text[start:end]

unit = '\n'.join([
    '#![allow(dead_code)]',
    source[source.index('pub(crate) const G17P_DM0_MONITOR_READS'):source.index('];', source.index('pub(crate) const G17P_DM0_MONITOR_READS'))+2],
    source[source.index('pub(crate) const G17P_DM1_MONITOR_READS'):source.index('];', source.index('pub(crate) const G17P_DM1_MONITOR_READS'))+2],
    block(source, 'pub(crate) fn g17p_monitor_reads('),
    block(source, 'pub(crate) fn g17p_ta_callback_index('),
    r"""
    #[test]
    fn ta_callback_pointer_accepts_only_static_object_starts() {
        let base = 0xffff_fc00_0005_9948;
        for i in 0..128u64 {
            assert_eq!(g17p_ta_callback_index(base+i*0x38), Some(i as usize));
            for byte in 1..0x38 { assert_eq!(g17p_ta_callback_index(base+i*0x38+byte), None); }
        }
        for p in [0, base-1, base+128*0x38, u64::MAX] {
            assert_eq!(g17p_ta_callback_index(p), None);
        }
    }
    """,

    r"""
    #[test]
    fn native_engine_tables_are_bounded_and_do_not_alias_namespaces() {
        assert_eq!(g17p_monitor_reads(0).unwrap(), &[(0x1678,0),(0x1c0e0,0),(0xa088,0x50000000000)]);
        assert_eq!(g17p_monitor_reads(1).unwrap().len(), 8);
        for dm in [2,3,u32::MAX] { assert!(g17p_monitor_reads(dm).is_none()); }
        assert_eq!(g17p_dm1_monitor_selector_index(0x0201000d), Some(1));
        assert_eq!(g17p_dm1_monitor_selector_index(0x0302000d), Some(2));
        for dm in [0,1] {
            for (reg,flags) in g17p_monitor_reads(dm).unwrap() {
                assert_eq!(*reg & *flags, 0);
                assert!(g17p_monitor_reads(dm).unwrap().len() <= 8);
            }
        }
    }
    """,
    '#[derive(Clone,Copy)]',
    block(source, 'pub(crate) struct G17PHaltedSlotGate'),
    block(source, 'impl G17PHaltedSlotGate'),
    block(source, 'pub(crate) const fn g17p_slot_has_progress_selector('),
    block(source, 'pub(crate) const fn g17p_dm1_monitor_selector_index('),
    r"""
    #[test] fn secondary_monitor_requires_same_proven_halt_and_distinct_bounded_slot() {
        let gate = G17PHaltedSlotGate { handshake_state:1, epoch:7, expected_epoch:7,
            report_state:2, host_requested:0, slot_present:1, slot:7,
            data_master_present:1, data_master:0 };
        for slot in 0..32 { for state in 0..4 {
            assert_eq!(gate.allows_dm1_peer_sample(slot,state),slot<16&&slot!=7&&state==2);
        }}
        for bad in [G17PHaltedSlotGate{epoch:8,..gate},
            G17PHaltedSlotGate{handshake_state:0,..gate},
            G17PHaltedSlotGate{report_state:1,..gate},
            G17PHaltedSlotGate{host_requested:1,..gate},
            G17PHaltedSlotGate{data_master:1,..gate},
            G17PHaltedSlotGate{slot_present:0,..gate}] {
            assert!(!bad.allows_dm1_peer_sample(1,2));
        }
    }
    """,
    '#[test]', block(source, 'fn halted_slot_gate_rejects_every_wrong_state_or_identity('),
    '#[test]', block(source, 'fn slot_progress_selection_matches_b1_c020_predicate('),
])
with tempfile.TemporaryDirectory() as d:
    binary = str(Path(d) / 'halted-slot')
    subprocess.run(['rustc', '--edition=2021', '--test', '-', '-o', binary], input=unit, text=True, check=True)
    subprocess.run([binary], check=True)
regs = (root / 'regs.rs').read_text()
method = block(regs, 'pub(crate) fn g17p_halted_slot_snapshot(')
assert method.count('0xc020, u64::from(slot), 1u64 << 42') == 2
assert '0xc078, u64::from(slot), 0' in method
assert method.index('if slot >= 16') < method.index('let sgx')
assert method.index('if gpc_state == 0') < method.index('0xc020')
assert 'g17p_dm1_monitor_selector_index(state_before)' in method
live = block((root / 'g17_live_boot.rs').read_text(), 'fn log_g17p_halted_slot(')
assert 'Some(gate)' in live
assert 'DM1_WATCHDOG_SLOT_COUNT.min(16)' in live
assert 'allows_dm1_peer_sample(slot as u32, monitor.state())' in live
assert 'handshake.state != 1 || handshake.epoch != expected_epoch' in live
sample = block((root / 'g17_live_boot.rs').read_text(), 'fn log_g17p_halted_slot_sample(')
assert 'g17p_halted_slot_snapshot(slot, data_master)' in sample
assert method.index('g17p_monitor_reads(data_master)') < method.index('let sgx')
assert 'reads.iter().enumerate()' in method
print('halted slot gate, progress selector, actual selected-slot port wiring PASS')
