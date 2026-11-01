#!/usr/bin/env python3
"""Execute the production backing-retirement block with drop-counted allocations."""
from pathlib import Path
import subprocess, tempfile
root = Path(__file__).resolve().parents[1]
src = (root / 'g17_resources.rs').read_text()
a = src.index('        let rt_slot = &mut self.render_target_memory.rt_slot_ordinals[')
b = src.index('        Ok(())', a)
body = src[a:b].replace('drop(retired.remove(index)?);', 'drop(retired.remove(index));')
guard_start = src.index('        let slot = submission_ordinal as usize % G17P_RENDER_RT_EVENT_RECORD_COUNT;', src.index('impl G17PRenderTargetMemory {'))
guard_end = src.index('        let used =', guard_start)
guard = src[guard_start:guard_end].replace('self.rt_slot_ordinals', 'self.render_target_memory.rt_slot_ordinals')
program = r'''
use std::rc::Rc;
use std::cell::Cell;
const G17P_RENDER_RT_EVENT_RECORD_COUNT: usize = 16;
struct Backing(Rc<Cell<usize>>);
impl Drop for Backing { fn drop(&mut self) { self.0.set(self.0.get()+1); } }
struct Retired { _backing: Backing, owners: [Option<u64>;16] }
struct Target { rt_slot_ordinals: [Option<u64>;16], retired_backings: Vec<Retired> }
struct Storage { render_target_memory: Target }
impl Storage { fn retire(&mut self, submission_ordinal: u64) {
''' + body + r'''
}
fn can_reuse(&self, submission_ordinal:u64) -> Result<(), i32> {
const EBUSY:i32=16;
''' + guard + r'''
Ok(())
} }
fn main() {
    let first=Rc::new(Cell::new(0)); let second=Rc::new(Cell::new(0));
    let mut a=[None;16]; a[0]=Some(0); a[1]=Some(1);
    let mut b=[None;16]; b[1]=Some(1); b[2]=Some(2);
    let mut current=[None;16]; current[0]=Some(16); current[2]=Some(2);
    let mut s=Storage { render_target_memory: Target { rt_slot_ordinals:current,
      retired_backings:vec![Retired{_backing:Backing(first.clone()),owners:a},
                           Retired{_backing:Backing(second.clone()),owners:b}] } };
    assert_eq!(s.can_reuse(16),Ok(()));
    assert_eq!(s.can_reuse(32),Err(16)); // full ring wrap still owned
    assert_eq!(s.can_reuse(48),Err(16)); // multiple wraps cannot free it
    assert_eq!(s.can_reuse(0),Err(16)); // stale ordinal cannot replace it
    s.retire(99); // unrelated completion must release nothing
    s.retire(1); // shared owner retires out of order; both allocations stay live
    assert_eq!((first.get(),second.get()),(0,0));
    s.retire(0); // old generation must not erase the current ring owner 16
    assert_eq!(s.render_target_memory.rt_slot_ordinals[0],Some(16));
    assert_eq!((first.get(),second.get()),(1,0));
    s.retire(0); // duplicate completion cannot release the other allocation
    assert_eq!((first.get(),second.get()),(1,0));
    s.retire(2);
    assert_eq!((first.get(),second.get()),(1,1));
    assert!(s.render_target_memory.retired_backings.is_empty());
    assert_eq!(s.render_target_memory.rt_slot_ordinals[2],None);
    s.retire(16);
    assert_eq!(s.render_target_memory.rt_slot_ordinals[0],None);
    assert_eq!(s.can_reuse(32),Ok(()));
}
'''
with tempfile.TemporaryDirectory() as d:
    p=Path(d); (p/'test.rs').write_text(program)
    subprocess.run(['rustc','--edition=2021',str(p/'test.rs'),'-o',str(p/'test')],check=True)
    subprocess.run([str(p/'test')],check=True)
print('render growth retirement: exact owners, multiple growths, out-of-order and stale completions PASS')
