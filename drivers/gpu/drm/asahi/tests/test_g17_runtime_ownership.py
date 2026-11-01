#!/usr/bin/env python3
"""Actual runtime handle/shutdown code: owned callbacks and explicit cycle break."""
from pathlib import Path
import subprocess,tempfile
r=Path(__file__).resolve().parents[1]
drm=(r/'g17_drm.rs').read_text();live=(r/'g17_live_boot.rs').read_text();driver=(r/'driver.rs').read_text()
def item(s,marker):
 a=s.index(marker);b=s.index('{',a)+1;d=1
 while d:d+=(s[b]=='{')-(s[b]=='}');b+=1
 return s[a:b]
handle=item(drm,'pub(crate) struct G17PRuntimeRef {')
new=item(drm,'    pub(crate) fn new(runtime: &SharedG17PRuntime)')
mutex=item(drm,'    pub(crate) fn mutex(&self)')
shutdown=item(live,'    pub(crate) fn shutdown(')
drop=item(live,'impl Drop for G17PLiveRuntime {')
recover=item(live,'    pub(crate) fn recover_crashed_session_once(')
rust=r'''
#![allow(dead_code,unused_variables,dropping_copy_types)]
macro_rules! dev_info {($($t:tt)*)=>{()};}
macro_rules! dev_err {($($t:tt)*)=>{()};}
mod g17_config {pub const ABANDON_RECOVERY:u32=1;}
mod g17_preparation {pub struct G17PPreparations;impl G17PPreparations{pub fn is_removed(&self)->bool{false}}}
use std::sync::{Arc,Mutex as StdMutex,atomic::{AtomicUsize,Ordering}};
struct Mutex<T>(StdMutex<T>);
impl<T> Mutex<T>{fn lock(&self)->std::sync::MutexGuard<'_,T>{self.0.lock().unwrap()}}
#[derive(Default)]struct Audit{stops:AtomicUsize,graphs:AtomicUsize,runtime_drops:AtomicUsize}
struct Graph(Arc<Audit>);
impl Drop for Graph{fn drop(&mut self){assert_eq!(self.0.stops.load(Ordering::SeqCst),1);self.0.graphs.fetch_add(1,Ordering::SeqCst);}}
struct Device{handle:G17PRuntimeRef}
mod g17_live_boot {
 use super::*;
 pub struct G17PLiveRuntime{pub preparations:Arc<g17_preparation::G17PPreparations>,pub platform_removed:bool,pub session_rebuild_requested:bool,pub accepting_submissions:bool,pub crash_recoveries:u32,pub drm:Option<Arc<Device>>,pub registers:Option<()>,pub pdev:Option<()>,pub graph:Option<Graph>,pub audit:Arc<Audit>}
 impl G17PLiveRuntime{
  pub fn new(audit:Arc<Audit>)->Self{Self{preparations:Arc::new(g17_preparation::G17PPreparations),platform_removed:false,session_rebuild_requested:false,accepting_submissions:true,crash_recoveries:0,drm:None,registers:Some(()),pdev:Some(()),graph:Some(Graph(audit.clone())),audit}}
  fn disarm_firmware_event_worker(&mut self){self.accepting_submissions=false;}
'''+shutdown+recover+r'''
  pub fn preparation_control(&self)->Arc<g17_preparation::G17PPreparations>{self.preparations.clone()}
  fn rebuild_session_or_defer(&mut self)->Result<(),()>{panic!("removed or queued runtime attempted inline rebuild")}
  fn is_crashed(&self)->bool{self.platform_removed}
 }
'''+drop.replace('self.shutdown();','self.shutdown(); self.audit.runtime_drops.fetch_add(1,Ordering::SeqCst);')+r'''
}
mod g17_lifecycle{
 pub fn recover_persistent_runtime(_: &mut super::g17_live_boot::G17PLiveRuntime)->Result<(),()>{panic!("removed runtime attempted recovery")}
 pub fn teardown_persistent_runtime(r:&mut super::g17_live_boot::G17PLiveRuntime){
  assert!(r.platform_removed);assert!(!r.accepting_submissions);
  r.audit.stops.fetch_add(1,super::Ordering::SeqCst);drop(r.graph.take());
 }
}
type SharedG17PRuntime=Arc<Mutex<g17_live_boot::G17PLiveRuntime>>;
#[derive(Clone)]
'''+handle+'\nimpl G17PRuntimeRef {\n'+new+mutex+'}\n'+r'''
#[test]fn callback_handle_pins_allocation_after_platform_reference_drops(){
 let audit=Arc::new(Audit::default());let platform=Arc::new(Mutex(StdMutex::new(g17_live_boot::G17PLiveRuntime::new(audit.clone()))));
 let weak=Arc::downgrade(&platform);let callback=G17PRuntimeRef::new(&platform);drop(platform);
 assert!(weak.upgrade().is_some());assert!(callback.mutex().lock().accepting_submissions);
 drop(callback);assert!(weak.upgrade().is_none());assert_eq!(audit.stops.load(Ordering::SeqCst),1);
}
#[test]fn queued_rebuild_is_unavailable_without_inline_retry(){
 let audit=Arc::new(Audit::default());
 let mut runtime=g17_live_boot::G17PLiveRuntime::new(audit.clone());
 runtime.session_rebuild_requested=true;
 for _ in 0..32 {assert!(runtime.recover_crashed_session_once());}
 assert_eq!(runtime.crash_recoveries,0);assert_eq!(audit.stops.load(Ordering::SeqCst),0);
}
#[test]fn explicit_shutdown_breaks_device_back_reference_and_closes_existing_handles(){
 let audit=Arc::new(Audit::default());let platform=Arc::new(Mutex(StdMutex::new(g17_live_boot::G17PLiveRuntime::new(audit.clone()))));
 let weak=Arc::downgrade(&platform);let callback=G17PRuntimeRef::new(&platform);
 let device=Arc::new(Device{handle:callback.clone()});platform.lock().drm=Some(device.clone());drop(device);
 platform.lock().shutdown();platform.lock().shutdown();
 assert!(!callback.mutex().lock().accepting_submissions);assert!(callback.mutex().lock().drm.is_none());
 assert!(callback.mutex().lock().recover_crashed_session_once());
 assert_eq!(callback.mutex().lock().crash_recoveries,0);
 assert_eq!(audit.stops.load(Ordering::SeqCst),1);assert_eq!(audit.graphs.load(Ordering::SeqCst),1);
 drop(platform);assert!(weak.upgrade().is_some());drop(callback);assert!(weak.upgrade().is_none());
 assert_eq!(audit.runtime_drops.load(Ordering::SeqCst),1);assert_eq!(audit.stops.load(Ordering::SeqCst),1);
}
'''
with tempfile.TemporaryDirectory() as d:
 p=Path(d)/'test.rs';p.write_text(rust);binary=Path(d)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(binary)],check=True)
 subprocess.run([str(binary)],check=True)
assert '*const' not in handle and 'runtime.clone()' in new
unbind=item(driver,'    fn unbind(_pdev:')
assert unbind.index('quiesce_driver_unbind(') < unbind.index('runtime.lock().shutdown()')
probe=item(driver,'fn probe_t8140_runtime(')
setup=probe.index('let registration = (|| -> Result')
assert setup < probe.index('G17PRuntimeRef::new') < probe.index('data_initialized = true')
assert probe.index('data_initialized = true') < probe.index('arm_primary_firmware_event_worker')
error=probe[probe.index('if let Err(error) = registration'):]
assert error.index('quiesce_driver_unbind(') < error.index('runtime.lock().shutdown()') < error.index('return Err(error)')
recovery=item(live,'    pub(crate) fn recover_crashed_session_once(')
assert recovery.index('if self.platform_removed || self.preparations.is_removed() { return true; }') < recovery.index('rebuild_session_or_defer(')
print('PASS owned callback handles; explicit shutdown and probe-error cleanup precede final allocation release')
