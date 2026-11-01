#!/usr/bin/env python3
"""Exercise the production read-only explicit-pair snapshot against a mock slot store."""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / 'g17_manager.rs').read_text()
start = source.index('    pub(crate) fn report_queued_render_failure(')
end = source.index('\n    }', start) + 6
method = source[start:end]
mock = r'''
type Result<T=()> = std::result::Result<T,i32>;
struct RenderSlots;
impl RenderSlots { fn allocated(&self,s:u8)->Result { if s<4 {Ok(())}else{Err(22)} } }
struct Manager { render_slots:RenderSlots, seen:Vec<(u8,u8)> }
impl Manager {
 fn report_render_wait_timeout(&mut self,slot:u8,_:&str){self.seen.push((slot,1));}
 fn log_selected_render_recovery_pre_ack(&mut self,slot:u8,_:&str,p:Option<u8>){
  assert_eq!(p,None);self.seen.push((slot,2));
 }
METHOD
}
#[test] fn reports_exact_target_without_selecting_shared_state(){
 let mut m=Manager{render_slots:RenderSlots,seen:vec![]};
 assert_eq!(m.report_queued_render_failure(2),Ok(()));
 assert_eq!(m.seen,[(2,1),(2,2)]);
 assert_eq!(m.report_queued_render_failure(1),Ok(()));
 assert_eq!(m.seen,[(2,1),(2,2),(1,1),(1,2)]);
}
#[test] fn invalid_target_does_not_report_another_owner(){
 let mut m=Manager{render_slots:RenderSlots,seen:vec![]};
 assert_eq!(m.report_queued_render_failure(255),Err(22));
 assert!(m.seen.is_empty());
}
'''.replace('METHOD', method)
with tempfile.TemporaryDirectory() as directory:
    exe = str(Path(directory) / 'test')
    subprocess.run(['rustc', '--edition=2021', '--test', '-', '-o', exe],
                   input=mock, text=True, check=True)
    subprocess.run([exe], check=True)
