#!/usr/bin/env python3
"""Production request/service code retains DMA and runs without another IRQ."""
from pathlib import Path
import subprocess,tempfile
r=Path(__file__).resolve().parents[1];source=(r/'g17_live_boot.rs').read_text()
def item(marker):
 a=source.index(marker);b=source.index('{',a)+1;d=1
 while d:d+=(source[b]=='{')-(source[b]=='}');b+=1
 return source[a:b]
request=item('    fn request_session_rebuild(')
compute=item('    fn request_compute_session_rebuild(')
service=item('    fn service_requested_session_rebuild(')
rebuild=item('    fn rebuild_session_or_defer(')
assert 'recover_persistent_runtime' not in request
assert source.count('return Err(self.request_session_rebuild(')==14
assert source.count('return Err(self.request_compute_session_rebuild(')==2
worker=item('    pub(crate) fn service_primary_recovery(')
assert worker.index('service_requested_session_rebuild()') < worker.index('let pdev =')
assert 'session_rebuild_requested' in item('    pub(crate) fn primary_recovery_pending(')
assert 'self.session_rebuild_requested = false' in item('    fn pause_submissions(')
assert 'self.session_rebuild_requested = false' in item('    pub(crate) fn shutdown(')
render=item('    pub(crate) fn submit_translated_render(')
assert render.index('self.install_render_completion(') < render.index('self.request_session_rebuild(')
finish=item('    pub(crate) fn finish_compute_submission(')
assert finish.index('self.compute_timestamps = Some(owned_timestamps)') < finish.index('.read_compute_completion()')
assert finish.index('.complete_compute_entries(') < finish.index('drop(self.compute_timestamps.take())')
rust=r'''
#![allow(dead_code)]
use std::sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}};
#[derive(Debug,Clone,Copy,PartialEq,Eq)]struct Error(i32);
const EAGAIN:Error=Error(-11);const ENODEV:Error=Error(-19);const EIO:Error=Error(-5);
type Result<T=()>=std::result::Result<T,Error>;
#[derive(Default)]struct Audit{queued:AtomicUsize,stops:AtomicUsize,drops:AtomicUsize,stopped:AtomicBool}
struct Mapping(Arc<Audit>);impl Drop for Mapping{fn drop(&mut self){assert!(self.0.stopped.load(Ordering::SeqCst),"DMA freed before stop");self.0.drops.fetch_add(1,Ordering::SeqCst);}}
mod mmu{pub type KernelMapping=super::Mapping;}
mod driver{pub fn queue_g17p_recovery_worker(a:std::sync::Arc<super::Audit>){a.queued.fetch_add(1,super::Ordering::SeqCst);}}
fn note_session_recovery_pending(p:&mut bool,_:Option<&()>,_:&str){*p=true;}
struct Preparations{active:usize,removed:bool}impl Preparations{fn close(&self,_:bool){}fn is_removed(&self)->bool{self.removed}fn is_drained(&self)->bool{self.active==0}fn fail(&self){}}
struct Runtime{preparations:Preparations,platform_removed:bool,accepting_submissions:bool,session_recovery_pending:bool,session_rebuild_requested:bool,pdev:Option<()>,drm:Option<Arc<Audit>>,compute_timestamps:Option<Mapping>,fail_stop:bool}
mod g17_lifecycle{
 use super::*;
 pub fn recover_persistent_runtime(r:&mut Runtime)->Result{
  let a=r.drm.as_ref().unwrap();assert!(!r.accepting_submissions);assert!(r.session_recovery_pending);
  a.stops.fetch_add(1,Ordering::SeqCst);if r.fail_stop{return Err(EIO)}
  a.stopped.store(true,Ordering::SeqCst);drop(r.compute_timestamps.take());r.session_recovery_pending=false;r.accepting_submissions=true;Ok(())
 }
}
impl Runtime{
 fn new()->(Self,Arc<Audit>){let a=Arc::new(Audit::default());(Self{preparations:Preparations{active:0,removed:false},platform_removed:false,accepting_submissions:true,session_recovery_pending:false,session_rebuild_requested:false,pdev:Some(()),drm:Some(a.clone()),compute_timestamps:None,fail_stop:false},a)}
'''+request+compute+service+rebuild+r'''
}
#[test]fn request_returns_original_error_without_waiting_or_dropping_owners(){let(mut r,a)=Runtime::new();assert_eq!(r.request_compute_session_rebuild(Error(-110),Mapping(a.clone())),Error(-110));assert!(!r.accepting_submissions);assert!(r.session_recovery_pending);assert_eq!(a.stops.load(Ordering::SeqCst),0);assert_eq!(a.drops.load(Ordering::SeqCst),0);assert_eq!(a.queued.load(Ordering::SeqCst),1);assert_eq!(r.service_requested_session_rebuild(),Ok(true));assert_eq!(a.drops.load(Ordering::SeqCst),1);assert!(r.accepting_submissions);assert_eq!(r.service_requested_session_rebuild(),Ok(false));}
#[test]fn repeated_requests_coalesce_without_an_irq(){let(mut r,a)=Runtime::new();for _ in 0..32{r.request_session_rebuild(EIO);}assert_eq!(a.queued.load(Ordering::SeqCst),1);assert_eq!(r.service_requested_session_rebuild(),Ok(true));assert_eq!(a.stops.load(Ordering::SeqCst),1);}
#[test]fn failed_stop_keeps_dma_owned_and_admission_closed(){let(mut r,a)=Runtime::new();r.fail_stop=true;r.request_compute_session_rebuild(EIO,Mapping(a.clone()));assert_eq!(r.service_requested_session_rebuild(),Err(EIO));assert!(!r.accepting_submissions);assert!(r.session_recovery_pending);assert!(r.compute_timestamps.is_some());assert_eq!(a.drops.load(Ordering::SeqCst),0);r.fail_stop=false;r.request_session_rebuild(EIO);assert_eq!(r.service_requested_session_rebuild(),Ok(true));assert_eq!(a.drops.load(Ordering::SeqCst),1);}
#[test]fn inline_rebuild_defers_without_stopping_an_active_preparer(){let(mut r,a)=Runtime::new();r.preparations.active=1;assert_eq!(r.rebuild_session_or_defer(),Err(EAGAIN));assert!(r.session_rebuild_requested);assert_eq!(a.stops.load(Ordering::SeqCst),0);r.preparations.active=0;assert_eq!(r.service_requested_session_rebuild(),Ok(true));}
#[test]fn removal_never_queues_or_restarts_firmware(){let(mut r,a)=Runtime::new();r.platform_removed=true;assert_eq!(r.request_session_rebuild(EIO),EIO);assert_eq!(a.queued.load(Ordering::SeqCst),0);r.session_rebuild_requested=true;assert_eq!(r.service_requested_session_rebuild(),Err(ENODEV));assert_eq!(a.stops.load(Ordering::SeqCst),0);assert!(!r.session_rebuild_requested);}
'''
with tempfile.TemporaryDirectory() as d:
 p=Path(d)/'test.rs';p.write_text(rust);binary=Path(d)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(binary)],check=True)
 subprocess.run([str(binary)],check=True)
print('PASS deferred rebuild requests coalesce, preserve DMA, require no new IRQ, and remain closed on stop failure')
