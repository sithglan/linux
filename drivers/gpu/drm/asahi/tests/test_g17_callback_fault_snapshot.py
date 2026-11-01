#!/usr/bin/env python3
"""Check static DATA bounds and capture-before-print fault instrumentation."""
from pathlib import Path
import subprocess,tempfile
r=Path(__file__).resolve().parents[1];m=(r/'g17_manager.rs').read_text()
def item(key):
 a=m.index(key);b=m.index('{',a)+1;n=1
 while n:n+=(m[b]=='{')-(m[b]=='}');b+=1
 return m[a:b]
body=item('    fn log_render_callback_frontier(')
assert body.index('let callback:')<body.index('let ended =')<body.index('dev_err!')
assert body.count('dev_err!')==1
for name in ['let readiness','let slot_maps','let irq_masks','let worker_masks','let walk_masks','let paired_exclusions','let counter_side_effects']:
 assert body.index(name) < body.index('let ended =')
assert '0xffff_fc00_0012_f670 + index as u64 * 8' in body
assert 'slot_maps: [Result<u64>; 16]' in body
for address,width in [(0x12f668,8),(0x12f670,16*8),(0x177128,8),(0x177130,8),(0x177228,8),(0x177230,8),(0x583b8,80),(0x58158,16),(0x59938,16)]:
 assert 0x4c000 <= address and address+width <= 0x180000 and address%8 == 0
assert 'observe_mask(u64::MAX, 1)' in body
assert 'read(pointer' not in body and 'while ' not in body and 'fsleep' not in body
assert 'g17p_fault_report.value() & 4 == 0' in body
report=item('    pub(crate) fn report_render_wait_timeout(')
assert report.index('log_render_callback_frontier(')<report.index('dev_err!')
program='const G17P_DEPENDENCY_RECORD_LINK_BASE:u64=0xffff_fc00_0012_61f0;\n'+item('fn g17p_render_callback_addresses(')+r'''
#[test]fn exact_capture_identity_and_bounds(){
 assert_eq!(g17p_render_callback_addresses(13,25),Some([0xfffffc0000059c20,0xfffffc0000126fb0,0xfffffc000012f7b8,0xfffffc0000126450,0xfffffc0000125eb8,0xfffffc0000177190]));
 assert!(g17p_render_callback_addresses(128,0).is_none());assert!(g17p_render_callback_addresses(0,128).is_none());
 let widths=[56,16,8,8,8,8];
 for id in 0..128{for qid in 0..128{let a=g17p_render_callback_addresses(id,qid).unwrap();for (a,w) in a.into_iter().zip(widths){assert_eq!(a&7,0);assert!(a>=0xfffffc000004c000);assert!(a+w<=0xfffffc0000180000);}}}
}
'''
with tempfile.TemporaryDirectory() as tmp:
 p=Path(tmp)/'cb.rs';p.write_text(program);exe=Path(tmp)/'cb'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(exe)],check=True)
 subprocess.run([str(exe)],check=True)
print('PASS callback DATA snapshot: 16384 index combinations bounded/aligned, exact captured addresses, reads before logging, no pointer chase/poll')
