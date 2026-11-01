#!/usr/bin/env python3
"""Check exact diagnostic allocation bounds, no reuse, and logical ownership."""
from pathlib import Path
import subprocess,tempfile
r=Path(__file__).resolve().parents[1]
s=(r/'g17_resources.rs').read_text()
def item(marker):
 a=s.index(marker); b=s.index('{',a)+1; n=1
 while n:
  n+=(s[b]=='{')-(s[b]=='}'); b+=1
 return s[a:b]
unit='''
use std::sync::{Arc,atomic::{AtomicU32,Ordering}};
type Result<T>=std::result::Result<T,i32>; const ENOSPC:i32=28;
mod g17_config { pub const RENDER_MCACHE_HWSID:u32=24; }
struct Context {render_mcache_hwsid:u32}
enum G17PRenderCommandContext {Bootstrap(()),Job(Arc<Context>)}
'''+item('fn g17p_mcache_probe_reserve(')+'\nimpl G17PRenderCommandContext {\n'+item('    fn mcache_hwsid(')+'\n}\n'+'''
#[test] fn concurrent_allocations_cover_native_range_once_and_never_reuse() {
 let next=Arc::new(AtomicU32::new(96)); let mut workers=vec![];
 for _ in 0..8 {let next=next.clone(); workers.push(std::thread::spawn(move||{
  let mut ids=vec![]; while let Some(id)=g17p_mcache_probe_reserve(&next) {ids.push(id);} ids
 }));}
 let mut ids:Vec<u8>=workers.into_iter().flat_map(|w|w.join().unwrap()).collect();ids.sort();
 assert_eq!(ids,(96..192).collect::<Vec<u8>>());
 assert_eq!(next.load(Ordering::Acquire),192);
 for _ in 0..100 {assert_eq!(g17p_mcache_probe_reserve(&next),None);}
 assert_eq!(next.load(Ordering::Acquire),192);
}
#[test] fn corrupt_or_exhausted_bounds_do_not_wrap_into_reserved_ids() {
 for start in [0,24,95,192,255,u32::MAX] {let next=AtomicU32::new(start);
  assert_eq!(g17p_mcache_probe_reserve(&next),None); assert_eq!(next.load(Ordering::Relaxed),start);
 }
}
#[test] fn identity_follows_logical_owner_and_exhaustion_fails_closed() {
 let a=Arc::new(Context{render_mcache_hwsid:96}); let b=Arc::new(Context{render_mcache_hwsid:97});
 assert_eq!(G17PRenderCommandContext::Bootstrap(()).mcache_hwsid(),Ok(24));
 let pair0=G17PRenderCommandContext::Job(a.clone());let pair1=G17PRenderCommandContext::Job(a);
 assert_eq!(pair0.mcache_hwsid(),pair1.mcache_hwsid());
 assert_eq!(G17PRenderCommandContext::Job(b).mcache_hwsid(),Ok(97));
 assert_eq!(G17PRenderCommandContext::Job(Arc::new(Context{render_mcache_hwsid:0})).mcache_hwsid(),Err(ENOSPC));
}
'''
with tempfile.TemporaryDirectory() as d:
 exe=str(Path(d)/'mcache-owner'); subprocess.run(['rustc','--edition=2021','--test','-','-o',exe],input=unit,text=True,check=True); subprocess.run([exe],check=True)
assert 'let mcache_hwsid = command_context.mcache_hwsid()?;' in s
assert 'let mcache_hwsid = self._command_context.as_ref().ok_or(EIO)?.mcache_hwsid()?;' in s
assert s.count('                mcache_hwsid,\n                fragment,')==2
print('native-range non-reusing diagnostic allocator and cold/retained owner wiring PASS')
