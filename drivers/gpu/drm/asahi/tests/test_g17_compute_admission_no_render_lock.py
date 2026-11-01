#!/usr/bin/env python3
"""Production compute admission/retirement must progress with render locked."""
from pathlib import Path
import subprocess,tempfile,sys
s=(Path(__file__).resolve().parents[1]/'g17_drm.rs').read_text()
def item(marker):
 a=s.index(marker);b=s.index('{',a)+1;d=1
 while d:d+=(s[b]=='{')-(s[b]=='}');b+=1
 return s[a:b]
acquire=item('    fn acquire_compute(')
if '--negative-control' in sys.argv:
 acquire=acquire.replace('        if self.compute_faulted', '        let _held = self.lease.lock();\n        if self.compute_faulted',1)
rust=r'''
#![allow(dead_code)]
use std::sync::{Arc,Mutex,MutexGuard,atomic::{AtomicU32,AtomicUsize,Ordering}};
type Result<T=()> = std::result::Result<T,i32>;const EIO:i32=5;
struct LeaseState{render_slots:u64,render_quarantined:u64}
struct Lock(Mutex<LeaseState>);impl Lock{fn lock(&self)->MutexGuard<'_,LeaseState>{self.0.lock().unwrap()}}
struct Wake(AtomicUsize);impl Wake{fn notify_all(&self){self.0.fetch_add(1,Ordering::SeqCst);}}
struct G17PQueueRegistry{lease:Lock,lease_free:Wake,compute_faulted:AtomicU32,compute_readers:AtomicU32,device_wide:bool}
impl G17PQueueRegistry{
 fn new(device_wide:bool)->Arc<Self>{Arc::new(Self{lease:Lock(Mutex::new(LeaseState{render_slots:0,render_quarantined:0})),lease_free:Wake(AtomicUsize::new(0)),compute_faulted:AtomicU32::new(0),compute_readers:AtomicU32::new(0),device_wide})}
 fn compute_poison_is_device_wide(&self)->bool{self.device_wide}
'''+acquire+r'''
}
'''+item('pub(crate) struct G17PComputeLeaseGuard {')+'\n'+item('impl G17PComputeLeaseGuard {')+'\n'+item('impl Drop for G17PComputeLeaseGuard {')+r'''
#[test]fn compute_admission_and_both_release_paths_progress_during_render_contention(){
 let r=G17PQueueRegistry::new(true);let held=r.lease.lock();
 let (tx,rx)=std::sync::mpsc::channel();let worker_r=r.clone();
 let worker=std::thread::spawn(move||{
  for _ in 0..1000{
   let mut lease=worker_r.acquire_compute().unwrap();lease.release_retired_admission();lease.release_retired_admission();drop(lease);
   drop(worker_r.acquire_compute().unwrap());
  }
  tx.send(()).unwrap();
 });
 let progressed=rx.recv_timeout(std::time::Duration::from_secs(1)).is_ok();
 drop(held);worker.join().unwrap();assert!(progressed,"compute waited for render registry mutex");
 assert_eq!(r.compute_readers.load(Ordering::SeqCst),0);assert_eq!(r.lease_free.0.load(Ordering::SeqCst),0);
}
#[test]fn parallel_compute_shares_are_counted_once_without_render_wakeups(){
 let r=G17PQueueRegistry::new(false);
 std::thread::scope(|scope|{for _ in 0..8{let r=&r;scope.spawn(move||{for _ in 0..1000{drop(r.acquire_compute().unwrap());}});}});
 assert_eq!(r.compute_readers.load(Ordering::SeqCst),0);assert_eq!(r.lease_free.0.load(Ordering::SeqCst),0);
}
#[test]fn poison_and_rehabilitation_keep_their_waiter_notification_protocol(){
 let r=G17PQueueRegistry::new(true);let lease=r.acquire_compute().unwrap();lease.quarantine();
 assert!(r.acquire_compute().is_err());assert_eq!(r.compute_readers.load(Ordering::SeqCst),1);
 lease.rehabilitate_after_processor_stop();drop(r.acquire_compute().unwrap());drop(lease);
 assert_eq!(r.compute_readers.load(Ordering::SeqCst),0);assert_eq!(r.lease_free.0.load(Ordering::SeqCst),2);
 let scoped=G17PQueueRegistry::new(false);let lease=scoped.acquire_compute().unwrap();lease.quarantine();
 drop(scoped.acquire_compute().unwrap());drop(lease);assert_eq!(scoped.compute_readers.load(Ordering::SeqCst),0);
}
'''
with tempfile.TemporaryDirectory() as d:
 p=Path(d)/'test.rs';p.write_text(rust);binary=Path(d)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(binary)],check=True)
 subprocess.run([str(binary)],check=True)
